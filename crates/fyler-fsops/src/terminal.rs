//! 外部terminal emulatorの起動。
//!
//! shell文字列は組み立てず、program / args / current_dirを個別に指定する。
//! terminal emulatorは拡張形式パスを扱えない場合があるため、cwdには
//! `long_path::to_fs`を通さない素の絶対パスを渡す。spawnしたプロセスは
//! 待たず、killもしない。

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use anyhow::bail;
use fyler_core::options::TerminalKind;

/// 起動候補1つ分のコマンド仕様。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// 実行するprogram(PATH解決に任せる)。
    pub program: &'static str,
    /// programへ渡す引数。各要素はshell分割されない。
    pub args: Vec<OsString>,
}

#[cfg(windows)]
fn candidates(kind: TerminalKind, cwd: &Path) -> Vec<CommandSpec> {
    let windows_terminal = || CommandSpec {
        program: "wt.exe",
        args: vec![OsString::from("-d"), cwd.as_os_str().to_owned()],
    };
    let powershell = || CommandSpec {
        program: "powershell.exe",
        args: vec![OsString::from("-NoExit")],
    };
    let cmd = || CommandSpec {
        program: "cmd.exe",
        args: Vec::new(),
    };

    match kind {
        TerminalKind::Auto => vec![windows_terminal(), powershell(), cmd()],
        TerminalKind::WindowsTerminal => vec![windows_terminal()],
        TerminalKind::PowerShell => vec![powershell()],
        TerminalKind::Cmd => vec![cmd()],
    }
}

#[cfg(not(windows))]
fn candidates(_kind: TerminalKind, _cwd: &Path) -> Vec<CommandSpec> {
    vec![CommandSpec {
        program: "x-terminal-emulator",
        args: Vec::new(),
    }]
}

/// `cwd`を作業ディレクトリとして外部terminalを起動する。
///
/// 候補を順に試し、最初にspawnに成功した時点で戻る。起動したプロセスの終了は
/// 待たず、全候補が失敗した場合は試したprogram名を含むエラーを返す。
pub fn open(cwd: &Path, kind: TerminalKind) -> anyhow::Result<()> {
    let candidates = candidates(kind, cwd);
    let programs = candidates
        .iter()
        .map(|candidate| candidate.program)
        .collect::<Vec<_>>()
        .join(", ");
    let mut last_error = None;

    for candidate in candidates {
        match Command::new(candidate.program)
            .args(&candidate.args)
            .current_dir(cwd)
            .spawn()
        {
            Ok(_child) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }

    match last_error {
        Some(error) => bail!("terminalを起動できませんでした ({programs} を試行): {error}"),
        None => bail!("terminalを起動できませんでした (起動候補がありません)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn terminal_kind_maps_to_windows_candidates_in_priority_order() {
        let cwd = Path::new(r"C:\work");
        assert_eq!(
            candidates(TerminalKind::Auto, cwd)
                .iter()
                .map(|candidate| candidate.program)
                .collect::<Vec<_>>(),
            ["wt.exe", "powershell.exe", "cmd.exe"]
        );
        for (kind, program) in [
            (TerminalKind::WindowsTerminal, "wt.exe"),
            (TerminalKind::PowerShell, "powershell.exe"),
            (TerminalKind::Cmd, "cmd.exe"),
        ] {
            let result = candidates(kind, cwd);
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].program, program);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_keeps_space_and_japanese_cwd_in_one_argument() {
        let cwd = Path::new(r"C:\Users\山田 太郎\ドキュメント");
        let result = candidates(TerminalKind::WindowsTerminal, cwd);

        assert_eq!(result[0].args.len(), 2);
        assert_eq!(result[0].args[1], cwd.as_os_str());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_uses_x_terminal_emulator_for_every_kind() {
        let cwd = Path::new("/tmp/山田 太郎/ドキュメント");
        for kind in [
            TerminalKind::Auto,
            TerminalKind::WindowsTerminal,
            TerminalKind::PowerShell,
            TerminalKind::Cmd,
        ] {
            assert_eq!(
                candidates(kind, cwd),
                [CommandSpec {
                    program: "x-terminal-emulator",
                    args: Vec::new(),
                }]
            );
        }
    }
}
