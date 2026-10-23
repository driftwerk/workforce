//! Cursor pagination for telemetry list endpoints.
//!
//! Cursors are an opaque base64 of `"<rfc3339 completed_at>|<id>"`. Both
//! SQLite and Postgres telemetry backends apply
//! `WHERE (completed_at, id) < ($cursor_ts, $cursor_id)` so the cursor
//! is stable under inserts (lexicographic on the pair) and never returns
//! duplicates across pages.
//!
//! The id half disambiguates rows that share the same `completed_at`
//! down to the second — common for batched recordings.

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde::de::{self, Deserializer, Visitor};

/// Encode a `(completed_at, id)` pair into an opaque cursor string.
pub fn encode_cursor(completed_at: DateTime<Utc>, id: i64) -> String {
    let raw = format!("{}|{}", completed_at.to_rfc3339(), id);
    URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

/// Decode a cursor back into its `(completed_at, id)` pair.
///
/// Returns an error rather than `None` so handlers can respond with 400
/// instead of silently returning the first page when the client passes
/// garbage.
pub fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, i64)> {
    let decoded_bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|e| eyre::eyre!("invalid cursor encoding: {e}"))?;
    let decoded = std::str::from_utf8(&decoded_bytes)
        .map_err(|e| eyre::eyre!("cursor is not valid UTF-8: {e}"))?;
    let (ts_str, id_str) = decoded
        .split_once('|')
        .ok_or_else(|| eyre::eyre!("cursor missing `|` separator"))?;
    let ts = DateTime::parse_from_rfc3339(ts_str)
        .map_err(|e| eyre::eyre!("invalid cursor timestamp: {e}"))?
        .with_timezone(&Utc);
    let id: i64 = id_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid cursor id: {e}"))?;
    Ok((ts, id))
}

/// Default page size for cursor-paginated endpoints when the caller
/// omits `?limit=`. Big enough to be useful out of the box, small enough
/// not to abuse the round-trip when an admin lazily hits the URL.
pub const DEFAULT_PAGE_LIMIT: usize = 50;

/// Hard upper bound on `?limit=` for cursor-paginated endpoints.
/// Protects the hub from accidental `limit=1000000` requests.
pub const MAX_PAGE_LIMIT: usize = 200;

/// Clamp a caller-supplied limit into the allowed range. `None` returns
/// [`DEFAULT_PAGE_LIMIT`].
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit
        .map(|n| n.clamp(1, MAX_PAGE_LIMIT))
        .unwrap_or(DEFAULT_PAGE_LIMIT)
}

