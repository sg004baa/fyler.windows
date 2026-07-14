//! `git status --porcelain=v1 -z` によるGit状態の読み取り。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fyler_core::GitBadge;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 表示ルート以下のGit状態を、ルート相対パスごとのバッジとして返す。
///
/// `git`が存在しない、表示ルートがリポジトリ外、またはgitコマンドが非0で終了した
/// 場合はエラー表示せず空のmapを返す。porcelain v1が返すリポジトリルート相対パスは、
/// `rev-parse --show-prefix`で表示ルート相対へ変換し、ルート外の状態を除外する。
pub fn status_badges(root: &Path) -> anyhow::Result<HashMap<PathBuf, GitBadge>> {
    let prefix_output = match git_output(root, &["rev-parse", "--show-prefix"]) {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return Ok(HashMap::new()),
    };
    let status_output = match git_output(root, &["status", "--porcelain=v1", "-z"]) {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return Ok(HashMap::new()),
    };

    let prefix = trim_line_ending(&prefix_output.stdout);
    Ok(parse_porcelain(&status_output.stdout, prefix))
}

/// 表示ルートが属するGitブランチ名を返す。
///
/// `git`が無い/リポジトリ外/コマンド失敗時は`None`。detached HEADでは短縮SHAを返す。
pub fn branch(root: &Path) -> Option<String> {
    // symbolic-refはコミットが無い(unborn)ブランチでも名前を返す。
    if let Ok(output) = git_output(root, &["symbolic-ref", "--short", "HEAD"])
        && output.status.success()
    {
        let name = String::from_utf8_lossy(trim_line_ending(&output.stdout))
            .trim()
            .to_owned();
        if !name.is_empty() {
            return Some(name);
        }
    }
    // detached HEAD: 短縮SHAで代替する。
    let short = git_output(root, &["rev-parse", "--short", "HEAD"]).ok()?;
    if !short.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(trim_line_ending(&short.stdout))
        .trim()
        .to_owned();
    (!sha.is_empty()).then_some(sha)
}

