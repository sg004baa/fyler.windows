//! baseline scanで回復可能な部分失敗を共有するエンジン非依存型。

use std::fmt;
use std::path::PathBuf;

/// scan失敗の分類。OS固有の`io::ErrorKind`からの変換はfsops層が担う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanErrorKind {
    PermissionDenied,
    NotFound,
    TimedOut,
    NonUnicodeName,
    Other(String),
}

impl fmt::Display for ScanErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => write!(f, "permission denied"),
            Self::NotFound => write!(f, "not found"),
            Self::TimedOut => write!(f, "timed out"),
            Self::NonUnicodeName => write!(f, "file name is not valid Unicode"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

/// 列挙処理のどの段階で失敗したか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStage {
    EnumerateDir,
    DirEntry,
    Metadata,
    Name,
}

impl fmt::Display for ScanStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stage = match self {
            Self::EnumerateDir => "enumerating directory",
            Self::DirEntry => "reading directory entry",
            Self::Metadata => "reading metadata",
            Self::Name => "reading file name",
        };
        write!(f, "{stage}")
    }
}

/// scan全体を中断しなかった部分失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanWarning {
    /// UI表示用の絶対パス。非Unicode名はlossy表現でよい。
    pub path: PathBuf,
    pub stage: ScanStage,
    pub kind: ScanErrorKind,
}

impl fmt::Display for ScanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed while {} at {}: {}",
            self.stage,
            self.path.display(),
            self.kind
        )
    }
}
