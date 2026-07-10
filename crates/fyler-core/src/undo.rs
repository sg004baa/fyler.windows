//! apply後undoのreceipt型。

use std::path::PathBuf;
use std::time::SystemTime;

use crate::tree::EntryKind;

/// volume + file の実体識別子。
///
/// Windowsでは `VolumeSerialNumber` と `FILE_ID_INFO` の128bit ID、unixでは
/// `dev` と `ino` を格納する。同じファイル実体が同一ボリューム内でrenameされても
/// 変わらないことをundoのstale検知に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// ファイルシステム上のボリューム識別子。
    pub volume: u64,
    /// ボリューム内でのファイル実体識別子。
    pub file: u128,
}

/// 内容を読まずに採取する属性fingerprint。
///
/// 内容hashは取らない。OneDriveなどのplaceholderをhydrateしないため、Fileは
/// size/mtime、Dirは両方None、Symlinkはリンク先パスだけを照合材料にする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// 採取時点のエントリ種別。
    pub kind: EntryKind,
    /// Fileのみのサイズ。DirとSymlinkではNone。
    pub size: Option<u64>,
    /// Fileのみの更新時刻。DirとSymlinkではNone。
    pub mtime: Option<SystemTime>,
    /// Symlinkのみのリンク先パス。FileとDirではNone。
    pub link_target: Option<PathBuf>,
}

/// ディレクトリ子孫のmanifest。
///
/// Copyしたディレクトリをundoで消してよいかを判定するための記録。対象は
/// ディレクトリ自身ではなく子孫で、symlink配下へは潜らない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// ディレクトリからの相対パス。区切りはOSに関わらず `/`。
    pub rel_path: String,
    /// 採取時点のエントリ種別。
    pub kind: EntryKind,
    /// File/Symlinkのサイズ。DirではNone。
    pub size: Option<u64>,
    /// File/Symlinkの更新時刻。DirではNone。
    pub mtime: Option<SystemTime>,
}

/// forward apply の成功1操作に対応する逆操作記述。
///
/// パスはすべて絶対パスとして記録する。`steps` は実行順で保持し、undo実行時は
/// 後ろから処理する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoStep {
    /// Create の逆操作。作成物が未変更ならごみ箱へ移動する。
    RemoveCreated {
        /// 作成されたエントリの絶対パス。
        path: PathBuf,
        /// post-op時点の実体識別子。採取できなかった場合はNone。
        identity: Option<FileIdentity>,
        /// post-op時点の属性fingerprint。
        post: Fingerprint,
    },
    /// Copy の逆操作。copy先が未変更ならごみ箱へ移動する。
    RemoveCopied {
        /// copy先エントリの絶対パス。
        path: PathBuf,
        /// post-op時点の実体識別子。採取できなかった場合はNone。
        identity: Option<FileIdentity>,
        /// post-op時点の属性fingerprint。
        post: Fingerprint,
        /// copy先がDirの場合の子孫manifest。採取できなかった場合はNone。
        manifest: Option<Vec<ManifestEntry>>,
    },
    /// Move/Rename の逆操作。`to` の実体が同一なら `from` へ戻す。
    MoveBack {
        /// forward apply前の絶対パス。
        from: PathBuf,
        /// forward apply後の絶対パス。
        to: PathBuf,
        /// post-op時点で `to` から採取した実体識別子。採取できなかった場合はNone。
        identity: Option<FileIdentity>,
        /// post-op時点で `to` から採取した属性fingerprint。
        post: Fingerprint,
        /// 同一親内の大文字小文字だけのrenameとして計画されたかどうか。
        case_only: bool,
    },
    /// Delete の逆操作。backup payloadを元pathへ復元する。
    RestoreDeleted {
        /// forward applyで削除したエントリの絶対パス。
        path: PathBuf,
        /// transaction dir内のbackup payload参照。
        backup: BackupRef,
    },
    /// overwrite退避の逆操作。上書き前backupをtargetへ復元する。
    RestoreOverwritten {
        /// forward applyで上書き退避したtargetの絶対パス。
        path: PathBuf,
        /// transaction dir内のbackup payload参照。
        backup: BackupRef,
    },
}

/// transaction dir 内の payload 相対パス参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRef {
    /// transaction dirからの相対パス。区切りはOSに関わらず `/`。
    pub payload_rel: String,
    /// backup payloadのエントリ種別。
    pub kind: EntryKind,
}

/// forward apply 1回分のundo receipt。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoTransaction {
    /// transaction識別子。形式はapp層で `{unix_ms}-{pid}-{seq}` として採番する。
    pub id: String,
    /// 表示用のルート絶対パス。
    pub root: PathBuf,
    /// forward applyで実際に成功した副作用に対応するstep列。実行順で保持する。
    pub steps: Vec<UndoStep>,
    /// backup payloadを持つtransaction dir。backup未使用ならNone。
    pub backup_dir: Option<PathBuf>,
}

/// undo preflightのstep単位表示結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoStepStatus {
    /// stale検知を通過し、undo実行可能なstep。
    Ready,
    /// staleまたは占有などの理由でundo実行を拒否するstep。
    Rejected {
        /// ユーザーへ表示する拒否理由。
        reason: String,
    },
}