/// `serde(deserialize_with = ...)` helper for `Option<usize>` query
/// parameters. `serde_urlencoded` always presents values as strings, and
/// when combined with `#[serde(flatten)]` the inner struct receives them
/// verbatim — vanilla `Option<usize>` then fails with
/// `invalid type: string "50", expected usize`. This visitor accepts both
/// numeric and string-encoded forms, and treats empty / missing as `None`.
pub fn deserialize_opt_usize<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptUsize;

    impl<'de> Visitor<'de> for OptUsize {
        type Value = Option<usize>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an unsigned integer, its string representation, or null")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2>(self, d: D2) -> std::result::Result<Self::Value, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            d.deserialize_any(OptUsize)
        }

        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            usize::try_from(v)
                .map(Some)
                .map_err(|_| E::custom(format!("limit {v} exceeds usize::MAX")))
        }

        fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            u64::try_from(v)
                .map_err(|_| E::custom(format!("limit must be non-negative, got {v}")))
                .and_then(|u| self.visit_u64(u))
        }

        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<usize>()
                .map(Some)
                .map_err(|e| E::custom(format!("invalid limit {trimmed:?}: {e}")))
        }

        fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(OptUsize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_cursor() {
        let ts = DateTime::parse_from_rfc3339("2026-04-30T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        let encoded = encode_cursor(ts, 42);
        let (out_ts, out_id) = decode_cursor(&encoded).unwrap();
        assert_eq!(out_ts, ts);
        assert_eq!(out_id, 42);
    }

    #[test]
    fn opaque_round_trip_includes_subsecond() {
        let ts = DateTime::parse_from_rfc3339("2026-04-30T12:34:56.789Z")
            .unwrap()
            .with_timezone(&Utc);
        let encoded = encode_cursor(ts, 100);
        assert!(!encoded.contains('|'));
        assert!(!encoded.contains('+'));
        let (out_ts, _) = decode_cursor(&encoded).unwrap();
        assert_eq!(out_ts, ts);
    }

    #[test]
    fn rejects_garbage_cursor() {
        assert!(decode_cursor("not-base64!!!").is_err());
        // Valid base64 but not "ts|id"
        let bogus = URL_SAFE_NO_PAD.encode("hello");
        assert!(decode_cursor(&bogus).is_err());
    }

    #[test]
    fn clamp_limit_defaults_and_caps() {
        assert_eq!(clamp_limit(None), DEFAULT_PAGE_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10)), 10);
        assert_eq!(clamp_limit(Some(usize::MAX)), MAX_PAGE_LIMIT);
    }

    /// Mirrors the shape produced by `axum::extract::Query` + `serde_urlencoded`
    /// + `#[serde(flatten)]`: numeric query params arrive as strings.
    #[derive(serde::Deserialize, Debug)]
    struct PageProbe {
        #[serde(default, deserialize_with = "deserialize_opt_usize")]
        limit: Option<usize>,
    }

    #[derive(serde::Deserialize, Debug)]
    struct FlattenedProbe {
        #[serde(flatten)]
        page: PageProbe,
    }

    fn parse_qs<T: serde::de::DeserializeOwned>(
        qs: &str,
    ) -> Result<T, serde_urlencoded::de::Error> {
        serde_urlencoded::from_str(qs)
    }

    #[test]
    fn deserialize_opt_usize_accepts_numeric_string_from_query() {
        let probe: PageProbe = parse_qs("limit=50").unwrap();
        assert_eq!(probe.limit, Some(50));
    }

    #[test]
    fn deserialize_opt_usize_accepts_numeric_string_when_flattened() {
        let probe: FlattenedProbe = parse_qs("limit=50").unwrap();
        assert_eq!(probe.page.limit, Some(50));
    }

    #[test]
    fn deserialize_opt_usize_treats_missing_as_none() {
        let probe: PageProbe = parse_qs("").unwrap();
        assert_eq!(probe.limit, None);
    }

    #[test]
    fn deserialize_opt_usize_treats_empty_value_as_none() {
        let probe: PageProbe = parse_qs("limit=").unwrap();
        assert_eq!(probe.limit, None);
    }

    #[test]
    fn deserialize_opt_usize_rejects_non_numeric() {
        let err = parse_qs::<PageProbe>("limit=abc").unwrap_err();
        assert!(err.to_string().contains("invalid limit"));
    }

    #[test]
    fn deserialize_opt_usize_rejects_negative() {
        let err = parse_qs::<PageProbe>("limit=-1").unwrap_err();
        assert!(err.to_string().contains("invalid limit"));
    }

    #[test]
    fn deserialize_opt_usize_accepts_native_number_from_json() {
        let probe: PageProbe = serde_json::from_str(r#"{"limit": 25}"#).unwrap();
        assert_eq!(probe.limit, Some(25));
    }

    #[test]
    fn deserialize_opt_usize_accepts_string_number_from_json() {
        let probe: PageProbe = serde_json::from_str(r#"{"limit": "25"}"#).unwrap();
        assert_eq!(probe.limit, Some(25));
    }

    #[test]
    fn deserialize_opt_usize_accepts_null_from_json() {
        let probe: PageProbe = serde_json::from_str(r#"{"limit": null}"#).unwrap();
        assert_eq!(probe.limit, None);
    }
}
