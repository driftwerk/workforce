//! Sets of message uids, kept and written as IMAP sequence sets
//! (`1:40,52,60:61`), so a job over 300k messages stays a few KB.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Sorted, disjoint, non-adjacent runs of uids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UidSet(Vec<(u32, u32)>);

impl UidSet {
    /// The set of `runs`, in any order, overlapping or not.
    fn of_runs(mut runs: Vec<(u32, u32)>) -> UidSet {
        runs.sort_unstable();
        UidSet(runs.into_iter().fold(Vec::new(), |mut merged, (a, b)| {
            match merged.last_mut() {
                Some((_, end)) if a <= end.saturating_add(1) => *end = (*end).max(b),
                _ => merged.push((a, b)),
            }
            merged
        }))
    }

    pub fn len(&self) -> usize {
        self.0.iter().map(|(a, b)| (b - a) as usize + 1).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.0.iter().flat_map(|&(a, b)| a..=b)
    }

    pub fn contains(&self, uid: u32) -> bool {
        self.0
            .binary_search_by(|&(a, b)| match (b < uid, a > uid) {
                (true, _) => std::cmp::Ordering::Less,
                (_, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            })
            .is_ok()
    }

    /// The first `n` uids, and what is left after them.
    pub fn split_first(&self, n: usize) -> (UidSet, UidSet) {
        let mut left = n;
        let (mut head, mut rest) = (Vec::new(), Vec::new());
        for &(a, b) in &self.0 {
            let size = (b - a) as usize + 1;
            match left {
                0 => rest.push((a, b)),
                _ if size <= left => {
                    head.push((a, b));
                    left -= size;
                }
                _ => {
                    let cut = a + left as u32;
                    head.push((a, cut - 1));
                    rest.push((cut, b));
                    left = 0;
                }
            }
        }
        (UidSet(head), UidSet(rest))
    }

    pub fn without(&self, other: &UidSet) -> UidSet {
        let cut = |(a, b): (u32, u32)| {
            other.0.iter().filter(|&&(x, y)| x <= b && y >= a).fold(
                vec![(a, b)],
                |pieces, &(x, y)| {
                    pieces
                        .into_iter()
                        .flat_map(|(p, q)| {
                            let below = (p < x).then(|| (p, q.min(x - 1)));
                            let above = (q > y).then(|| (p.max(y + 1), q));
                            below.into_iter().chain(above)
                        })
                        .collect()
                },
            )
        };
        UidSet(self.0.iter().copied().flat_map(cut).collect())
    }

    pub fn intersection(&self, other: &UidSet) -> UidSet {
        self.without(&self.without(other))
    }

    pub fn union(&self, other: &UidSet) -> UidSet {
        UidSet::of_runs(self.0.iter().chain(&other.0).copied().collect())
    }

    pub fn parse(text: &str) -> Result<UidSet, String> {
        let bad = || format!("'{text}' is not a uid set");
        text.split(',')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let uid = |s: &str| s.parse::<u32>().ok().filter(|u| *u > 0).ok_or_else(bad);
                match part.split_once(':') {
                    Some((a, b)) => {
                        let (a, b) = (uid(a)?, uid(b)?);
                        Ok((a.min(b), a.max(b)))
                    }
                    None => uid(part).map(|u| (u, u)),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(UidSet::of_runs)
    }
}

impl FromIterator<u32> for UidSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        UidSet::of_runs(
            iter.into_iter()
                .filter(|u| *u > 0)
                .map(|u| (u, u))
                .collect(),
        )
    }
}

impl fmt::Display for UidSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self
            .0
            .iter()
            .map(|(a, b)| match a == b {
                true => a.to_string(),
                false => format!("{a}:{b}"),
            })
            .collect::<Vec<_>>()
            .join(",");
        f.write_str(&text)
    }
}

impl Serialize for UidSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for UidSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        UidSet::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_is_written_as_runs_and_read_back() {
        let set: UidSet = [1, 2, 3, 4, 7, 9, 10].into_iter().collect();
        assert_eq!(set.to_string(), "1:4,7,9:10");
        assert_eq!(UidSet::parse("1:4,7,9:10").unwrap(), set);
        assert_eq!(UidSet::parse("10:9").unwrap().to_string(), "9:10");
        assert_eq!(UidSet::parse("5:8,1:6,9").unwrap().to_string(), "1:9");
    }

    #[test]
    fn three_hundred_thousand_contiguous_uids_stay_one_run() {
        let set: UidSet = (1..=300_000).collect();
        assert_eq!(set.to_string(), "1:300000");
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(serde_json::from_str::<UidSet>(&json).unwrap(), set);
    }

    #[test]
    fn the_widest_set_is_never_expanded() {
        let all = UidSet::parse("1:4294967295").unwrap();
        assert_eq!(all.len(), u32::MAX as usize);
        assert!(all.contains(u32::MAX));
        let (head, rest) = all.split_first(500);
        assert_eq!(head.to_string(), "1:500");
        assert_eq!(rest.to_string(), "501:4294967295");
        assert_eq!(all.iter().next_back(), Some(u32::MAX));
    }

    #[test]
    fn nothing_but_positive_numbers_is_a_uid_set() {
        assert!(UidSet::parse("1,x").is_err());
        assert!(UidSet::parse("0").is_err());
        assert!(UidSet::parse("1:*").is_err());
        assert!(UidSet::parse("").unwrap().is_empty());
    }

    #[test]
    fn a_set_splits_subtracts_and_meets_another() {
        let set: UidSet = (1..=10).collect();
        let (head, rest) = set.split_first(4);
        assert_eq!(head.to_string(), "1:4");
        assert_eq!(rest.to_string(), "5:10");
        assert_eq!(set.without(&head), rest);
        let holes = UidSet::parse("3,5:6,20").unwrap();
        assert_eq!(set.without(&holes).to_string(), "1:2,4,7:10");
        assert_eq!(set.intersection(&holes).to_string(), "3,5:6");
        assert_eq!(set.union(&holes).to_string(), "1:10,20");
        assert!(set.contains(10) && !set.contains(11) && !holes.contains(4));
        assert_eq!(set.split_first(20), (set.clone(), UidSet::default()));
    }
}
