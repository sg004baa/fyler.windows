//! fylerが利用するNeovim実行ファイルの探索。

use std::path::{Path, PathBuf};

/// 選択した実行ファイルと、起動失敗時に表示する探索結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedNvim {
    pub(super) path: PathBuf,
    pub(super) diagnostics: Vec<String>,
}

/// 環境とfyler自身の配置からNeovim実行ファイルを解決する。
pub(super) fn resolve() -> ResolvedNvim {
    let override_exe = std::env::var_os("FYLER_NVIM_EXE").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    resolve_from(override_exe, exe.as_deref().and_then(Path::parent))
}

/// 外部状態の読み取りを分離した、探索順序の正典。
pub(super) fn resolve_from(override_exe: Option<PathBuf>, exe_dir: Option<&Path>) -> ResolvedNvim {
    let bundled = exe_dir.map(|dir| dir.join("nvim").join("bin").join(nvim_filename()));

    if let Some(path) = override_exe {
        return ResolvedNvim {
            diagnostics: vec![
                format!("FYLER_NVIM_EXE: using ({})", path.display()),
                bundled_status(bundled.as_deref(), false),
                "nvim on PATH: not searched".to_owned(),
            ],
            path,
        };
    }

    let mut diagnostics = vec!["FYLER_NVIM_EXE: not set".to_owned()];
    if let Some(path) = bundled {
        if path.is_file() {
            diagnostics.push(format!("Bundled version {}: using", path.display()));
            diagnostics.push("nvim on PATH: not searched".to_owned());
            return ResolvedNvim { path, diagnostics };
        }
        diagnostics.push(format!("Bundled version {}: not found", path.display()));
    } else {
        diagnostics.push(
            "Bundled version: not searched because the fyler executable location is unavailable"
                .to_owned(),
        );
    }

    diagnostics.push("nvim on PATH: using".to_owned());
    ResolvedNvim {
        path: PathBuf::from("nvim"),
        diagnostics,
    }
}

fn bundled_status(path: Option<&Path>, checked: bool) -> String {
    match (path, checked) {
        (Some(path), true) if path.is_file() => {
            format!("Bundled version {}: using", path.display())
        }
        (Some(path), true) => format!("Bundled version {}: not found", path.display()),
        (Some(path), false) => format!("Bundled version {}: not searched", path.display()),
        (None, _) => {
            "Bundled version: not searched because the fyler executable location is unavailable"
                .to_owned()
        }
    }
}

#[cfg(windows)]
fn nvim_filename() -> &'static str {
    "nvim.exe"
}

#[cfg(not(windows))]
fn nvim_filename() -> &'static str {
    "nvim"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_bundled_nvim(exe_dir: &Path) -> PathBuf {
        let path = exe_dir.join("nvim").join("bin").join(nvim_filename());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn override_is_used_without_checking_existence() {
        let temp = tempfile::tempdir().unwrap();
        let override_exe = temp.path().join("存在しない nvim.exe");

        let resolved = resolve_from(Some(override_exe.clone()), Some(temp.path()));

        assert_eq!(resolved.path, override_exe);
        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|line| line.contains("FYLER_NVIM_EXE") && line.contains("using"))
        );
    }

    #[test]
    fn bundled_nvim_is_used_when_present() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = create_bundled_nvim(temp.path());

        let resolved = resolve_from(None, Some(temp.path()));

        assert_eq!(resolved.path, bundled);
    }

    #[test]
    fn path_is_used_when_bundled_nvim_is_absent() {
        let temp = tempfile::tempdir().unwrap();

        let resolved = resolve_from(None, Some(temp.path()));

        assert_eq!(resolved.path, PathBuf::from("nvim"));
    }

    #[test]
    fn bundled_nvim_supports_spaces_and_japanese_in_exe_dir() {
        let temp = tempfile::tempdir().unwrap();
        let exe_dir = temp.path().join("日本語 のフォルダー");
        let bundled = create_bundled_nvim(&exe_dir);

        let resolved = resolve_from(None, Some(&exe_dir));

        assert_eq!(resolved.path, bundled);
    }

    #[test]
    fn diagnostics_include_all_three_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = resolve_from(None, Some(temp.path()));
        let diagnostics = resolved.diagnostics.join("\n");

        assert!(diagnostics.contains("FYLER_NVIM_EXE"));
        assert!(diagnostics.contains("Bundled version"));
        assert!(diagnostics.contains("nvim on PATH"));
        assert!(diagnostics.contains("not set"));
        assert!(diagnostics.contains("not found"));
        assert!(diagnostics.contains("using"));
    }
}
