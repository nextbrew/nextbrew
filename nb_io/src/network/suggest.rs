use strsim::damerau_levenshtein;

const MIN_SIMILARITY_SCORE: f64 = 0.45;

#[derive(Debug, Clone, PartialEq)]
struct CandidateScore {
    name: String,
    score: f64,
    distance: usize,
    len_delta: usize,
}

pub fn rank_formula_suggestions(query: &str, candidates: &[String], limit: usize) -> Vec<String> {
    rank_formula_suggestions_with(query, candidates, limit, damerau_levenshtein)
}

fn rank_formula_suggestions_with<F>(
    query: &str,
    candidates: &[String],
    limit: usize,
    mut distance_fn: F,
) -> Vec<String>
where
    F: FnMut(&str, &str) -> usize,
{
    if limit == 0 {
        return Vec::new();
    }

    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<CandidateScore> = candidates
        .iter()
        .filter_map(|candidate| {
            let normalized = candidate.trim().to_ascii_lowercase();
            if normalized.is_empty() || !is_plausible_candidate(&query, &normalized) {
                return None;
            }

            let distance = distance_fn(&query, &normalized);
            let max_len = query.len().max(normalized.len());
            let similarity = if max_len == 0 {
                1.0
            } else {
                1.0 - (distance as f64 / max_len as f64)
            };

            let score = similarity;
            if score < MIN_SIMILARITY_SCORE {
                return None;
            }

            Some(CandidateScore {
                name: candidate.clone(),
                score,
                distance,
                len_delta: query.len().abs_diff(normalized.len()),
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.distance.cmp(&b.distance))
            .then_with(|| a.len_delta.cmp(&b.len_delta))
            .then_with(|| a.name.cmp(&b.name))
    });

    scored.into_iter().take(limit).map(|s| s.name).collect()
}

fn is_plausible_candidate(query: &str, candidate: &str) -> bool {
    query.len().abs_diff(candidate.len()) <= max_len_delta(query.len())
}

fn max_len_delta(query_len: usize) -> usize {
    query_len.saturating_mul(2) / 3 + 1
}

/// Ruby's `DidYouMean::SpellChecker#correct`, which `brew search` adds to
/// a plain query's results: words close by Jaro-Winkler, best first, kept
/// if within a quarter of the input's length in edits, or else the single
/// closest one with fewer edits than the shorter of the two has letters.
pub fn did_you_mean<S: AsRef<str>>(input: &str, dictionary: &[S]) -> Vec<String> {
    fn normalize(s: &str) -> Vec<char> {
        s.to_lowercase().chars().filter(|c| *c != '@').collect()
    }
    let wanted = normalize(input);
    let threshold = if wanted.len() > 3 { 0.834 } else { 0.77 };
    let mut words: Vec<(&str, f64)> = dictionary
        .iter()
        .map(AsRef::as_ref)
        .filter(|word| jaro_winkler(&normalize(word), &wanted) >= threshold && *word != input)
        .map(|word| {
            (
                word,
                jaro_winkler(&word.chars().collect::<Vec<_>>(), &wanted),
            )
        })
        .collect();
    // Ruby sorts ascending and reverses; its sort is not stable, so equally
    // close words may come out in another order.
    words.sort_by(|a, b| a.1.total_cmp(&b.1));
    words.reverse();

    let edits = |word: &str| levenshtein(&normalize(word), &wanted);
    let most = (wanted.len() as f64 * 0.25).ceil() as usize;
    let typos: Vec<String> = words
        .iter()
        .filter(|(word, _)| edits(word) <= most)
        .map(|(word, _)| word.to_string())
        .collect();
    if !typos.is_empty() {
        return typos;
    }
    words
        .iter()
        .find(|(word, _)| edits(word) < wanted.len().min(normalize(word).len()))
        .map(|(word, _)| word.to_string())
        .into_iter()
        .collect()
}

/// `DidYouMean::Jaro.distance`, including its match window of zero for
/// words of up to three letters.
fn jaro(a: &[char], b: &[char]) -> f64 {
    let (short, long) = if a.len() > b.len() { (b, a) } else { (a, b) };
    let range = if long.len() > 3 {
        long.len() / 2 - 1
    } else {
        0
    };
    let mut short_flags = vec![false; short.len()];
    let mut long_flags = vec![false; long.len()];
    let mut matches = 0.0;
    for (i, c) in short.iter().enumerate() {
        for j in i.saturating_sub(range)..=(i + range).min(long.len().saturating_sub(1)) {
            if j < long.len() && !long_flags[j] && *c == long[j] {
                long_flags[j] = true;
                short_flags[i] = true;
                matches += 1.0;
                break;
            }
        }
    }
    if matches == 0.0 {
        return 0.0;
    }
    let mut transpositions = 0.0;
    let mut next = 0;
    for (i, c) in short.iter().enumerate() {
        if short_flags[i] {
            let j = (next..long.len())
                .find(|&j| long_flags[j])
                .unwrap_or(long.len());
            next = j + 1;
            if long.get(j) != Some(c) {
                transpositions += 1.0;
            }
        }
    }
    let transpositions = (transpositions / 2.0_f64).floor();
    (matches / short.len() as f64
        + matches / long.len() as f64
        + (matches - transpositions) / matches)
        / 3.0
}

/// `DidYouMean::JaroWinkler.distance`: up to four letters of common prefix
/// raise a Jaro score above 0.7.
fn jaro_winkler(a: &[char], b: &[char]) -> f64 {
    let score = jaro(a, b);
    if score <= 0.7 {
        return score;
    }
    let prefix = a.iter().zip(b).take(4).take_while(|(x, y)| x == y).count();
    score + prefix as f64 * 0.1 * (1.0 - score)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let substitution = diagonal + usize::from(x != y);
            diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(row[j + 1] + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use strsim::damerau_levenshtein;

    use super::{
        did_you_mean, jaro_winkler, max_len_delta, rank_formula_suggestions,
        rank_formula_suggestions_with,
    };

    #[test]
    fn ranks_common_typo_as_top_match() {
        let candidates = vec![
            "python".to_string(),
            "pytest".to_string(),
            "pypy".to_string(),
        ];

        let suggestions = rank_formula_suggestions("pythn", &candidates, 3);
        assert_eq!(suggestions.first().map(String::as_str), Some("python"));
    }

    #[test]
    fn filters_unrelated_candidates() {
        let candidates = vec![
            "wget".to_string(),
            "ripgrep".to_string(),
            "zstd".to_string(),
        ];

        let suggestions = rank_formula_suggestions("completelydifferent", &candidates, 3);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn respects_result_limit() {
        let candidates = vec![
            "git".to_string(),
            "gitea".to_string(),
            "git-lfs".to_string(),
            "glow".to_string(),
        ];

        let suggestions = rank_formula_suggestions("git", &candidates, 2);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn skips_edit_distance_for_implausible_length_deltas() {
        let candidates = vec![
            "git".to_string(),
            "gitea".to_string(),
            "super-long-package-name".to_string(),
            "another-extremely-long-formula-name".to_string(),
        ];
        let distance_calls = Cell::new(0usize);

        let suggestions =
            rank_formula_suggestions_with("git", &candidates, 3, |query, candidate| {
                distance_calls.set(distance_calls.get() + 1);
                damerau_levenshtein(query, candidate)
            });

        assert_eq!(distance_calls.get(), 2);
        assert_eq!(suggestions.first().map(String::as_str), Some("git"));
    }

    #[test]
    fn max_len_delta_scales_with_query_length() {
        assert_eq!(max_len_delta(3), 3);
        assert_eq!(max_len_delta(5), 4);
        assert_eq!(max_len_delta(9), 7);
    }

    #[test]
    fn does_not_keep_short_prefix_only_matches() {
        let candidates = vec![
            "git".to_string(),
            "gnupg".to_string(),
            "ripgrep".to_string(),
        ];

        let suggestions = rank_formula_suggestions("gitignore", &candidates, 3);

        assert!(!suggestions.iter().any(|candidate| candidate == "git"));
    }

    // Expected values are Ruby 4.0's `DidYouMean` on the same input.
    #[test]
    fn did_you_mean_matches_ruby() {
        let dictionary = [
            "ripgrep",
            "ripgrep-all",
            "rg",
            "grep",
            "jq",
            "gojq",
            "node",
            "node@22",
            "nodenv",
            "python@3.13",
            "python3",
            "bpython",
            "ghostty",
            "ghcitty",
            "holst",
            "ghost",
            "dash",
            "bash",
            "bat",
            "bar",
            "zsh",
        ];
        let cases: [(&str, &[&str]); 10] = [
            ("ripgrp", &["ripgrep"]),
            ("jq", &[]),
            ("nodejs", &["node", "nodenv", "node@22"]),
            ("pyhton", &["python3"]),
            ("ghosty", &["ghostty", "ghost"]),
            ("ghost", &["ghostty", "holst"]),
            ("xyz", &[]),
            ("ba", &["bar", "bat"]),
            ("Python@3.13", &["python@3.13", "python3"]),
            ("b", &[]),
        ];
        for (input, expected) in cases {
            assert_eq!(did_you_mean(input, &dictionary), expected, "{input}");
        }
    }

    #[test]
    fn jaro_winkler_matches_ruby() {
        for (a, b, expected) in [
            ("martha", "marhta", 0.961111),
            ("dwayne", "duane", 0.84),
            ("ab", "ba", 0.0),
            ("jq", "j", 0.85),
            ("abcdefgh", "abdcefhg", 0.933333),
        ] {
            let chars = |s: &str| s.chars().collect::<Vec<_>>();
            let score = jaro_winkler(&chars(a), &chars(b));
            assert!((score - expected).abs() < 1e-6, "{a} {b}: {score}");
        }
    }
}
