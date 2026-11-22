//! Sets of message uids, kept and written as IMAP sequence sets
//! (`1:40,52,60:61`), so a job over 300k messages stays a few KB.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UidSet(BTreeSet<u32>);

impl UidSet {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.0.iter().copied()
    }

    pub fn contains(&self, uid: u32) -> bool {
        self.0.contains(&uid)
    }

    /// The first `n` uids, and what is left after them.
    pub fn split_first(&self, n: usize) -> (UidSet, UidSet) {
        let head: UidSet = self.iter().take(n).collect();
        let rest = self.iter().skip(n).collect();
        (head, rest)
    }

    pub fn without(&self, other: &UidSet) -> UidSet {
        UidSet(self.0.difference(&other.0).copied().collect())
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
                        Ok(a.min(b)..=a.max(b))
                    }
                    None => uid(part).map(|u| u..=u),
                }
            })
            .try_fold(BTreeSet::new(), |mut set, range| {
                set.extend(range?);
                Ok(set)
            })
            .map(UidSet)
    }
}

impl FromIterator<u32> for UidSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        UidSet(iter.into_iter().filter(|u| *u > 0).collect())
    }
}

impl fmt::Display for UidSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for uid in self.iter() {
            match runs.last_mut() {
                Some((_, end)) if end.checked_add(1) == Some(uid) => *end = uid,
                _ => runs.push((uid, uid)),
            }
        }
        let text = runs
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
    }

    #[test]
    fn three_hundred_thousand_contiguous_uids_stay_one_run() {
        let set: UidSet = (1..=300_000).collect();
        assert_eq!(set.to_string(), "1:300000");
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(serde_json::from_str::<UidSet>(&json).unwrap(), set);
    }

    #[test]
    fn nothing_but_positive_numbers_is_a_uid_set() {
        assert!(UidSet::parse("1,x").is_err());
        assert!(UidSet::parse("0").is_err());
        assert!(UidSet::parse("1:*").is_err());
        assert!(UidSet::parse("").unwrap().is_empty());
    }

    #[test]
    fn a_set_splits_and_subtracts() {
        let set: UidSet = (1..=10).collect();
        let (head, rest) = set.split_first(4);
        assert_eq!(head.to_string(), "1:4");
        assert_eq!(rest.to_string(), "5:10");
        assert_eq!(set.without(&head), rest);
    }
}
