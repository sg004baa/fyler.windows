//! ごみ箱削除。planのDeleteは**必ずここを通る**(直接削除しない)。

use std::path::Path;

use anyhow::Context;

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
