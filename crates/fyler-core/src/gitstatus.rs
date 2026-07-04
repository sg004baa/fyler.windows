//! Git status の描画装飾に使う共有型。

/// porcelain v1 のXYコードから写像するGit状態バッジ。
///
/// GUIはこの値をバッファ文字列へ混ぜず、装飾列にASCII 1文字で描く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitBadge {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}
