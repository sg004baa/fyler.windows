//! ファイルpicker向けの、エンジン・GUI・ファイルシステム非依存検索。

use crate::tree::EntryKind;

/// 検索時に毎回変換しない情報をキャッシュした候補。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub kind: EntryKind,
    /// 自身または祖先がhiddenなら`true`。
    pub hidden: bool,
    /// `/` 区切りのルート相対パス。
    pub display: Box<str>,
    /// [`SearchCandidate::display`] をUnicode小文字化した検索key。
    pub key: Box<str>,
    /// [`SearchCandidate::key`] 上のbasename開始バイトオフセット。
    pub name_offset: usize,
}

impl SearchCandidate {
    /// 表示用相対パスから、検索時に再利用する小文字keyを一度だけ構築する。
    pub fn new(display: String, kind: EntryKind, hidden: bool) -> Self {
        let key = display.to_lowercase().into_boxed_str();
        let name_offset = key.rfind('/').map_or(0, |offset| offset + 1);
        Self {
            kind,
            hidden,
            display: display.into_boxed_str(),
            key,
            name_offset,
        }
    }
}

/// 検索結果。`index`は入力候補slice上の位置である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit {
    pub index: usize,
    pub score: u32,
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
pub fn search(
    candidates: &[SearchCandidate],
    query: &str,
    limit: usize,
    include_hidden: bool,
) -> Vec<SearchHit> {
    search_refs(candidates.iter(), query, limit, include_hidden)
        .into_iter()
        .map(|hit| SearchHit {
            index: hit.index,
            score: hit.score,
        })
        .collect()
}

/// 複数の共有chunkを連結したiteratorなどを、候補cloneなしで検索する。
/// `index`はiterator上の位置で、同点時はその挿入順を維持する。
pub fn search_refs<'a>(
    candidates: impl IntoIterator<Item = &'a SearchCandidate>,
    query: &str,
    limit: usize,
    include_hidden: bool,
) -> Vec<SearchRefHit<'a>> {
    if limit == 0 {
        return Vec::new();
    }

    let tokens = expand_query(query);
    if tokens.is_empty() {
        return candidates
            .into_iter()
            .enumerate()
            .filter(|(_, candidate)| include_hidden || !candidate.hidden)
            .take(limit)
            .map(|(index, candidate)| SearchRefHit {
                index,
                score: 0,
                candidate,
            })
            .collect();
    }

    let mut hits = candidates
        .into_iter()
        .enumerate()
        .filter(|(_, candidate)| include_hidden || !candidate.hidden)
        .filter_map(|(index, candidate)| {
            score_candidate(candidate, &tokens).map(|score| SearchRefHit {
                index,
                score,
                candidate,
            })
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

#[derive(Debug, Clone, Copy)]
pub struct SearchRefHit<'a> {
    pub index: usize,
    pub score: u32,
    pub candidate: &'a SearchCandidate,
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

    use super::*;

    fn candidate(path: &str) -> SearchCandidate {
        SearchCandidate::new(path.to_owned(), EntryKind::File, false)
    }

    #[test]
    fn score_categories_follow_the_picker_contract() {
        let candidates = [
            candidate("docs/foo"),
            candidate("docs/foobar.txt"),
            candidate("docs/afoo.txt"),
            candidate("foo-dir/file.txt"),
            candidate("far/other/output.txt"),
        ];

        let hits = search(&candidates, "foo", candidates.len(), true);

        assert_eq!(
            hits.iter().map(|hit| hit.index).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(hits.windows(2).all(|pair| pair[0].score > pair[1].score));
    }

    #[test]
    fn continuous_matches_and_earlier_starts_receive_bonuses() {
        let continuity = [candidate("ab--c.txt"), candidate("a-b-c.txt")];
        let hits = search(&continuity, "abc", 2, true);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [0, 1]);
        assert!(hits[0].score > hits[1].score);

        let start = [candidate("xaxbxc.txt"), candidate("axbxc.txt")];
        let hits = search(&start, "abc", 2, true);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [1, 0]);
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn search_is_case_insensitive_and_all_tokens_must_match() {
        let candidates = [
            candidate("src/main.rs"),
            candidate("src/lib.rs"),
            candidate("tests/main.rs"),
            candidate("Foo.txt"),
        ];

        assert_eq!(search(&candidates, "src main", 10, true)[0].index, 0);
        assert_eq!(search(&candidates, "src main", 10, true).len(), 1);
        assert_eq!(search(&candidates, "Foo", 10, true)[0].index, 3);
        assert!(search(&candidates, "missing", 10, true).is_empty());
        assert_eq!(expand_query("  SRC\tMain  "), ["src", "main"]);
    }

    #[test]
    fn empty_query_limit_zero_and_equal_scores_preserve_input_order() {
        let candidates = [
            candidate("same.txt"),
            candidate("same.txt"),
            candidate("other.txt"),
        ];

        assert_eq!(
            search(&candidates, "  ", 2, true)
                .iter()
                .map(|hit| hit.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(search(&candidates, "same", 0, true).is_empty());
        assert_eq!(
            search(&candidates, "same", 10, true)
                .iter()
                .map(|hit| hit.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn unicode_names_are_lowercased_and_searchable() {
        let candidates = [candidate("資料/設計書.txt"), candidate("MÜNCHEN/Äpfel.txt")];

        assert_eq!(search(&candidates, "設計", 10, true)[0].index, 0);
        assert_eq!(search(&candidates, "münchen äPF", 10, true)[0].index, 1);
    }

    #[test]
    fn constructor_caches_compact_search_fields() {
        let candidate = SearchCandidate::new("Zed/Äpfel.txt".to_owned(), EntryKind::Dir, true);
        assert_eq!(&*candidate.display, "Zed/Äpfel.txt");
        assert_eq!(&*candidate.key, "zed/äpfel.txt");
        assert_eq!(&candidate.key[candidate.name_offset..], "äpfel.txt");
        assert_eq!(candidate.kind, EntryKind::Dir);
        assert!(candidate.hidden);
    }

    #[test]
    fn search_filters_hidden_candidates_when_requested() {
        let candidates = [
            SearchCandidate::new("visible.txt".to_owned(), EntryKind::File, false),
            SearchCandidate::new(".hidden.txt".to_owned(), EntryKind::File, true),
        ];
        assert_eq!(search(&candidates, "", 10, false).len(), 1);
        assert_eq!(search(&candidates, "", 10, true).len(), 2);
        assert!(search(&candidates, "hidden", 10, false).is_empty());
    }

    #[test]
    #[ignore = "environment-dependent performance measurement with 50k candidates"]
    fn search_fifty_thousand_candidates_within_a_relaxed_limit() {
        let candidates = (0..50_000)
            .map(|index| candidate(&format!("src/item_{index:05}.txt")))
            .collect::<Vec<_>>();
        let started = Instant::now();

        let hits = search(&candidates, "item_49999", 100, true);

        let elapsed = started.elapsed();
        eprintln!("50k candidate search elapsed: {elapsed:?}");
        assert_eq!(hits.len(), 1);
        assert!(elapsed <= Duration::from_secs(1), "elapsed: {elapsed:?}");
    }
}
