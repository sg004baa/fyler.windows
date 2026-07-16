//! ファイルpicker向けの、エンジン・GUI・ファイルシステム非依存な検索候補型。
//!
//! スコアリング自体はnucleo-matcherを使う`fyler-app`側で行う(nucleoは`fyler-core`の
//! 依存境界=std/anyhow/thiserrorに反するため持ち込めない)。この`fyler-core`型は、
//! `fyler-fsops`のcatalog walkerが生成し`fyler-app`のpicker workerが照合する、
//! クレート間で共有する最小の候補データだけを保持する。

use crate::tree::EntryKind;

/// 検索候補。`display`はソート・照合に使う`/`区切りのルート相対パス。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub kind: EntryKind,
    /// 自身または祖先がhiddenなら`true`。
    pub hidden: bool,
    /// `/` 区切りのルート相対パス。照合のhaystackでもある。
    pub display: Box<str>,
}

impl SearchCandidate {
    pub fn new(display: String, kind: EntryKind, hidden: bool) -> Self {
        Self {
            kind,
            hidden,
            display: display.into_boxed_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_retains_display_kind_and_hidden() {
        let candidate = SearchCandidate::new("Zed/Äpfel.txt".to_owned(), EntryKind::Dir, true);
        assert_eq!(&*candidate.display, "Zed/Äpfel.txt");
        assert_eq!(candidate.kind, EntryKind::Dir);
        assert!(candidate.hidden);
    }
}
