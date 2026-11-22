//! tantivy 索引生命周期与查询。
//!
//! 设计要点：
//! - 一个 [`SearchIndex`] 持有 `Index` + `IndexReader` + `IndexWriter`；
//!   `upsert/delete` 只改 writer，显式 [`SearchIndex::commit`] 后才对查询可见
//!   （与 storage 的提交边界对齐，避免"索引可见但数据库未提交"）。
//! - `note_id` 用 raw STRING 字段做 term 删除；同名 upsert = 先删旧、再加新。
//! - 版本字段只 stored，不建索引；查询返回 `Hit` 不带版本（对账由上层读索引清单）。
//! - snippet 按笔记分组为多条命中行；每行区间绑定该行 `SnippetLine::text`。

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cang_jie::jieba_rs::Jieba;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, SchemaBuilder, TextFieldIndexing, TextOptions, Value, STORED,
    STRING,
};
use tantivy::tokenizer::NgramTokenizer;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::analyzer::{self, CS_TOKENIZER_NAME, TOKENIZER_NAME};
use crate::error::SearchError;
use crate::fingerprint::{Fingerprint, FINGERPRINT_FILE};

/// trigram 字段使用的 tokenizer 名（`NgramTokenizer(2,3)`）。
pub const GRAM_TOKENIZER_NAME: &str = "search_gram_2_3";
/// 索引内存预算（byte）。v1 与探针一致取 50 MB；数据量上来后再调。
const WRITER_MEMORY_BUDGET: usize = 50_000_000;
/// 单行摘要过长时，以命中点为中心保留的最大内容字节数（不含省略号）。
pub const SNIPPET_WINDOW_BYTES: usize = 160;

/// 查询开关。
///
/// 默认保持旧行为：大小写不敏感、整词匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryOptions {
    /// `true` 时走原样大小写字段（`title_cs`/`body_cs` 与 cs trigram）。
    pub case_sensitive: bool,
    /// `true` 精确 token（旧行为）；`false` 子串（trigram 召回 + 原文 `contains` 校验）。
    pub whole_word: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            whole_word: true,
        }
    }
}

/// 查询结果。
///
/// `total` 是**索引侧**命中数：
/// - 整词查询 = Tantivy `Count` 的结果；
/// - 子串查询 = trigram 召回后回原文校验通过的数量；
/// - 两者都在 SQL 墓碑终检之前统计。`hits` 已经过终检，所以墓碑未及时清理时
///   `total` 可能大于 `hits.len()`；口径在此写清。
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// 索引侧符合查询的文档数（见结构体注释）。
    pub total: usize,
    /// 当前页命中（已墓碑终检，按 BM25 降序）。
    pub hits: Vec<Hit>,
}

/// 写入索引的一篇笔记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDoc {
    /// 稳定笔记 ID（来自结构投影）。
    pub note_id: String,
    /// 标题（纯文本；不经过 Markdown 转换）。
    pub title: String,
    /// 正文纯文本；调用方应先用 [`crate::markdown_to_plain`] 处理 Markdown 源文。
    pub body_text: String,
    /// 版本水位，保存进索引供后续对账。
    pub version: DocVersion,
}

/// 正文/元数据版本水位。
///
/// `meta_hash` 由调用方按自己的 meta 内容算稳定哈希；search 只保存、不解释。
/// `body_frontier_hash` 用 [`Self::hash_frontier`]（稳定 FNV-1a 64）计算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocVersion {
    /// 正文文档 ID（Loro doc id）。
    pub body_doc_id: String,
    /// 已物化到的 update 序号。
    pub body_seq: u64,
    /// 物化 frontier 的稳定哈希；`None` 与空 frontier 得到同一个稳定值。
    pub body_frontier_hash: u64,
    /// 结构投影水位。
    pub meta_seq: u64,
    /// 调用方对 meta 内容算出的稳定哈希。
    pub meta_hash: u64,
}

impl DocVersion {
    /// 用原始 frontier 构造；内部算 [`Self::hash_frontier`]。
    pub fn new(
        body_doc_id: impl Into<String>,
        body_seq: u64,
        body_frontier: Option<&[u8]>,
        meta_seq: u64,
        meta_hash: u64,
    ) -> Self {
        Self {
            body_doc_id: body_doc_id.into(),
            body_seq,
            body_frontier_hash: Self::hash_frontier(body_frontier),
            meta_seq,
            meta_hash,
        }
    }

    /// 稳定 frontier 哈希（FNV-1a 64）。
    ///
    /// 必须是稳定算法：同一 frontier 在不同进程/机器上要得到同一值，
    /// 否则对账会误判为版本变化。不要换成 `DefaultHasher`（不保证跨版本稳定）。
    pub fn hash_frontier(frontier: Option<&[u8]>) -> u64 {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET_BASIS;
        if let Some(bytes) = frontier {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(PRIME);
            }
        }
        hash
    }
}

