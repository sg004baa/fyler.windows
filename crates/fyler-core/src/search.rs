//! ファイルpicker向けの、エンジン・GUI・ファイルシステム非依存検索。

use crate::id::EntryId;
use crate::path::TreePath;
use crate::tree::{BaselineTree, EntryKind};

/// 検索時に毎回変換しない情報をキャッシュした候補。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub id: EntryId,
    pub path: TreePath,
    pub kind: EntryKind,
    /// `/` 区切りのルート相対パス。
    pub display: String,
    /// [`SearchCandidate::display`] をUnicode小文字化した検索key。
    pub key: String,
    /// [`SearchCandidate::key`] 上のbasename開始バイトオフセット。
    pub name_offset: usize,
}

/// 検索結果。`index`は入力候補slice上の位置である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit {
    pub index: usize,
    pub score: u32,
}

/// baselineの表示順を維持して検索候補を構築する。
///
/// hidden設定はbaselineスキャン時に反映済みなので、この関数では再判定しない。
pub fn build_candidates(baseline: &BaselineTree) -> Vec<SearchCandidate> {
    baseline
        .entries()
        .iter()
        .map(|entry| {
            let display = entry.path.to_string();
            let key = display.to_lowercase();
            let name_offset = key.rfind('/').map_or(0, |offset| offset + 1);
            SearchCandidate {
                id: entry.id,
                path: entry.path.clone(),
                kind: entry.kind,
                display,
                key,
                name_offset,
            }
        })
        .collect()
}

/// queryを空白区切りtokenへ展開する。
///
/// 現状は各tokenのUnicode小文字化だけを行う。migemo対応時は、この関数を
/// tokenから複数検索パターンへの展開境界として差し替える。
pub fn expand_query(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// 全tokenがsubsequence一致する候補をscore順に返す。
///
/// score降順、同点は入力候補順で安定させ、最大`limit`件に制限する。空queryは
/// score 0のまま入力先頭から返す。
pub fn search(candidates: &[SearchCandidate], query: &str, limit: usize) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }

    let tokens = expand_query(query);
    if tokens.is_empty() {
        return (0..candidates.len().min(limit))
            .map(|index| SearchHit { index, score: 0 })
            .collect();
    }

    let mut hits = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            score_candidate(candidate, &tokens).map(|score| SearchHit { index, score })
        })
        .collect::<Vec<_>>();
    hits.sort_unstable_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.index.cmp(&right.index))
    });
    hits.truncate(limit);
    hits
}

fn score_candidate(candidate: &SearchCandidate, tokens: &[String]) -> Option<u32> {
    tokens.iter().try_fold(0_u32, |total, token| {
        score_token(candidate, token).map(|score| total.saturating_add(score))
    })
}

fn score_token(candidate: &SearchCandidate, token: &str) -> Option<u32> {
    const EXACT: u32 = 1_000_000;
    const PREFIX: u32 = 800_000;
    const PARTIAL: u32 = 600_000;
    const SEGMENT: u32 = 400_000;
    const SUBSEQUENCE: u32 = 100_000;

    let positions = subsequence_positions(&candidate.key, token)?;
    let basename = &candidate.key[candidate.name_offset..];
    let (class_score, start, contiguous) = if basename == token {
        (EXACT, candidate.name_offset, token.chars().count())
    } else if basename.starts_with(token) {
        (PREFIX, candidate.name_offset, token.chars().count())
    } else if let Some(offset) = basename.find(token) {
        (
            PARTIAL,
            candidate.name_offset + offset,
            token.chars().count(),
        )
    } else if let Some(offset) = segment_boundary_match(&candidate.key, token) {
        (SEGMENT, offset, token.chars().count())
    } else {
        let start = positions.first().map_or(0, |(offset, _)| *offset);
        (SUBSEQUENCE, start, longest_contiguous_run(&positions))
    };

    // 分類間の20万点差を侵食しない範囲で、連続一致と開始の早さを加点する。
    let contiguous_bonus = u32::try_from(contiguous)
        .unwrap_or(u32::MAX)
        .saturating_mul(1_000)
        .min(50_000);
    let early_bonus =
        10_000_u32.saturating_sub(u32::try_from(start).unwrap_or(u32::MAX).min(10_000));
    Some(class_score + contiguous_bonus + early_bonus)
}

fn segment_boundary_match(key: &str, token: &str) -> Option<usize> {
    key.match_indices(token)
        .map(|(offset, _)| offset)
        .find(|offset| *offset == 0 || key.as_bytes().get(offset - 1) == Some(&b'/'))
}

/// 一番左の一致を選ぶsubsequence照合。戻り値は各一致文字の
/// `(バイトオフセット, UTF-8バイト長)`。
fn subsequence_positions(key: &str, token: &str) -> Option<Vec<(usize, usize)>> {
    let mut positions = Vec::with_capacity(token.chars().count());
    let mut cursor = 0;
    for expected in token.chars() {
        let (relative, actual) = key[cursor..]
            .char_indices()
            .find(|(_, actual)| *actual == expected)?;
        let offset = cursor + relative;
        positions.push((offset, actual.len_utf8()));
        cursor = offset + actual.len_utf8();
    }
    Some(positions)
}

