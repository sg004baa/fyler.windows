//! ルート相対のツリー内パス。
//!
//! parse / validate / diff 層はOSのパス表現(区切りや拡張長パス形式)に依存しては
//! ならないため、コンポーネント列としてパスを扱う。実FSパスへの変換は
//! [`TreePath::to_fs_path`] だけで行い、OS固有変換はfsops層(long_path)の責務。

use std::fmt;
use std::path::{Path, PathBuf};

/// 表示ルートからの相対パス(コンポーネント列)。
///
/// - 各コンポーネントは空文字列でない名前(区切り文字を含まない)
/// - `Display` は `/` 区切り(ログ・テスト・ダイアログ表示用)
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreePath(Vec<String>);

impl TreePath {
    /// 表示ルート自身を指す空パス。
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// `"src/main.rs"` 形式の文字列からパースする(テスト・ログ用の簡易形)。
    /// 空コンポーネントは無視する。
    pub fn parse(s: &str) -> Self {
        Self(
            s.split('/')
                .filter(|c| !c.is_empty())
                .map(String::from)
                .collect(),
        )
    }

    pub fn from_components<I, S>(components: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(components.into_iter().map(Into::into).collect())
    }

    pub fn components(&self) -> &[String] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// 階層の深さ。ルート直下のエントリは1。
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// 末尾コンポーネント(= エントリ名)。ルートはNone。
    pub fn name(&self) -> Option<&str> {
        self.0.last().map(String::as_str)
    }

    pub fn parent(&self) -> Option<TreePath> {
        if self.0.is_empty() {
            None
        } else {
            Some(Self(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    pub fn child(&self, name: impl Into<String>) -> TreePath {
        let mut components = self.0.clone();
        components.push(name.into());
        Self(components)
    }

    /// selfがotherの真の祖先(self自身は含まない)かどうか。
    /// 「ディレクトリの自分自身の子孫への移動」の検出などに使う。
    pub fn is_strict_ancestor_of(&self, other: &TreePath) -> bool {
        self.0.len() < other.0.len() && other.0[..self.0.len()] == self.0[..]
    }

    /// 実FSパスへ変換する。OS固有変換はここではしない(fsops::long_pathの責務)。
    pub fn to_fs_path(&self, root: &Path) -> PathBuf {
        self.0.iter().fold(root.to_path_buf(), |p, c| p.join(c))
    }
}

impl fmt::Display for TreePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_roundtrip() {
        let p = TreePath::parse("src/main.rs");
        assert_eq!(p.components(), ["src", "main.rs"]);
        assert_eq!(p.to_string(), "src/main.rs");
        assert_eq!(p.name(), Some("main.rs"));
        assert_eq!(p.parent(), Some(TreePath::parse("src")));
        assert_eq!(p.depth(), 2);
    }

    #[test]
    fn root_path() {
        let root = TreePath::root();
        assert!(root.is_root());
        assert_eq!(root.parent(), None);
        assert_eq!(root.name(), None);
        assert_eq!(root.child("a"), TreePath::parse("a"));
    }

    #[test]
    fn strict_ancestor() {
        let a = TreePath::parse("a");
        let ab = TreePath::parse("a/b");
        let ax = TreePath::parse("ax/b");
        assert!(a.is_strict_ancestor_of(&ab));
        assert!(!a.is_strict_ancestor_of(&a));
        assert!(!a.is_strict_ancestor_of(&ax));
        assert!(TreePath::root().is_strict_ancestor_of(&a));
    }

    #[test]
    fn to_fs_path_joins_components() {
        let p = TreePath::parse("src/main.rs");
        assert_eq!(
            p.to_fs_path(Path::new("/root")),
            Path::new("/root").join("src").join("main.rs")
        );
    }
}