/// 一条命中行（一篇笔记可有多条，行数不设上限，由 UI 折叠展示）。
///
/// `text` 是命中所在整行（不含 `\n`）；行超过 [`SNIPPET_WINDOW_BYTES`] 时
/// 以该行首个命中点为中心裁剪，并在两端加 `…`。`ranges` 绑定 `text` 的
/// UTF-8 字节区间，覆盖该行内所有可见命中（合并重叠）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetLine {
    /// 单行文本，不含换行。
    pub text: String,
    /// 高亮区间，UTF-8 字节偏移，绑定 `Self::text`。
    pub ranges: Vec<Range<usize>>,
    /// 原正文中的 0-based 行号（标题摘要固定为 0）。
    pub line_index: usize,
}

/// 一条检索命中（一篇笔记一条 Hit）。
///
/// `snippets` 返回**全部**命中行（按行号排序、同一行只出现一次），不设行数
/// 上限；长列表由 UI 折叠。`matched_lines` 与 `snippets.len()` 相同（保留字段
/// 供 UI 直接显示"命中 N 行"）。正文无命中时退回标题摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// 笔记 ID。
    pub note_id: String,
    /// 标题。
    pub title: String,
    /// 全部命中行摘要（不设行数上限，由 UI 折叠）。
    pub snippets: Vec<SnippetLine>,
    /// 命中行总数；与 `snippets.len()` 相同（去重同行后）。
    pub matched_lines: usize,
    /// BM25 分数。
    pub score: f32,
}

/// 索引里一条文档的版本（启动对账/增量对账用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDocVersion {
    /// 笔记 ID。
    pub note_id: String,
    /// 索引里保存的版本水位。
    pub version: DocVersion,
}

/// 一个打开/创建的索引。
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    fields: Fields,
    jieba: Arc<Jieba>,
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    note_id: Field,
    title: Field,
    body: Field,
    title_cs: Field,
    body_cs: Field,
    title_gram: Field,
    body_gram: Field,
    title_gram_cs: Field,
    body_gram_cs: Field,
    body_doc_id: Field,
    body_seq: Field,
    body_frontier_hash: Field,
    meta_seq: Field,
    meta_hash: Field,
}

impl SearchIndex {
    /// 创建新索引目录。
    ///
    /// `dir` 必须是不存在或空目录；`schema` 是索引格式 fingerprint
    /// （不是 tantivy 的 `Schema`——字段布局由本 crate 固定，调用方只决定版本）。
    pub fn create(dir: impl AsRef<Path>, schema: &Fingerprint) -> Result<Self, SearchError> {
        let dir = dir.as_ref();
        if schema.tokenizer != TOKENIZER_NAME {
            return Err(SearchError::Fingerprint(format!(
                "fingerprint tokenizer={:?}，应为 {TOKENIZER_NAME:?}",
                schema.tokenizer
            )));
        }
        let expected = Fingerprint::current();
        if schema != &expected {
            return Err(SearchError::Fingerprint(format!(
                "create 只能写当前版本的索引: given={schema:?}, expected={expected:?}"
            )));
        }
        fs::create_dir_all(dir)?;
        let index = Index::create_in_dir(dir, build_schema())?;
        fs::write(dir.join(FINGERPRINT_FILE), schema.encode())?;
        Self::open_index(index)
    }

    /// 打开已有索引；fingerprint 必须与当前代码一致，否则拒绝（由上层重建）。
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, SearchError> {
        let dir = dir.as_ref();
        let found = read_fingerprint(dir)?;
        let expected = Fingerprint::current();
        if found != expected {
            return Err(SearchError::Fingerprint(format!(
                "索引 fingerprint 不匹配（需重建）: found={found:?}, expected={expected:?}"
            )));
        }
        let index = Index::open_in_dir(dir)?;
        Self::open_index(index)
    }

    fn open_index(index: Index) -> Result<Self, SearchError> {
        let fields = Fields::from_schema(&index.schema())?;
        let jieba = Arc::new(Jieba::new());
        // 索引与查询共用同一 analyzer 定义；QueryParser 不用，查询由本 crate 显式构建。
        index
            .tokenizers()
            .register(TOKENIZER_NAME, analyzer::build(jieba.clone()));
        index.tokenizers().register(
            CS_TOKENIZER_NAME,
            analyzer::build_case_sensitive(jieba.clone()),
        );
        let gram = NgramTokenizer::new(2, 3, false)?;
        index.tokenizers().register(GRAM_TOKENIZER_NAME, gram);
        let reader = index.reader()?;
        let writer = index.writer(WRITER_MEMORY_BUDGET)?;
        Ok(Self {
            index,
            reader,
            writer,
            fields,
            jieba,
        })
    }

    /// 新增或替换一篇笔记（不自动 commit）。
    ///
    /// 先按 `note_id` 删除旧 doc，再添加新 doc；同一 commit 内旧 doc 的
    /// opstamp 小于 delete，新 doc 大于 delete，因此不会被误删。
    pub fn upsert(&mut self, doc: SearchDoc) -> Result<(), SearchError> {
        let term = Term::from_field_text(self.fields.note_id, &doc.note_id);
        self.writer.delete_term(term);

        let mut document = TantivyDocument::new();
        document.add_text(self.fields.note_id, &doc.note_id);
        document.add_text(self.fields.title, &doc.title);
        document.add_text(self.fields.body, &doc.body_text);
        document.add_text(self.fields.title_cs, &doc.title);
        document.add_text(self.fields.body_cs, &doc.body_text);
        document.add_text(self.fields.title_gram, doc.title.to_lowercase());
        document.add_text(self.fields.body_gram, doc.body_text.to_lowercase());
        document.add_text(self.fields.title_gram_cs, &doc.title);
        document.add_text(self.fields.body_gram_cs, &doc.body_text);
        document.add_text(self.fields.body_doc_id, &doc.version.body_doc_id);
        document.add_u64(self.fields.body_seq, doc.version.body_seq);
        document.add_u64(
            self.fields.body_frontier_hash,
            doc.version.body_frontier_hash,
        );
        document.add_u64(self.fields.meta_seq, doc.version.meta_seq);
        document.add_u64(self.fields.meta_hash, doc.version.meta_hash);
        self.writer.add_document(document)?;
        Ok(())
    }

