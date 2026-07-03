//! ファイルシステムエントリの一意ID(in-buffer ID方式)。

use std::fmt;

/// ファイルシステムエントリをセッション内で一意に識別するID。
///
/// バッファ各行の `/{id} ` プレフィックス(例: `/012 `)としてテキスト自体に
/// 埋め込まれる(oil.nvim方式。DESIGN.md「行ID追跡」)。テキストの一部であるため
/// `dd`/`p`/`yy`/`:m`/`:s` 等あらゆるVim操作で行に付いて回る。
///
/// - 永続化しない。アプリ起動中のセッション内でのみ一意であればよい
/// - テキスト表現との変換は [`crate::grammar`] を使うこと(再実装禁止)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub u64);

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// [`EntryId`] の採番器。baselineスキャン(`fyler-fsops::scan`)が使う。
#[derive(Debug)]
pub struct IdAllocator {
    next: u64,
}

impl IdAllocator {
    pub fn new() -> Self {
        Self { next: 1 }
    }

    pub fn allocate(&mut self) -> EntryId {
        let id = EntryId(self.next);
        self.next += 1;
        id
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}
