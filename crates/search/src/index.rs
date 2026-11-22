//! tantivy 索引生命周期与查询。
//!
//! 设计要点：
//! - 一个 [`SearchIndex`] 持有 `Index` + `IndexReader` + `IndexWriter`；
//!   `upsert/delete` 只改 writer，显式 [`SearchIndex::commit`] 后才对查询可见
//!   （与 storage 的提交边界对齐，避免"索引可见但数据库未提交"）。
//! - `note_id` 用 raw STRING 字段做 term 删除；同名 upsert = 先删旧、再加新。
//! - 版本字段只 stored，不建索引；查询返回 `Hit` 不带版本（对账由上层读索引清单）。
//! - snippet 区间是 `snippet_text` 内的 UTF-8 字节区间，**不绑定原正文**。

use std::fs;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cang_jie::jieba_rs::Jieba;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, SchemaBuilder, TextFieldIndexing, TextOptions, Value, STORED,
    STRING,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::analyzer::{self, TOKENIZER_NAME};
use crate::error::SearchError;
use crate::fingerprint::{Fingerprint, FINGERPRINT_FILE};

/// 索引内存预算（byte）。v1 与探针一致取 50 MB；数据量上来后再调。
const WRITER_MEMORY_BUDGET: usize = 50_000_000;
/// snippet 最大字符数（不是字节）。
const SNIPPET_MAX_CHARS: usize = 120;

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

/// 一条检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// 笔记 ID。
    pub note_id: String,
    /// 标题。
    pub title: String,
    /// snippet 文本（纯文本，非 HTML）；[`Self::snippet_ranges`] 是它的字节区间。
    pub snippet_text: String,
    /// 高亮区间，UTF-8 **字节**偏移，绑定 `snippet_text`。
    pub snippet_ranges: Vec<Range<usize>>,
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

    /// 查询；返回按 BM25 降序的命中。
    ///
    /// 语义（显式）：
    /// - 引号包住的片段 = 短语（字段内相邻）；
    /// - 空格分隔的片段 = AND；
    /// - 无空格中文串 = 其 analyzer token 的 AND（**不是**隐式短语）。
    ///
    /// 空查询/纯空白返回空 vec，不报错。
    pub fn query(&self, q: &str, limit: usize) -> Result<Vec<Hit>, SearchError> {
        if limit == 0 || q.trim().is_empty() {
            return Ok(Vec::new());
        }
        let Some(query) = self.build_query(q) else {
            return Ok(Vec::new());
        };

        let searcher = self.reader.searcher();
        let top_docs = searcher.search(&*query, &TopDocs::with_limit(limit).order_by_score())?;

        let mut body_snippets = SnippetGenerator::create(&searcher, &*query, self.fields.body)?;
        body_snippets.set_max_num_chars(SNIPPET_MAX_CHARS);
        let mut title_snippets = SnippetGenerator::create(&searcher, &*query, self.fields.title)?;
        title_snippets.set_max_num_chars(SNIPPET_MAX_CHARS);

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, address) in top_docs {
            let document: TantivyDocument = searcher.doc(address)?;
            let note_id = first_text(&document, self.fields.note_id).to_owned();
            let title = first_text(&document, self.fields.title).to_owned();
            let body_text = first_text(&document, self.fields.body);

            let (snippet_text, snippet_ranges) =
                choose_snippet(&body_snippets, &title_snippets, &document, body_text);

            hits.push(Hit {
                note_id,
                title,
                snippet_text,
                snippet_ranges,
                score,
            });
        }
        Ok(hits)
    }

    /// 当前索引的 fingerprint（便于上层展示/诊断）。
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::current()
    }

    /// 底层 `Index` 只读访问（测试/诊断用）。
    pub fn index(&self) -> &Index {
        &self.index
    }

    fn build_query(&self, input: &str) -> Option<Box<dyn Query>> {
        // 索引与查询共用同一 analyzer 定义；这里只构建一次，所有 clause 复用。
        let mut analyzer = analyzer::build(self.jieba.clone());
        let mut clauses = Vec::new();
        for (quoted, text) in split_clauses(input) {
            let tokens = analyzer::tokens_with(&mut analyzer, &text);
            if tokens.is_empty() {
                continue;
            }
            if quoted {
                clauses.push(self.field_phrase_query(&tokens));
            } else {
                // 无引号：同一片段内的所有 token 都 AND（显式，不依赖 QueryParser 的隐含短语）。
                for token in tokens {
                    clauses.push(self.field_term_query(&token));
                }
            }
        }

        match clauses.len() {
            0 => None,
            1 => clauses.pop(),
            _ => Some(Box::new(BooleanQuery::new(
                clauses
                    .into_iter()
                    .map(|query| (Occur::Must, query))
                    .collect(),
            ))),
        }
    }

    /// 一个 token 在 title/body 上的 SHOULD 查询。
    fn field_term_query(&self, token: &str) -> Box<dyn Query> {
        let title = TermQuery::new(
            Term::from_field_text(self.fields.title, token),
            IndexRecordOption::WithFreqsAndPositions,
        );
        let body = TermQuery::new(
            Term::from_field_text(self.fields.body, token),
            IndexRecordOption::WithFreqsAndPositions,
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, Box::new(title)),
            (Occur::Should, Box::new(body)),
        ]))
    }

    /// 一个短语在 title/body 上的 SHOULD 查询。
    fn field_phrase_query(&self, tokens: &[String]) -> Box<dyn Query> {
        if tokens.len() == 1 {
            return self.field_term_query(&tokens[0]);
        }
        let title = PhraseQuery::new(
            tokens
                .iter()
                .map(|token| Term::from_field_text(self.fields.title, token))
                .collect(),
        );
        let body = PhraseQuery::new(
            tokens
                .iter()
                .map(|token| Term::from_field_text(self.fields.body, token))
                .collect(),
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
    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer(TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_options = TextOptions::default()
        .set_indexing_options(text_indexing)
        .set_stored();

    builder.add_text_field("note_id", STRING | STORED);
    builder.add_text_field("title", text_options.clone());
    builder.add_text_field("body", text_options);
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

/// 优先 body snippet；body 没有命中片段（比如只命中标题）时退回 title snippet。
fn choose_snippet(
    body_generator: &SnippetGenerator,
    title_generator: &SnippetGenerator,
    document: &TantivyDocument,
    body_text: &str,
) -> (String, Vec<Range<usize>>) {
    for generator in [body_generator, title_generator] {
        let snippet = generator.snippet_from_doc(document);
        let fragment = snippet.fragment();
        if !fragment.is_empty() {
            let text = fragment.to_owned();
            let ranges = snippet
                .highlighted()
                .iter()
                .filter(|range| {
                    range.start < range.end
                        && range.end <= text.len()
                        && text.is_char_boundary(range.start)
                        && text.is_char_boundary(range.end)
                })
                .cloned()
                .collect();
            return (text, ranges);
        }
    }

    // 查询命中但生成器没给出片段（例如只剩标题命中）：给正文开头作为兜底。
    let text: String = body_text.chars().take(SNIPPET_MAX_CHARS).collect();
    (text, Vec::new())
}
