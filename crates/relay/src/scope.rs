//! 显式租户作用域（ADR 0014 §A）。
//!
//! relay 的每个租户存储操作都必须带一个**显式** [`VaultScope`]：
//! - 不存在 `current`/`default` 之类的隐式回退，也**不按 token subject 猜租户**；
//! - v1 旧 API 需要的 legacy 默认 vault 由 [`crate::vault::resolve_legacy_scope`] 显式解析
//!   （部署配置覆盖，或 `vaults.legacy_default = 1`），解析失败就是 404/未初始化，
//!   绝不"挑第一个 vault"；
//! - [`ScopeConn`] 把 `(Connection, VaultScope)` 绑成一体：领域模块只接受 `&ScopeConn`，
//!   于是"忘了带 scope"在类型上就写不出来。
//!
//! SQL 约定（重要）：租户表的每条语句都要显式写 `vault_id` 条件/列，并把 scope 作为
//! **最后一个绑定参数**；SQL 里用匿名 `?` 接在既有 `?1..?n` 之后（SQLite 会把它编号为
//! `n+1`，正好对应参数元组的最后一项）。这样既不改动既有参数编号，也不可能漏绑。

use std::ops::Deref;

use rusqlite::Connection;

use crate::ids::Id16;

/// 一个 vault 的显式作用域（128-bit 公开路由标识，不是授权）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VaultScope {
    vault_id: Id16,
}

impl VaultScope {
    /// 由路由/初始化请求里的 `vault_id` 构造。
    pub const fn new(vault_id: Id16) -> Self {
        Self { vault_id }
    }

    /// 该作用域的 `vault_id`。
    pub const fn vault_id(&self) -> &Id16 {
        &self.vault_id
    }

    /// 绑定参数用（16 字节 BLOB）。
    pub fn as_slice(&self) -> &[u8] {
        &self.vault_id
    }
}

impl From<Id16> for VaultScope {
    fn from(vault_id: Id16) -> Self {
        Self::new(vault_id)
    }
}

/// 带作用域的连接：领域函数只拿到它（而不是裸 `Connection`）。
#[derive(Debug, Clone, Copy)]
pub struct ScopeConn<'a> {
    conn: &'a Connection,
    scope: VaultScope,
}

impl<'a> ScopeConn<'a> {
    /// 绑定连接与作用域（`Db::read`/`Db::write` 内部使用；测试可直接构造）。
    pub const fn new(conn: &'a Connection, scope: VaultScope) -> Self {
        Self { conn, scope }
    }

    /// 当前作用域。
    pub const fn scope(&self) -> VaultScope {
        self.scope
    }

    /// 当前作用域的 `vault_id`。
    pub const fn vault_id(&self) -> &Id16 {
        self.scope.vault_id()
    }

    /// 裸连接：**只允许**访问实例/运维元数据（`relay_meta`、`sqlite_master`）或做显式
    /// 跨租户运维；任何租户表读写都必须走带 `vault_id` 的 scoped SQL。
    pub const fn raw(&self) -> &'a Connection {
        self.conn
    }
}

impl Deref for ScopeConn<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_explicit_and_converts_from_id() {
        let id: Id16 = [9u8; 16];
        let scope = VaultScope::from(id);
        assert_eq!(scope.vault_id(), &id);
        assert_eq!(scope.as_slice(), id.as_slice());
        assert_ne!(scope, VaultScope::new([8u8; 16]));
    }

    #[test]
    fn scope_conn_exposes_scope_and_raw_connection() {
        let conn = Connection::open_in_memory().expect("open");
        let scope = VaultScope::new([1u8; 16]);
        let scoped = ScopeConn::new(&conn, scope);
        assert_eq!(scoped.scope(), scope);
        assert_eq!(scoped.vault_id(), &[1u8; 16]);
        // Deref 到 rusqlite::Connection（PRAGMA 等通用能力仍可用）。
        let mode: String = scoped
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("pragma");
        assert!(!mode.is_empty());
        assert!(std::ptr::eq(scoped.raw(), &conn));
    }
}
