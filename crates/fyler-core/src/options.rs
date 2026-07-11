//! 表示・走査に共通するエンジン非依存の設定型。

/// ツリーエントリのソート順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// ディレクトリを先にまとめ、それぞれを自然順で並べる。
    #[default]
    DirsFirst,
    /// 種別を分けず、ディレクトリとファイルを自然順で混在させる。
    Mixed,
}

/// ツリーエントリのソートキー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// 名前の自然順(既定)。
    #[default]
    Name,
    /// 更新日時順。
    Date,
    /// バイトサイズ順。
    Size,
    /// 拡張子順。
    Extension,
}