    /// 删除一篇笔记（墓碑；不自动 commit）。
    pub fn delete(&mut self, note_id: &str) -> Result<(), SearchError> {
        let term = Term::from_field_text(self.fields.note_id, note_id);
        self.writer.delete_term(term);
        Ok(())
    }

    /// 提交 writer 并刷新 reader，使查询立即可见。
    pub fn commit(&mut self) -> Result<(), SearchError> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// 当前索引里的文档总数。
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// 枚举索引里所有文档的 `DocVersion`（启动对账用）。
    ///
    /// 用 `AllQuery` + stored 字段；v1 一次性取回，后续数据量上来若需要
    /// 真正的流式扫描，可在这里改成分页/分段。
    pub fn all_doc_versions(&self) -> Result<Vec<IndexedDocVersion>, SearchError> {
        let searcher = self.reader.searcher();
        let num_docs = searcher.num_docs();
        if num_docs == 0 {
            return Ok(Vec::new());
        }
        let limit = usize::try_from(num_docs).unwrap_or(usize::MAX);
        let top_docs = searcher.search(&AllQuery, &TopDocs::with_limit(limit).order_by_score())?;
        let mut out = Vec::with_capacity(top_docs.len());
        for (_score, address) in top_docs {
            let document: TantivyDocument = searcher.doc(address)?;
            out.push(IndexedDocVersion {
                note_id: first_text(&document, self.fields.note_id).to_owned(),
                version: self.version_from_document(&document),
            });
        }
        Ok(out)
    }

