//! case-onlyリネーム(`Foo → foo`)対応(DESIGN.md「その他の対応事項」)。

use std::path::Path;

/// ディレクトリが大文字小文字を区別する設定かどうかを返す。
///
/// 実装契約(Windows): Windowsは**ディレクトリ単位**でcase-sensitiveにできる
/// (`FILE_CASE_SENSITIVE_DIR`)。名前衝突の判定は対象ディレクトリの実際の
/// 設定に合わせること(グローバルに大文字小文字無視と決めつけない)。
pub fn dir_is_case_sensitive(dir: &Path) -> anyhow::Result<bool> {
    todo!("M3: FILE_CASE_SENSITIVE_DIR の取得")
}

/// case-onlyリネームを実行する。
///
/// 実装契約:
/// - case-insensitiveなディレクトリでは `Foo → foo` が同名扱いで失敗し得るため、
///   **temp名経由の2段rename**で行う(`Foo → .fyler-tmp-XXXX → foo`)
/// - temp名は同一ディレクトリ内で衝突しない名前を生成する
/// - 1段目成功後に2段目が失敗した場合は1段目を巻き戻す(この操作内のみロールバック)
pub fn case_only_rename(from: &Path, to: &Path) -> anyhow::Result<()> {
    todo!("M3: temp名経由の2段rename")
}
