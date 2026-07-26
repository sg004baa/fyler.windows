//! ごみ箱削除・復元。planのDeleteは**必ずここを通る**(直接削除しない)。

use std::path::Path;

use anyhow::{Context, anyhow};
use trash::TrashItem;

/// ファイル/ディレクトリをごみ箱へ移動する。
///
/// 実装契約:
/// - 初期実装は `trash` クレートでよい
/// - IFileOperation COM APIへ置き換える場合は**専用のCOM STAスレッド**が必要
///   (tokioのワーカースレッドに直接投げられない。DESIGN.md「その他の対応事項」)。
///   置き換え時はこの関数のシグネチャは変えず内部実装だけ差し替えること
/// - `trash`クレートによる拡張形式パスの受け入れは未検証。MAX_PATH超の
///   ごみ箱削除はWindows実機で要検証(M7残件)
pub fn delete_to_recycle_bin(path: &Path) -> anyhow::Result<()> {
    trash::delete(path)
        .with_context(|| format!("Failed to move to recycle bin: {}", path.display()))
}

/// ごみ箱から `original_path` に一致するitemを復元する(複数あれば最新の削除時刻を選ぶ)。
///
/// 復元は既存エントリの移動(Windows: `IFileOperation::MoveItem`)であり、
/// symlink/junctionでもリンク作成用の特権を一切要しない。itemが見つからない
/// (ごみ箱が空にされた等)場合や、復元先が既に占有されている場合は明確なErrを返す。
pub fn restore_from_recycle_bin(original_path: &Path) -> anyhow::Result<()> {
    let items = trash::os_limited::list().context("Failed to list the recycle bin")?;
    let item = select_restore_candidate(&items, original_path)
        .ok_or_else(|| {
            anyhow!(
                "\"{}\" was not found in the recycle bin (it may have been emptied)",
                original_path.display()
            )
        })?
        .clone();
    trash::os_limited::restore_all([item]).map_err(|error| match error {
        trash::Error::RestoreCollision { path, .. } => anyhow!(
            "Failed to restore \"{}\" from the recycle bin: the restore destination is already occupied ({})",
            original_path.display(),
            path.display()
        ),
        other => anyhow::Error::new(other).context(format!(
            "Failed to restore \"{}\" from the recycle bin",
            original_path.display()
        )),
    })
}

/// `original_path` に一致するitemがごみ箱に存在するかを確認する(preflight用)。
///
/// `list()` 自体が失敗した場合はErrを返す。検証を助言に留めたい呼び出し側
/// (undoのvalidate)がErrを「未確認」として扱い、実行時
/// ([`restore_from_recycle_bin`])のfail fastに委ねる。
pub(crate) fn has_restore_candidate(original_path: &Path) -> anyhow::Result<bool> {
    let items = trash::os_limited::list().context("Failed to list the recycle bin")?;
    Ok(select_restore_candidate(&items, original_path).is_some())
}

/// `items` のうち `original_path` に一致する最新のものを選ぶ純関数。
///
/// 削除時に拡張形式(`\\?\`)パスで `trash::delete` された可能性があるため、
/// 両辺を [`crate::long_path::from_fs`] で表示形式へ正規化してから比較する。
/// 複数マッチした場合は `time_deleted` が最大のものを返す。
fn select_restore_candidate<'a>(
    items: &'a [TrashItem],
    original_path: &Path,
) -> Option<&'a TrashItem> {
    let target = crate::long_path::from_fs(original_path);
    items
        .iter()
        .filter(|item| crate::long_path::from_fs(&item.original_path()) == target)
        .max_by_key(|item| item.time_deleted)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;

    fn item(parent: &str, name: &str, time_deleted: i64) -> TrashItem {
        TrashItem {
            id: OsString::from(format!("{parent}/{name}#{time_deleted}")),
            name: OsString::from(name),
            original_parent: PathBuf::from(parent),
            time_deleted,
        }
    }

    #[test]
    fn select_restore_candidate_returns_none_without_a_match() {
        let items = vec![item("/tmp/a", "foo", 1)];
        assert!(select_restore_candidate(&items, Path::new("/tmp/b/foo")).is_none());
    }

    #[test]
    fn select_restore_candidate_returns_the_sole_match() {
        let items = vec![item("/tmp/a", "foo", 1), item("/tmp/a", "bar", 2)];
        let found = select_restore_candidate(&items, Path::new("/tmp/a/foo")).unwrap();
        assert_eq!(found.name, OsString::from("foo"));
        assert_eq!(found.time_deleted, 1);
    }

    #[test]
    fn select_restore_candidate_picks_the_latest_time_deleted() {
        let items = vec![
            item("/tmp/a", "foo", 1),
            item("/tmp/a", "foo", 5),
            item("/tmp/a", "foo", 3),
        ];
        let found = select_restore_candidate(&items, Path::new("/tmp/a/foo")).unwrap();
        assert_eq!(found.time_deleted, 5);
    }
}
