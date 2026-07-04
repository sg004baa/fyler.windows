//! case-onlyリネーム(`Foo → foo`)対応(DESIGN.md「その他の対応事項」)。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};

static TEMP_NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// ディレクトリが大文字小文字を区別する設定かどうかを返す。
///
/// 実装契約(Windows): Windowsは**ディレクトリ単位**でcase-sensitiveにできる
/// (`FILE_CASE_SENSITIVE_DIR`)。名前衝突の判定は対象ディレクトリの実際の
/// 設定に合わせること(グローバルに大文字小文字無視と決めつけない)。
///
/// 現在のWindows実装はWin32属性取得を導入するまでの保守的なフォールバックとして
/// case-insensitiveを返す。非Windowsでは通常のファイルシステム既定に合わせて
/// case-sensitiveを返す。
pub fn dir_is_case_sensitive(_dir: &Path) -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        Ok(false)
    }

    #[cfg(not(windows))]
    {
        Ok(true)
    }
}

/// case-onlyリネームを実行する。
///
/// 実装契約:
/// - case-insensitiveなディレクトリでは `Foo → foo` が同名扱いで失敗し得るため、
///   **temp名経由の2段rename**で行う(`Foo → .fyler-tmp-XXXX → foo`)
/// - temp名は同一ディレクトリ内で衝突しない名前を生成する
/// - 1段目成功後に2段目が失敗した場合は1段目を巻き戻す(この操作内のみロールバック)
pub fn case_only_rename(from: &Path, to: &Path) -> anyhow::Result<()> {
    let parent = from
        .parent()
        .context("case-only renameの移動元に親ディレクトリがありません")?;
    if to.parent() != Some(parent) {
        bail!("case-only renameの移動元と移動先の親ディレクトリが異なります");
    }

    let temporary = unique_temporary_path(parent);
    fs::rename(from, &temporary).with_context(|| {
        format!(
            "case-only renameの一段目に失敗しました: {} → {}",
            from.display(),
            temporary.display()
        )
    })?;

    if let Err(rename_error) = fs::rename(&temporary, to) {
        return match fs::rename(&temporary, from) {
            Ok(()) => Err(anyhow::anyhow!(
                "case-only renameの二段目に失敗し、元へ戻しました: {} → {}: {rename_error}",
                temporary.display(),
                to.display()
            )),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "case-only renameの二段目と巻き戻しに失敗しました: {} → {}: \
                 {rename_error}; {} → {}: {rollback_error}",
                temporary.display(),
                to.display(),
                temporary.display(),
                from.display()
            )),
        };
    }

    Ok(())
}

fn unique_temporary_path(parent: &Path) -> PathBuf {
    loop {
        let sequence = TEMP_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".fyler-tmp-{}-{sequence}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}