    /// 读取单篇文档的版本；不存在返回 `None`。
    pub fn doc_version(&self, note_id: &str) -> Result<Option<DocVersion>, SearchError> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.note_id, note_id),
            IndexRecordOption::Basic,
        );
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_score, address)) = top_docs.into_iter().next() else {
            return Ok(None);
        };
        let document: TantivyDocument = searcher.doc(address)?;
        Ok(Some(self.version_from_document(&document)))
    }

    fn version_from_document(&self, document: &TantivyDocument) -> DocVersion {
        DocVersion {
            body_doc_id: first_text(document, self.fields.body_doc_id).to_owned(),
            body_seq: first_u64(document, self.fields.body_seq),
            body_frontier_hash: first_u64(document, self.fields.body_frontier_hash),
            meta_seq: first_u64(document, self.fields.meta_seq),
            meta_hash: first_u64(document, self.fields.meta_hash),
        }
    }

    /// 查询；默认 [`QueryOptions`]（大小写不敏感、整词）。
    ///
    /// 语义（整词模式，旧行为）：
    /// - 引号包住的片段 = 短语（字段内相邻）；
    /// - 空格分隔的片段 = AND；
    /// - 无空格中文串 = 其 analyzer token 的 AND（**不是**隐式短语）。
    ///
    /// 子串模式见 [`QueryOptions::whole_word`]；每条返回命中都有单行 snippet。
    pub fn query(&self, q: &str, limit: usize) -> Result<Vec<Hit>, SearchError> {
        Ok(self
            .query_with_options(q, limit, QueryOptions::default())?
            .hits)
    }

    /// 带开关的查询；`total` 是索引侧命中数（详见 [`QueryResult`]）。
    pub fn query_with_options(
        &self,
        q: &str,
        limit: usize,
        options: QueryOptions,
    ) -> Result<QueryResult, SearchError> {
        let mut result = self.query_candidates(q, limit, options)?;
        self.attach_snippets(q, options, &mut result.hits)?;
        Ok(result)
    }

    /// 只取候选（不生成 snippet）；给 `IndexManager` 在墓碑终检后再补。
    pub(crate) fn query_candidates(
        &self,
        q: &str,
        limit: usize,
        options: QueryOptions,
    ) -> Result<QueryResult, SearchError> {
        let clauses = clauses_for(q);
        if clauses.is_empty() {
            return Ok(QueryResult {
                total: 0,
                hits: Vec::new(),
            });
        }
        let searcher = self.reader.searcher();

        if options.whole_word {
            let Some(query) = self.build_token_query(&clauses, options) else {
                return Ok(QueryResult {
                    total: 0,
                    hits: Vec::new(),
                });
            };
            let total = searcher.search(&*query, &Count)?;
            if limit == 0 || total == 0 {
                return Ok(QueryResult {
                    total,
                    hits: Vec::new(),
                });
            }
            let top_docs =
                searcher.search(&*query, &TopDocs::with_limit(limit).order_by_score())?;
            let mut hits = Vec::with_capacity(top_docs.len());
            for (score, address) in top_docs {
                let document: TantivyDocument = searcher.doc(address)?;
                hits.push(Hit {
                    note_id: first_text(&document, self.fields.note_id).to_owned(),
                    title: first_text(&document, self.fields.title).to_owned(),
                    snippets: Vec::new(),
                    matched_lines: 0,
                    score,
                });
            }
            return Ok(QueryResult { total, hits });
        }

        // 子串：trigram AND 召回 → 回原文 contains 校验；total 用校验后的数量。
        let Some(query) = self.build_gram_query(&clauses, options) else {
            return Ok(QueryResult {
                total: 0,
                hits: Vec::new(),
            });
        };
        let candidate_total = searcher.search(&*query, &Count)?;
        if candidate_total == 0 {
            return Ok(QueryResult {
                total: 0,
                hits: Vec::new(),
            });
        }
        let top_docs = searcher.search(
            &*query,
            &TopDocs::with_limit(candidate_total).order_by_score(),
        )?;
        let mut total = 0usize;
        let mut hits = Vec::with_capacity(limit.min(candidate_total));
        for (score, address) in top_docs {
            let document: TantivyDocument = searcher.doc(address)?;
            let title = first_text(&document, self.fields.title);
            let body = first_text(&document, self.fields.body);
            if !verify_clauses(title, body, &clauses, options.case_sensitive) {
                continue;
            }
            total += 1;
            if hits.len() < limit {
                hits.push(Hit {
                    note_id: first_text(&document, self.fields.note_id).to_owned(),
                    title: title.to_owned(),
                    snippets: Vec::new(),
                    matched_lines: 0,
                    score,
                });
            }
        }
        Ok(QueryResult { total, hits })
    }

    /// 给最终返回的每条命中补单行 snippet。
    pub(crate) fn attach_snippets(
        &self,
        q: &str,
        options: QueryOptions,
        hits: &mut [Hit],
    ) -> Result<(), SearchError> {
        if hits.is_empty() {
            return Ok(());
        }
        let tokens = self.query_highlight_tokens(q, options);
        if tokens.is_empty() {
            return Ok(());
        }
        let searcher = self.reader.searcher();
        for hit in hits {
            let note_query = TermQuery::new(
                Term::from_field_text(self.fields.note_id, &hit.note_id),
                IndexRecordOption::Basic,
            );
            let top_docs =
                searcher.search(&note_query, &TopDocs::with_limit(1).order_by_score())?;
            let Some((_score, address)) = top_docs.into_iter().next() else {
                continue;
            };
            let document: TantivyDocument = searcher.doc(address)?;
            let body_text = first_text(&document, self.fields.body);
            let title_text = first_text(&document, self.fields.title);
            let (mut snippets, mut matched_lines) = fast_snippets_with_case(
                body_text,
                &tokens,
                SNIPPET_WINDOW_BYTES,
                options.case_sensitive,
            );
            if snippets.is_empty() {
                (snippets, matched_lines) = fast_snippets_with_case(
                    title_text,
                    &tokens,
                    SNIPPET_WINDOW_BYTES,
                    options.case_sensitive,
                );
            }
            hit.snippets = snippets;
            hit.matched_lines = matched_lines;
        }
        Ok(())
    }

    /// 当前索引的 fingerprint（便于上层展示/诊断）。
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::current()
    }

    /// 底层 `Index` 只读访问（测试/诊断用）。
    pub fn index(&self) -> &Index {
        &self.index
    }

    fn query_highlight_tokens(&self, input: &str, options: QueryOptions) -> Vec<String> {
        let clauses = clauses_for(input);
        if !options.whole_word {
            let mut tokens = Vec::new();
            for (_quoted, text) in clauses {
                if !text.is_empty() && !tokens.contains(&text) {
                    tokens.push(text);
                }
            }
            return tokens;
        }
        let mut analyzer = if options.case_sensitive {
            analyzer::build_case_sensitive(self.jieba.clone())
        } else {
            analyzer::build(self.jieba.clone())
        };
        let mut tokens = Vec::new();
        for (_quoted, text) in clauses {
            for token in analyzer::tokens_with(&mut analyzer, &text) {
                if !token.is_empty() && !tokens.contains(&token) {
                    tokens.push(token);
                }
            }
        }
        tokens
    }

    fn build_token_query(
        &self,
        clauses: &[(bool, String)],
        options: QueryOptions,
    ) -> Option<Box<dyn Query>> {
        let mut analyzer = if options.case_sensitive {
            analyzer::build_case_sensitive(self.jieba.clone())
        } else {
            analyzer::build(self.jieba.clone())
        };
        let mut queries = Vec::new();
        for (quoted, text) in clauses {
            let tokens = analyzer::tokens_with(&mut analyzer, text);
            if tokens.is_empty() {
                continue;
            }
            if *quoted {
                queries.push(self.field_phrase_query(&tokens, options));
            } else {
                // 无引号：同一片段内的所有 token 都 AND（显式，不依赖 QueryParser 的隐含短语）。
                for token in tokens {
                    queries.push(self.field_term_query(&token, options));
                }
            }
        }
        combine_must(queries)
    }

    fn build_gram_query(
        &self,
        clauses: &[(bool, String)],
        options: QueryOptions,
    ) -> Option<Box<dyn Query>> {
        let mut queries = Vec::new();
        for (_quoted, text) in clauses {
            let normalized = if options.case_sensitive {
                text.clone()
            } else {
                text.to_lowercase()
            };
            for gram in ngrams(&normalized) {
                queries.push(self.field_gram_term_query(&gram, options));
            }
        }
        if queries.is_empty() {
            // 全部关键词都短于 2 个字符：无法用 trigram 召回，退化为全量候选，
            // 最终仍由 contains 校验决定结果。
            return Some(Box::new(AllQuery));
        }
        combine_must(queries)
    }

    /// 一个 token 在 title/body 上的 SHOULD 查询（大小写字段由 options 决定）。
    fn field_term_query(&self, token: &str, options: QueryOptions) -> Box<dyn Query> {
        let (title_field, body_field) = if options.case_sensitive {
            (self.fields.title_cs, self.fields.body_cs)
        } else {
            (self.fields.title, self.fields.body)
        };
        let title = TermQuery::new(
            Term::from_field_text(title_field, token),
            IndexRecordOption::WithFreqsAndPositions,
        );
        let body = TermQuery::new(
            Term::from_field_text(body_field, token),
            IndexRecordOption::WithFreqsAndPositions,
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, Box::new(title)),
            (Occur::Should, Box::new(body)),
        ]))
    }

    /// 一个短语在 title/body 上的 SHOULD 查询。
    fn field_phrase_query(&self, tokens: &[String], options: QueryOptions) -> Box<dyn Query> {
        if tokens.len() == 1 {
            return self.field_term_query(&tokens[0], options);
        }
        let (title_field, body_field) = if options.case_sensitive {
            (self.fields.title_cs, self.fields.body_cs)
        } else {
            (self.fields.title, self.fields.body)
        };
        let title = PhraseQuery::new(
            tokens
                .iter()
                .map(|token| Term::from_field_text(title_field, token))
                .collect(),
        );
        let body = PhraseQuery::new(
            tokens
                .iter()
                .map(|token| Term::from_field_text(body_field, token))
                .collect(),
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, Box::new(title)),
            (Occur::Should, Box::new(body)),
        ]))
    }

    fn field_gram_term_query(&self, gram: &str, options: QueryOptions) -> Box<dyn Query> {
        let (title_field, body_field) = if options.case_sensitive {
            (self.fields.title_gram_cs, self.fields.body_gram_cs)
        } else {
            (self.fields.title_gram, self.fields.body_gram)
        };
        let title = TermQuery::new(
            Term::from_field_text(title_field, gram),
            IndexRecordOption::Basic,
        );
        let body = TermQuery::new(
            Term::from_field_text(body_field, gram),
            IndexRecordOption::Basic,
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, Box::new(title)),
            (Occur::Should, Box::new(body)),
        ]))
    }
}