fn git_output(root: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.output()
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn parse_porcelain(output: &[u8], root_prefix: &[u8]) -> HashMap<PathBuf, GitBadge> {
    let mut badges = HashMap::new();
    let mut records = output.split(|byte| *byte == b'\0');

    while let Some(record) = records.next() {
        let Some((&x, rest)) = record.split_first() else {
            continue;
        };
        let Some((&y, rest)) = rest.split_first() else {
            continue;
        };
        let Some((&separator, path)) = rest.split_first() else {
            continue;
        };
        if separator != b' ' || path.is_empty() {
            continue;
        }

        // porcelain -zのrename/copyは「新パス\0旧パス\0」の順になる。
        // 旧パスを次のstatus行として誤解釈しないよう、必ずここで消費する。
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let _ = records.next();
        }

        let Some(badge) = badge_from_xy(x, y) else {
            continue;
        };
        let Some(relative) = path.strip_prefix(root_prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        let Some(relative) = path_buf_from_git_bytes(relative) else {
            continue;
        };
        badges.insert(relative, badge);
    }

    badges
}

fn badge_from_xy(x: u8, y: u8) -> Option<GitBadge> {
    if (x, y) == (b'?', b'?') {
        return Some(GitBadge::Untracked);
    }
    if x == b'U' || y == b'U' || matches!((x, y), (b'D', b'D') | (b'A', b'A')) {
        return Some(GitBadge::Conflicted);
    }
    if x == b'R' || y == b'R' {
        return Some(GitBadge::Renamed);
    }
    if x == b'D' || y == b'D' {
        return Some(GitBadge::Deleted);
    }
    if x == b'A' || y == b'A' {
        return Some(GitBadge::Added);
    }
    if matches!(x, b'M' | b'T') || matches!(y, b'M' | b'T') {
        return Some(GitBadge::Modified);
    }
    None
}

#[cfg(unix)]
fn path_buf_from_git_bytes(path: &[u8]) -> Option<PathBuf> {
    Some(PathBuf::from(OsString::from_vec(path.to_vec())))
}

#[cfg(windows)]
fn path_buf_from_git_bytes(path: &[u8]) -> Option<PathBuf> {
    String::from_utf8(path.to_vec()).ok().map(PathBuf::from)
}

#[cfg(not(any(unix, windows)))]
fn path_buf_from_git_bytes(path: &[u8]) -> Option<PathBuf> {
    String::from_utf8(path.to_vec()).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_modified_added_and_deleted() {
        let badges = parse_porcelain(
            b" M modified.txt\0A  added.txt\0 D deleted.txt\0T  type-changed.txt\0",
            b"",
        );

        assert_eq!(
            badges.get(Path::new("modified.txt")),
            Some(&GitBadge::Modified)
        );
        assert_eq!(badges.get(Path::new("added.txt")), Some(&GitBadge::Added));
        assert_eq!(
            badges.get(Path::new("deleted.txt")),
            Some(&GitBadge::Deleted)
        );
        assert_eq!(
            badges.get(Path::new("type-changed.txt")),
            Some(&GitBadge::Modified)
        );
    }

    #[test]
    fn parses_untracked() {
        let badges = parse_porcelain(b"?? new.txt\0", b"");

        assert_eq!(badges.get(Path::new("new.txt")), Some(&GitBadge::Untracked));
    }

    #[test]
    fn parses_rename_new_path_and_skips_old_path() {
        let badges = parse_porcelain(b"R  new.txt\0old.txt\0 M after.txt\0", b"");

        assert_eq!(badges.get(Path::new("new.txt")), Some(&GitBadge::Renamed));
        assert!(!badges.contains_key(Path::new("old.txt")));
        assert_eq!(
            badges.get(Path::new("after.txt")),
            Some(&GitBadge::Modified)
        );
    }

    #[test]
    fn conflicted_has_priority() {
        let badges = parse_porcelain(
            b"UU both.txt\0DD deleted.txt\0AA added.txt\0RU renamed.txt\0old.txt\0",
            b"",
        );

        for path in ["both.txt", "deleted.txt", "added.txt", "renamed.txt"] {
            assert_eq!(badges.get(Path::new(path)), Some(&GitBadge::Conflicted));
        }
    }

    #[test]
    fn strips_subdirectory_prefix_and_discards_root_outside_paths() {
        let badges = parse_porcelain(
            b" M crates/app/src/main.rs\0 M crates/core/src/lib.rs\0",
            b"crates/app/",
        );

        assert_eq!(
            badges.get(Path::new("src/main.rs")),
            Some(&GitBadge::Modified)
        );
        assert_eq!(badges.len(), 1);
    }

    #[test]
    fn empty_input_returns_empty_map() {
        assert!(parse_porcelain(b"", b"").is_empty());
    }

    #[test]
    fn unparseable_records_are_skipped() {
        let badges = parse_porcelain(b"broken\0 M valid.txt\0", b"");

        assert_eq!(
            badges.get(Path::new("valid.txt")),
            Some(&GitBadge::Modified)
        );
        assert_eq!(badges.len(), 1);
    }

    #[test]
    fn status_badges_reads_an_initialized_repository() {
        let root = tempdir().unwrap();
        let version = git_output(root.path(), &["--version"]);
        if !version.is_ok_and(|output| output.status.success()) {
            return;
        }

        let init = git_output(root.path(), &["init", "--quiet"]).unwrap();
        assert!(init.status.success());
        fs::write(root.path().join("untracked.txt"), b"new").unwrap();

        let badges = status_badges(root.path()).unwrap();

        assert_eq!(
            badges.get(Path::new("untracked.txt")),
            Some(&GitBadge::Untracked)
        );
    }

    #[test]
    fn branch_reads_current_branch_of_initialized_repository() {
        let root = tempdir().unwrap();
        let version = git_output(root.path(), &["--version"]);
        if !version.is_ok_and(|output| output.status.success()) {
            return;
        }

        assert!(
            git_output(root.path(), &["init", "--quiet"])
                .unwrap()
                .status
                .success()
        );
        // ブランチ名を固定してデフォルト名(main/master)差を避ける。
        assert!(
            git_output(root.path(), &["checkout", "-q", "-b", "trunk"])
                .unwrap()
                .status
                .success()
        );

        assert_eq!(branch(root.path()).as_deref(), Some("trunk"));
        assert_eq!(branch(&root.path().join("missing")), None);
    }
}
