//! "Did you mean" candidates by edit distance.

/// Up to `limit` candidates closest to `name`, nearest first, ties by name.
pub(crate) fn closest<'c>(
    name: &str,
    candidates: impl IntoIterator<Item = &'c str>,
    limit: usize,
) -> Vec<&'c str> {
    let mut scored: Vec<(usize, &str)> = candidates
        .into_iter()
        .filter(|candidate| *candidate != name)
        .map(|candidate| (distance(name, candidate), candidate))
        .collect();
    scored.sort_unstable();
    scored.dedup_by_key(|(_, candidate)| *candidate);
    scored.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// Levenshtein distance over characters.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("same", "same"), 0);
    }

    #[test]
    fn nearest_first_capped_and_deduplicated() {
        let names = [
            "jira_search",
            "jira_get",
            "sum",
            "jira_search",
            "linear_search",
            "a",
            "b",
        ];
        assert_eq!(
            closest("jira_serch", names, 3),
            ["jira_search", "jira_get", "linear_search"]
        );
        assert_eq!(closest("x", ["a", "b", "c", "d", "e", "f"], 5).len(), 5);
    }
}