fn longest_contiguous_run(positions: &[(usize, usize)]) -> usize {
    let mut longest = 0;
    let mut current = 0;
    let mut previous_end = None;
    for (offset, len) in positions {
        current = if previous_end == Some(*offset) {
            current + 1
        } else {
            1
        };
        longest = longest.max(current);
        previous_end = Some(offset + len);
    }
    longest
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::tree::BaselineEntry;

    use super::*;

    fn candidate(index: u64, path: &str) -> SearchCandidate {
        let mut baseline = BaselineTree::new("C:/root");
        baseline.insert(BaselineEntry {
            id: EntryId(index),
            path: TreePath::parse(path),
            kind: EntryKind::File,
        });
        build_candidates(&baseline).remove(0)
    }

    #[test]
    fn score_categories_follow_the_picker_contract() {
        let candidates = [
            candidate(1, "docs/foo"),
            candidate(2, "docs/foobar.txt"),
            candidate(3, "docs/afoo.txt"),
            candidate(4, "foo-dir/file.txt"),
            candidate(5, "far/other/output.txt"),
        ];

        let hits = search(&candidates, "foo", candidates.len());

        assert_eq!(
            hits.iter().map(|hit| hit.index).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(hits.windows(2).all(|pair| pair[0].score > pair[1].score));
    }

    #[test]
    fn continuous_matches_and_earlier_starts_receive_bonuses() {
        let continuity = [candidate(1, "ab--c.txt"), candidate(2, "a-b-c.txt")];
        let hits = search(&continuity, "abc", 2);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [0, 1]);
        assert!(hits[0].score > hits[1].score);

        let start = [candidate(3, "xaxbxc.txt"), candidate(4, "axbxc.txt")];
        let hits = search(&start, "abc", 2);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [1, 0]);
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn search_is_case_insensitive_and_all_tokens_must_match() {
        let candidates = [
            candidate(1, "src/main.rs"),
            candidate(2, "src/lib.rs"),
            candidate(3, "tests/main.rs"),
            candidate(4, "Foo.txt"),
        ];

        assert_eq!(search(&candidates, "src main", 10)[0].index, 0);
        assert_eq!(search(&candidates, "src main", 10).len(), 1);
        assert_eq!(search(&candidates, "Foo", 10)[0].index, 3);
        assert!(search(&candidates, "missing", 10).is_empty());
        assert_eq!(expand_query("  SRC\tMain  "), ["src", "main"]);
    }

    #[test]
    fn empty_query_limit_zero_and_equal_scores_preserve_input_order() {
        let candidates = [
            candidate(1, "same.txt"),
            candidate(2, "same.txt"),
            candidate(3, "other.txt"),
        ];

        assert_eq!(
            search(&candidates, "  ", 2)
                .iter()
                .map(|hit| hit.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(search(&candidates, "same", 0).is_empty());
        assert_eq!(
            search(&candidates, "same", 10)
                .iter()
                .map(|hit| hit.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn unicode_names_are_lowercased_and_searchable() {
        let candidates = [
            candidate(1, "資料/設計書.txt"),
            candidate(2, "MÜNCHEN/Äpfel.txt"),
        ];

        assert_eq!(search(&candidates, "設計", 10)[0].index, 0);
        assert_eq!(search(&candidates, "münchen äPF", 10)[0].index, 1);
    }

    #[test]
    fn build_candidates_preserves_baseline_order_and_caches_fields() {
        let mut baseline = BaselineTree::new("C:/root");
        baseline.insert(BaselineEntry {
            id: EntryId(7),
            path: TreePath::parse("Zed/Äpfel.txt"),
            kind: EntryKind::File,
        });
        baseline.insert(BaselineEntry {
            id: EntryId(8),
            path: TreePath::parse("alpha"),
            kind: EntryKind::Dir,
        });

        let candidates = build_candidates(&baseline);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id)
                .collect::<Vec<_>>(),
            [EntryId(7), EntryId(8)]
        );
        assert_eq!(candidates[0].display, "Zed/Äpfel.txt");
        assert_eq!(candidates[0].key, "zed/äpfel.txt");
        assert_eq!(&candidates[0].key[candidates[0].name_offset..], "äpfel.txt");
        assert_eq!(candidates[1].kind, EntryKind::Dir);
    }

    #[test]
    fn build_candidates_only_contains_entries_present_in_baseline() {
        let visible = candidate(1, "visible.txt");
        assert_eq!(visible.display, "visible.txt");
        assert_ne!(visible.display, ".hidden.txt");
    }

    #[test]
    #[ignore = "50k候補の環境依存性能計測"]
    fn search_fifty_thousand_candidates_within_a_relaxed_limit() {
        let candidates = (0..50_000)
            .map(|index| candidate(index + 1, &format!("src/item_{index:05}.txt")))
            .collect::<Vec<_>>();
        let started = Instant::now();

        let hits = search(&candidates, "item_49999", 100);

        let elapsed = started.elapsed();
        eprintln!("50k candidate search elapsed: {elapsed:?}");
        assert_eq!(hits.len(), 1);
        assert!(elapsed <= Duration::from_secs(1), "elapsed: {elapsed:?}");
    }
}