impl Fields {
    fn from_schema(schema: &Schema) -> Result<Self, SearchError> {
        Ok(Self {
            note_id: field(schema, "note_id")?,
            title: field(schema, "title")?,
            body: field(schema, "body")?,
            title_cs: field(schema, "title_cs")?,
            body_cs: field(schema, "body_cs")?,
            title_gram: field(schema, "title_gram")?,
            body_gram: field(schema, "body_gram")?,
            title_gram_cs: field(schema, "title_gram_cs")?,
            body_gram_cs: field(schema, "body_gram_cs")?,
            body_doc_id: field(schema, "body_doc_id")?,
            body_seq: field(schema, "body_seq")?,
            body_frontier_hash: field(schema, "body_frontier_hash")?,
            meta_seq: field(schema, "meta_seq")?,
            meta_hash: field(schema, "meta_hash")?,
        })
    }
}

fn field(schema: &Schema, name: &str) -> Result<Field, SearchError> {
    schema
        .get_field(name)
        .map_err(|error| SearchError::Schema(format!("缺少字段 {name:?}: {error}")))
}

/// 构造 schema；字段布局变化必须升 [`crate::SCHEMA_VERSION`]。
fn build_schema() -> Schema {
    let mut builder = SchemaBuilder::default();
    let token_indexing = TextFieldIndexing::default()
        .set_tokenizer(TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_options = TextOptions::default()
        .set_indexing_options(token_indexing)
        .set_stored();
    let cs_indexing = TextFieldIndexing::default()
        .set_tokenizer(CS_TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let cs_options = TextOptions::default().set_indexing_options(cs_indexing);
    let gram_indexing = TextFieldIndexing::default()
        .set_tokenizer(GRAM_TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::Basic);
    let gram_options = TextOptions::default().set_indexing_options(gram_indexing);

    builder.add_text_field("note_id", STRING | STORED);
    builder.add_text_field("title", text_options.clone());
    builder.add_text_field("body", text_options);
    builder.add_text_field("title_cs", cs_options.clone());
    builder.add_text_field("body_cs", cs_options);
    builder.add_text_field("title_gram", gram_options.clone());
    builder.add_text_field("body_gram", gram_options.clone());
    builder.add_text_field("title_gram_cs", gram_options.clone());
    builder.add_text_field("body_gram_cs", gram_options);
    builder.add_text_field("body_doc_id", STRING | STORED);
    builder.add_u64_field("body_seq", STORED);
    builder.add_u64_field("body_frontier_hash", STORED);
    builder.add_u64_field("meta_seq", STORED);
    builder.add_u64_field("meta_hash", STORED);
    builder.build()
}

fn read_fingerprint(dir: &Path) -> Result<Fingerprint, SearchError> {
    let path = dir.join(FINGERPRINT_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => Fingerprint::parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(SearchError::MissingFingerprint)
        }
        Err(error) => Err(SearchError::Io(error)),
    }
}

/// 把查询串切成 (是否引号, 文本)；不处理嵌套引号/转义引号（v1 不做查询语言）。
fn split_clauses(input: &str) -> Vec<(bool, String)> {
    let mut clauses = Vec::new();
    let mut buffer = String::new();
    let mut quoted = false;

    let flush = |quoted: bool, buffer: &mut String, clauses: &mut Vec<(bool, String)>| {
        let text = buffer.trim().to_owned();
        if !text.is_empty() {
            clauses.push((quoted, text));
        }
        buffer.clear();
    };

    for ch in input.chars() {
        if ch == '"' {
            if quoted {
                flush(true, &mut buffer, &mut clauses);
                quoted = false;
            } else {
                flush(false, &mut buffer, &mut clauses);
                quoted = true;
            }
        } else if ch.is_whitespace() && !quoted {
            flush(false, &mut buffer, &mut clauses);
        } else {
            buffer.push(ch);
        }
    }
    flush(quoted, &mut buffer, &mut clauses);
    clauses
}

fn first_text(document: &TantivyDocument, field: Field) -> &str {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

fn first_u64(document: &TantivyDocument, field: Field) -> u64 {
    document
        .get_first(field)
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

/// 把输入切成非空 clause；引号内文本保持为单个 clause。
fn clauses_for(input: &str) -> Vec<(bool, String)> {
    split_clauses(input)
        .into_iter()
        .filter(|(_quoted, text)| !text.trim().is_empty())
        .collect()
}

fn combine_must(mut queries: Vec<Box<dyn Query>>) -> Option<Box<dyn Query>> {
    match queries.len() {
        0 => None,
        1 => queries.pop(),
        _ => Some(Box::new(BooleanQuery::new(
            queries
                .into_iter()
                .map(|query| (Occur::Must, query))
                .collect(),
        ))),
    }
}

/// 2/3 字符 n-gram（与 `NgramTokenizer::new(2,3,false)` 对齐）；去重、稳定顺序。
fn ngrams(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut grams = BTreeSet::new();
    for size in 2..=3 {
        if chars.len() < size {
            continue;
        }
        for window in chars.windows(size) {
            grams.insert(window.iter().collect::<String>());
        }
    }
    grams.into_iter().collect()
}

fn verify_clauses(
    title: &str,
    body: &str,
    clauses: &[(bool, String)],
    case_sensitive: bool,
) -> bool {
    clauses.iter().all(|(_quoted, text)| {
        contains_clause(title, text, case_sensitive) || contains_clause(body, text, case_sensitive)
    })
}

fn contains_clause(text: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        text.contains(needle)
    } else {
        text.to_lowercase().contains(&needle.to_lowercase())
    }
}

/// 兼容旧调用：只返回第一条命中行的文本与区间。
///
/// 新代码应使用 [`SnippetLine`]/[`Hit::snippets`]；这里保留单行入口供诊断/测试。
pub fn fast_snippet(text: &str, tokens: &[String], window: usize) -> (String, Vec<Range<usize>>) {
    fast_snippet_with_case(text, tokens, window, false)
}

fn fast_snippet_with_case(
    text: &str,
    tokens: &[String],
    window: usize,
    case_sensitive: bool,
) -> (String, Vec<Range<usize>>) {
    let (snippets, _) = fast_snippets_with_case(text, tokens, window, case_sensitive);
    snippets
        .into_iter()
        .next()
        .map(|snippet| (snippet.text, snippet.ranges))
        .unwrap_or_default()
}

/// 收集正文全部命中行，并返回去重后的命中行总数。
fn fast_snippets_with_case(
    text: &str,
    tokens: &[String],
    window: usize,
    case_sensitive: bool,
) -> (Vec<SnippetLine>, usize) {
    if text.is_empty() || tokens.is_empty() {
        return (Vec::new(), 0);
    }
    let line_hits = collect_hit_lines(text, tokens, case_sensitive);
    let matched_lines = line_hits.len();
    let snippets = line_hits
        .into_iter()
        .map(|line_hit| build_line_snippet(&line_hit, window))
        .collect();
    (snippets, matched_lines)
}

struct LineHit {
    line_index: usize,
    text: String,
    ranges: Vec<Range<usize>>,
}

/// 按 `\n` 扫全文；每行最多保留一条；行内所有 token 出现都收集。
fn collect_hit_lines(text: &str, tokens: &[String], case_sensitive: bool) -> Vec<LineHit> {
    let mut out = Vec::new();
    for (line_index, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let ranges = collect_token_ranges(line, tokens, case_sensitive);
        if !ranges.is_empty() {
            out.push(LineHit {
                line_index,
                text: line.to_owned(),
                ranges,
            });
        }
    }
    out
}

fn collect_token_ranges(line: &str, tokens: &[String], case_sensitive: bool) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for token in tokens {
        if token.is_empty() {
            continue;
        }
        for start in find_all(line, token, case_sensitive) {
            ranges.push(start..start + token.len());
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    merge_ranges(ranges)
}

fn merge_ranges(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start < last.end => {
                if range.end > last.end {
                    last.end = range.end;
                }
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// 单行过长时以该行首个命中点为中心裁剪；两端加省略号，区间随文本平移。
fn build_line_snippet(line_hit: &LineHit, window: usize) -> SnippetLine {
    let line = &line_hit.text;
    if line.len() <= window || line_hit.ranges.is_empty() {
        return SnippetLine {
            text: line.clone(),
            ranges: line_hit.ranges.clone(),
            line_index: line_hit.line_index,
        };
    }

    let first = &line_hit.ranges[0];
    let mut content_start = first
        .start
        .saturating_sub(window.saturating_sub(first.len()) / 2);
    let mut content_end = (content_start + window).max(first.end).min(line.len());
    if content_end.saturating_sub(content_start) > window && content_start > 0 {
        content_start = content_end.saturating_sub(window);
    }
    if content_start > first.start {
        content_start = first.start;
    }
    if content_end < first.end {
        content_end = first.end;
    }
    while content_start < first.start && !line.is_char_boundary(content_start) {
        content_start += 1;
    }
    while content_end > first.end && !line.is_char_boundary(content_end) {
        content_end -= 1;
    }

    let prefix = content_start > 0;
    let suffix = content_end < line.len();
    let mut text = String::new();
    if prefix {
        text.push('…');
    }
    text.push_str(&line[content_start..content_end]);
    if suffix {
        text.push('…');
    }
    let base = if prefix { '…'.len_utf8() } else { 0 };

    let mut ranges = Vec::new();
    for range in &line_hit.ranges {
        if range.start >= content_start && range.end <= content_end {
            let start = base + (range.start - content_start);
            let end = base + (range.end - content_start);
            if end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end) {
                ranges.push(start..end);
            }
        }
    }
    SnippetLine {
        text,
        ranges: merge_ranges(ranges),
        line_index: line_hit.line_index,
    }
}

fn find_match(text: &str, token: &str, case_sensitive: bool) -> Option<usize> {
    if case_sensitive {
        text.find(token)
    } else {
        find_ci(text.as_bytes(), token.as_bytes())
    }
}

fn find_all(text: &str, token: &str, case_sensitive: bool) -> Vec<usize> {
    if token.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0usize;
    while from <= text.len().saturating_sub(token.len()) {
        let Some(relative) = find_match(&text[from..], token, case_sensitive) else {
            break;
        };
        let start = from + relative;
        out.push(start);
        from = start
            + text[start..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
    }
    out
}

/// 在字节串里找第一个 ASCII 大小写不敏感的出现位置。
fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::{fast_snippet, fast_snippet_with_case, fast_snippets_with_case, find_all, ngrams};

    fn assert_ranges_match(snippet: &str, ranges: &[std::ops::Range<usize>], expected: &[&str]) {
        let mut got: Vec<&str> = ranges.iter().map(|range| &snippet[range.clone()]).collect();
        got.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(got, expected, "snippet={snippet:?} ranges={ranges:?}");
    }

    #[test]
    fn snippet_is_exactly_the_hit_line() {
        let text = "第一行\n第二行命中点\n第三行";
        let (snippet, ranges) = fast_snippet(text, &["命中".into()], 160);
        assert_eq!(snippet, "第二行命中点");
        assert!(!snippet.contains('\n'));
        assert_ranges_match(&snippet, &ranges, &["命中"]);
    }

    #[test]
    fn long_line_is_trimmed_with_ellipsis_and_ranges_stay_valid() {
        let text = format!("{}命中{}", "a".repeat(300), "b".repeat(300));
        let (snippet, ranges) = fast_snippet(&text, &["命中".into()], 160);
        assert!(snippet.contains("命中"));
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(!snippet.contains('\n'));
        assert!(snippet.len() <= 160 + 2 * '…'.len_utf8());
        assert_ranges_match(&snippet, &ranges, &["命中"]);
        for range in &ranges {
            assert!(snippet.is_char_boundary(range.start));
            assert!(snippet.is_char_boundary(range.end));
        }
    }

    #[test]
    fn hit_at_line_start_and_end() {
        let text = "命中开头\n中间行\n结尾命中";
        let (start_snippet, start_ranges) = fast_snippet(text, &["命中".into()], 160);
        assert_eq!(start_snippet, "命中开头");
        assert_ranges_match(&start_snippet, &start_ranges, &["命中"]);

        let (end_snippet, end_ranges) = fast_snippet("只有结尾命中", &["命中".into()], 160);
        assert_eq!(end_snippet, "只有结尾命中");
        assert_ranges_match(&end_snippet, &end_ranges, &["命中"]);
    }

    #[test]
    fn all_hits_on_line_are_highlighted() {
        let text = "foo bar foo\n第二行 foo";
        let (snippet, ranges) = fast_snippet(text, &["foo".into()], 160);
        assert_eq!(snippet, "foo bar foo");
        assert_ranges_match(&snippet, &ranges, &["foo", "foo"]);
    }

    #[test]
    fn all_hit_lines_are_returned_without_cap() {
        let text = (0..5)
            .map(|index| format!("第 {index} 行命中"))
            .collect::<Vec<_>>()
            .join("\n");
        let (snippets, matched_lines) =
            fast_snippets_with_case(&text, &["命中".into()], 160, false);
        assert_eq!(matched_lines, 5);
        assert_eq!(snippets.len(), 5);
        assert_eq!(
            snippets
                .iter()
                .map(|snippet| snippet.line_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        for snippet in &snippets {
            assert!(!snippet.text.contains('\n'));
            assert_ranges_match(&snippet.text, &snippet.ranges, &["命中"]);
        }
    }

    #[test]
    fn one_hundred_hit_lines_are_all_returned() {
        let text = (0..100)
            .map(|index| format!("第 {index} 行命中"))
            .collect::<Vec<_>>()
            .join("\n");
        let (snippets, matched_lines) =
            fast_snippets_with_case(&text, &["命中".into()], 160, false);
        assert_eq!(matched_lines, 100);
        assert_eq!(snippets.len(), 100);
        assert_eq!(snippets[0].line_index, 0);
        assert_eq!(snippets[99].line_index, 99);
    }

    #[test]
    fn one_thousand_hit_lines_do_not_panic() {
        let text = (0..1000)
            .map(|index| format!("第 {index} 行命中"))
            .collect::<Vec<_>>()
            .join("\n");
        let (snippets, matched_lines) =
            fast_snippets_with_case(&text, &["命中".into()], 160, false);
        assert_eq!(matched_lines, 1000);
        assert_eq!(snippets.len(), 1000);
        assert_eq!(snippets[999].line_index, 999);
    }

    #[test]
    fn multiple_hits_on_one_long_line_are_one_snippet_with_all_ranges() {
        let mut line = "a".repeat(200);
        line.push_str("命中-命中-命中");
        line.push_str(&"b".repeat(200));
        let (snippets, matched_lines) =
            fast_snippets_with_case(&line, &["命中".into()], 160, false);
        assert_eq!(matched_lines, 1, "同一行只算一条命中行");
        assert_eq!(snippets.len(), 1);
        assert!(snippets[0].text.contains('…'));
        assert_ranges_match(
            &snippets[0].text,
            &snippets[0].ranges,
            &["命中", "命中", "命中"],
        );
    }

    #[test]
    fn chinese_and_english_are_supported() {
        let text = "The GPUI editor 笔记流程";
        let (snippet, ranges) = fast_snippet(text, &["gpui".into(), "笔记".into()], 160);
        assert_eq!(snippet, text);
        assert_ranges_match(&snippet, &ranges, &["GPUI", "笔记"]);
    }

    #[test]
    fn case_sensitive_snippet_matches_exact_case_only() {
        let text = "GPUI gpui";
        let (case_snippet, case_ranges) = fast_snippet_with_case(text, &["GPUI".into()], 160, true);
        assert_eq!(case_snippet, text);
        assert_ranges_match(&case_snippet, &case_ranges, &["GPUI"]);

        let (insensitive_snippet, insensitive_ranges) =
            fast_snippet_with_case(text, &["GPUI".into()], 160, false);
        assert_eq!(insensitive_snippet, text);
        assert_ranges_match(&insensitive_snippet, &insensitive_ranges, &["GPUI", "gpui"]);
    }

    #[test]
    fn match_at_end_of_one_megabyte_is_found() {
        let mut text = "a".repeat(1_000_000);
        text.push_str("末尾命中");
        let (snippet, ranges) = fast_snippet(&text, &["命中".into()], 160);
        assert!(snippet.contains("命中"), "1MB 末尾命中必须能找到");
        assert!(!snippet.contains('\n'));
        assert_ranges_match(&snippet, &ranges, &["命中"]);
    }

    #[test]
    fn utf8_boundaries_are_respected() {
        let text = "😀😀前命中😀😀后";
        let (snippet, ranges) = fast_snippet(text, &["命中".into()], 5);
        assert!(snippet.contains("命中"));
        for range in &ranges {
            assert!(snippet.is_char_boundary(range.start));
            assert!(snippet.is_char_boundary(range.end));
        }
        assert_ranges_match(&snippet, &ranges, &["命中"]);
    }

    #[test]
    fn empty_and_no_match_are_safe() {
        assert_eq!(
            fast_snippet("", &["笔记".into()], 160),
            (String::new(), Vec::new())
        );
        assert_eq!(
            fast_snippet("没有目标词", &["笔记".into()], 160),
            (String::new(), Vec::new())
        );
        assert_eq!(fast_snippet("有词", &[], 160), (String::new(), Vec::new()));
    }

    #[test]
    fn ngrams_are_two_and_three_chars() {
        assert_eq!(
            ngrams("abcd"),
            vec!["ab", "abc", "bc", "bcd", "cd"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        assert_eq!(ngrams("笔"), Vec::<String>::new());
    }

    #[test]
    fn find_all_finds_overlapping_matches() {
        assert_eq!(find_all("aaaa", "aa", true), vec![0, 1, 2]);
    }
}
