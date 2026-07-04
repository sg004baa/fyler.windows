//! OneDriveプレースホルダ対応(DESIGN.md「その他の対応事項」)。M5。

use std::path::Path;

/// クラウドプレースホルダ(中身がローカルにないファイル)を示す属性。
/// このファイルのデータを読むとhydration(リモート取得)が発生する。
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// パスがクラウドプレースホルダかどうか。
///
/// 実装契約:
/// - 属性取得のみで判定する(データを読まない)
/// - サイズ取得・プレビュー・ハッシュ等、**内容に触れる処理の前に必ずこれを確認**し、
///   プレースホルダに対しては不要なhydrationを発生させない
pub fn is_cloud_placeholder(_path: &Path) -> anyhow::Result<bool> {
    todo!("M5: GetFileAttributesW による属性確認")
}
