//! 長いパス対応。**`\\?\` プレフィックスを扱ってよいのはこのモジュールだけ**(絶対ルール3)。
//!
//! 背景(DESIGN.md「その他の対応事項」):
//! - `\\?\` prefixは**絶対パス専用**で、`.` `..` `/` の解釈も変わる
//!   (正規化されずそのままファイルシステムへ渡る)
//! - アプリmanifestには `longPathAware` を入れる(fyler-appのビルド設定。M5)

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

const EXTENDED_PREFIX: &str = r"\\?\";
const EXTENDED_UNC_PREFIX: &str = r"\\?\UNC\";
const UNC_PREFIX: &str = r"\\";

/// `fs::*` 呼び出し直前の最終変換を行う。
///
/// Windowsでは [`to_extended`] を試み、相対パス・非UTF-8等で変換できない場合は
/// 元のパスを返す。非Windowsでは恒等変換する。`\\?\` を扱うのはこのモジュール
/// だけとする(絶対ルール3)。
pub fn to_fs(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        to_extended(path).unwrap_or_else(|_| path.to_path_buf())
    }

    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// MAX_PATHを超えるパスでもWin32 APIに渡せる形へ変換する。
///
/// 実装契約:
/// - 入力は**絶対パスであること**(相対パスはエラー。呼び出し側で解決してから渡す)
/// - 通常パス `C:\foo` → `\\?\C:\foo`
/// - UNCパス `\\server\share\foo` → `\\?\UNC\server\share\foo`
/// - すでに `\\?\` 付きならそのまま返す
/// - `/` 区切り・`.`・`..` を含む場合は先に正規化する(`\\?\` 下では解釈されないため)
/// - 短いパスには付けない選択もあるが、判定を分岐させず常に付けてよい
///   (挙動が一貫する方を優先)。方針変更するならこのdocを更新すること
pub fn to_extended(path: &Path) -> anyhow::Result<PathBuf> {
    let raw = path.to_str().with_context(|| {
        format!(
            "Windows path cannot be represented as UTF-8: {}",
            path.display()
        )
    })?;

    if raw.starts_with(EXTENDED_PREFIX) {
        return Ok(path.to_path_buf());
    }

    let normalized_separators = raw.replace('/', r"\");
    let normalized = if let Some(unc) = normalized_separators.strip_prefix(UNC_PREFIX) {
        normalize_unc(unc)?
    } else if is_drive_absolute(&normalized_separators) {
        normalize_drive(&normalized_separators)
    } else {
        bail!("Path is not absolute: {}", path.display());
    };

    Ok(PathBuf::from(normalized))
}

fn is_drive_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

fn normalize_drive(path: &str) -> String {
    let drive = &path[..2];
    let components = normalize_components(path[3..].split('\\'));
    if components.is_empty() {
        format!("{EXTENDED_PREFIX}{drive}\\")
    } else {
        format!("{EXTENDED_PREFIX}{drive}\\{}", components.join(r"\"))
    }
}

fn normalize_unc(path: &str) -> anyhow::Result<String> {
    let mut components = path.split('\\').filter(|component| !component.is_empty());
    let server = components
        .next()
        .filter(|component| *component != "." && *component != "..")
        .context("UNC path has no server name")?;
    let share = components
        .next()
        .filter(|component| *component != "." && *component != "..")
        .context("UNC path has no share name")?;
    let remainder = normalize_components(components);

    let mut normalized = format!("{EXTENDED_UNC_PREFIX}{server}\\{share}");
    if !remainder.is_empty() {
        normalized.push('\\');
        normalized.push_str(&remainder.join(r"\"));
    }
    Ok(normalized)
}

fn normalize_components<'a>(components: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut normalized = Vec::new();
    for component in components {
        match component {
            "" | "." => {}
            ".." => {
                normalized.pop();
            }
            component => normalized.push(component),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extends_drive_absolute_path() {
        assert_eq!(
            to_extended(Path::new(r"C:\foo\bar.txt")).unwrap(),
            PathBuf::from(r"\\?\C:\foo\bar.txt")
        );
    }

    #[test]
    fn extends_unc_path() {
        assert_eq!(
            to_extended(Path::new(r"\\server\share\foo")).unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\foo")
        );
    }

    #[test]
    fn preserves_existing_extended_prefix() {
        let path = Path::new(r"\\?\C:\foo");
        assert_eq!(to_extended(path).unwrap(), path);
    }

    #[test]
    fn rejects_relative_path() {
        assert!(to_extended(Path::new(r"foo\bar")).is_err());
    }

    #[test]
    fn normalizes_separators_and_parent_components_before_extending() {
        assert_eq!(
            to_extended(Path::new("C:/foo/./child/../bar")).unwrap(),
            PathBuf::from(r"\\?\C:\foo\bar")
        );
        assert_eq!(
            to_extended(Path::new(r"\\server\share\foo\..\bar")).unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\bar")
        );
    }

    #[cfg(windows)]
    #[test]
    fn to_fs_extends_absolute_path_and_preserves_unconvertible_relative_path() {
        assert_eq!(
            to_fs(Path::new(r"C:\foo\bar.txt")),
            PathBuf::from(r"\\?\C:\foo\bar.txt")
        );
        assert_eq!(
            to_fs(Path::new(r"relative\file.txt")),
            PathBuf::from(r"relative\file.txt")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn to_fs_is_identity_off_windows() {
        let path = Path::new("/tmp/fyler/../file.txt");
        assert_eq!(to_fs(path), path);
    }
}
