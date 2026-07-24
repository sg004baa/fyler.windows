//! symlink/junctionのリンク先解決。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// symlink/junctionがディレクトリを指す場合、その実体の絶対パスを返す。
///
/// ファイル等を指す場合は`Ok(None)`。broken link・アクセス不能はErr(fail fast)。
/// 呼び出し側は`Err`の場合にopen等へフォールバックしてはならない。
pub fn resolve_link_dir(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let metadata = fs::metadata(crate::long_path::to_fs(path))
        .with_context(|| format!("Failed to resolve link target: {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(None);
    }
    let canonical = fs::canonicalize(crate::long_path::to_fs(path))
        .with_context(|| format!("Failed to canonicalize link target: {}", path.display()))?;
    Ok(Some(crate::long_path::from_fs(&canonical)))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolve_link_dir_returns_target_for_dir_symlink() {
        let root = tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let resolved = resolve_link_dir(&link).unwrap().unwrap();

        assert_eq!(resolved, target.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_link_dir_returns_none_for_file_symlink() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.txt");
        fs::write(&target, b"content").unwrap();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(resolve_link_dir(&link).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_link_dir_errors_on_broken_link() {
        let root = tempdir().unwrap();
        let link = root.path().join("broken");
        std::os::unix::fs::symlink(root.path().join("missing"), &link).unwrap();

        assert!(resolve_link_dir(&link).is_err());
    }
}
