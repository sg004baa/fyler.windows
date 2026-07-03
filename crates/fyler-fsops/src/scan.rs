//! baselineスキャン: 実FS → BaselineTree(ID採番)。

use std::path::Path;

use fyler_core::id::IdAllocator;
use fyler_core::tree::BaselineTree;

/// ルート以下をスキャンしてBaselineTreeを構築する。
///
/// 実装契約:
/// - IDは `ids` から採番する(セッション内一意。永続化しない)。
///   reconcile時の再スキャンでは、**変化しなかったエントリのIDを維持する**必要が
///   あるため、既存baselineとの突き合わせ版(差分スキャン)もM3で必要になる
/// - symlink / junction / reparse point は**中に潜らず**、リンク自体を
///   `EntryKind::Symlink` の1エントリとして扱う(DESIGN.md「validateで弾くもの」)
/// - OneDriveプレースホルダ([`crate::onedrive`])のhydrationを発生させない
///   (メタデータ列挙のみ。内容・サイズの取得でリモートアクセスを誘発しない)
/// - collapsedなディレクトリの中もbaselineには**含める**(diffのDelete判定と
///   collapsed move追従に必要)。ただし深い階層の遅延スキャンにするかはM1で判断し、
///   遅延にする場合はEditContext/diffの契約と整合させること
pub fn scan_baseline(root: &Path, ids: &mut IdAllocator) -> anyhow::Result<BaselineTree> {
    todo!("M1: ディレクトリ走査とID採番")
}
