//! Pipeline notices: what a pipeline wants everyone who sees its board to
//! know now — a worker on an old protocol, a queue left idle — raised and
//! cleared by the workflows and skills that watch it, and gone once they
//! lapse. One document per notice in [`COLLECTION`], in the pipeline's scope.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{valid_id, Tone};
use crate::hub_core::pipelines::{Scope, StoreError};
use crate::store::{Direction, Document, DocumentStore, Op, Query, Where};

/// The collection, in a pipeline's scope, holding its notices.
pub const COLLECTION: &str = "pipeline-notices";
/// The longest a notice stands unless raised again: 30 days.
pub const TTL_MAX_SECS: i64 = 30 * 24 * 3_600;
const TITLE_MAX: usize = 200;
const DETAIL_MAX: usize = 2_000;
/// The most notices a board shows.
const SHOWN_MAX: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub key: String,
    pub tone: Tone,
    pub title: String,
    pub detail: Option<String>,
    pub link: Option<String>,
    pub raised_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A notice as its raiser hands it in.
#[derive(Debug, Clone, Deserialize)]
pub struct Raise {
    pub key: String,
    pub tone: Tone,
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// An `http(s)://` URL, or a path on the hub such as `/pipelines/…`.
    #[serde(default)]
    pub link: Option<String>,
    /// Seconds until it lapses; absent, it stands until cleared.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

fn doc_id(pipeline: &str, key: &str) -> String {
    format!("{pipeline}.{key}")
}

fn checked_key(key: &str) -> Result<&str, StoreError> {
    valid_id(key).then_some(key).ok_or_else(|| {
        StoreError::Invalid("notice `key` must be lowercase letters, digits, - or _".into())
    })
}

/// A link a notice may carry: an absolute http(s) URL, or a path on the hub
/// but never a protocol-relative `//host`. A browser drops tabs and newlines
/// and reads `\` as `/`, so `/\t/host` or `/\host` would leave the hub:
/// whitespace, control characters and backslashes are refused anywhere.
fn safe_link(link: &str) -> bool {
    !link
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        && (link.starts_with("https://")
            || link.starts_with("http://")
            || (link.starts_with('/') && !link.starts_with("//")))
}

fn invalid(message: String) -> StoreError {
    StoreError::Invalid(message)
}

/// Raise `raise` on `pipeline`, replacing any notice raised under its key.
pub async fn raise(
    store: &dyn DocumentStore,
    scope: &Scope,
    pipeline: &str,
    raise: Raise,
    actor: Option<&str>,
) -> Result<Notice, StoreError> {
    let key = checked_key(&raise.key)?;
    let title = raise.title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || title.chars().count() > TITLE_MAX {
        return Err(invalid(format!(
            "notice `title` is one line of 1–{TITLE_MAX} characters"
        )));
    }
    let detail = raise
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty());
    if detail.is_some_and(|detail| detail.chars().count() > DETAIL_MAX) {
        return Err(invalid(format!(
            "notice `detail` is at most {DETAIL_MAX} characters"
        )));
    }
    let link = raise
        .link
        .as_deref()
        .map(str::trim)
        .filter(|link| !link.is_empty());
    if link.is_some_and(|link| !safe_link(link)) {
        return Err(invalid(
            "notice `link` is an http(s) URL or a path on the hub starting with /".into(),
        ));
    }
    if raise
        .ttl_secs
        .is_some_and(|ttl| !(1..=TTL_MAX_SECS).contains(&ttl))
    {
        return Err(invalid(format!(
            "notice `ttl_secs` is 1–{TTL_MAX_SECS} seconds"
        )));
    }
    let body = json!({
        "pipeline": pipeline,
        "key": key,
        "tone": raise.tone,
        "title": title,
        "detail": detail,
        "link": link,
    });
    let doc = store
        .set(
            scope,
            COLLECTION,
            &doc_id(pipeline, key),
            body,
            None,
            raise.ttl_secs,
            actor,
        )
        .await?;
    notice_of(&doc)
        .ok_or_else(|| StoreError::Backend("the raised notice does not read back".into()))
}

/// Clear the notice raised on `pipeline` under `key`; `false` when there was
/// none.
pub async fn clear(
    store: &dyn DocumentStore,
    scope: &Scope,
    pipeline: &str,
    key: &str,
    actor: Option<&str>,
) -> Result<bool, StoreError> {
    let key = checked_key(key)?;
    store
        .delete(scope, COLLECTION, &doc_id(pipeline, key), None, actor)
        .await
}

/// The notices raised on `pipeline` that are neither cleared nor lapsed,
/// newest first.
pub async fn active(
    store: &dyn DocumentStore,
    scope: &Scope,
    pipeline: &str,
) -> eyre::Result<Vec<Notice>> {
    let page = store
        .query(
            scope,
            COLLECTION,
            &Query {
                filters: vec![Where {
                    field: "pipeline".into(),
                    op: Op::Eq,
                    value: json!(pipeline),
                }],
                direction: Direction::Desc,
                limit: Some(SHOWN_MAX),
                ..Default::default()
            },
        )
        .await?;
    let mut notices: Vec<Notice> = page.documents.iter().filter_map(notice_of).collect();
    notices.sort_by(|a, b| b.raised_at.cmp(&a.raised_at));
    Ok(notices)
}

fn notice_of(doc: &Document) -> Option<Notice> {
    let text = |key: &str| {
        doc.body
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Some(Notice {
        key: text("key")?,
        tone: serde_json::from_value(doc.body.get("tone")?.clone()).ok()?,
        title: text("title")?,
        detail: text("detail"),
        link: text("link").filter(|link| safe_link(link)),
        raised_at: doc.created_at,
        expires_at: doc.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryDocumentStore;

    fn warning(key: &str, title: &str) -> Raise {
        Raise {
            key: key.into(),
            tone: Tone::Warn,
            title: title.into(),
            detail: None,
            link: None,
            ttl_secs: None,
        }
    }

    fn example_agent() -> Scope {
        Scope::Agent("example".into())
    }

    #[tokio::test]
    async fn a_notice_is_raised_replaced_listed_per_pipeline_and_cleared() {
        let store = MemoryDocumentStore::new();
        let scope = example_agent();
        let first = raise(
            &store,
            &scope,
            "orders",
            Raise {
                detail: Some("  Every coding step failed within a minute.  ".into()),
                link: Some("/pipelines/agent:example/orders".into()),
                ..warning("hlt-2", "the worker speaks\nprotocol v2")
            },
            Some("pipeline-watch"),
        )
        .await
        .unwrap();
        assert_eq!(
            (
                first.title.as_str(),
                first.detail.as_deref(),
                first.link.as_deref(),
                first.expires_at
            ),
            (
                "the worker speaks protocol v2",
                Some("Every coding step failed within a minute."),
                Some("/pipelines/agent:example/orders"),
                None
            )
        );
        let mut keys: Vec<String> = serde_json::to_value(&first)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "detail",
                "expires_at",
                "key",
                "link",
                "raised_at",
                "title",
                "tone"
            ]
        );

        let refreshed = raise(
            &store,
            &scope,
            "orders",
            warning("hlt-2", "the worker speaks protocol v3"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (refreshed.title.as_str(), refreshed.raised_at),
            ("the worker speaks protocol v3", first.raised_at),
            "raised again, a notice is replaced but keeps when it was raised, so a dismissal holds"
        );
        raise(
            &store,
            &scope,
            "orders",
            Raise {
                tone: Tone::Bad,
                ..warning("hlt-7", "order-verify idle")
            },
            None,
        )
        .await
        .unwrap();
        raise(
            &store,
            &scope,
            "onboarding",
            warning("hlt-2", "elsewhere"),
            None,
        )
        .await
        .unwrap();

        let shown = active(&store, &scope, "orders").await.unwrap();
        let mut titles: Vec<(&str, &str)> = shown
            .iter()
            .map(|notice| (notice.key.as_str(), notice.title.as_str()))
            .collect();
        titles.sort_unstable();
        assert_eq!(
            titles,
            [
                ("hlt-2", "the worker speaks protocol v3"),
                ("hlt-7", "order-verify idle")
            ]
        );
        assert!(shown
            .windows(2)
            .all(|pair| pair[0].raised_at >= pair[1].raised_at));

        assert!(clear(&store, &scope, "orders", "hlt-2", None)
            .await
            .unwrap());
        assert!(!clear(&store, &scope, "orders", "hlt-2", None)
            .await
            .unwrap());
        assert_eq!(active(&store, &scope, "orders").await.unwrap().len(), 1);
        assert_eq!(active(&store, &scope, "onboarding").await.unwrap().len(), 1);
        let raised_anew = raise(
            &store,
            &scope,
            "orders",
            warning("hlt-2", "the worker speaks protocol v2"),
            None,
        )
        .await
        .unwrap();
        assert!(
            raised_anew.raised_at > first.raised_at,
            "a notice raised after it was cleared is new"
        );
    }

    #[tokio::test]
    async fn a_notice_lapses_after_its_ttl_and_a_bad_one_is_refused_naming_the_field() {
        let store = MemoryDocumentStore::new();
        let scope = example_agent();
        let lapsing = raise(
            &store,
            &scope,
            "orders",
            Raise {
                ttl_secs: Some(1),
                ..warning("flow-2", "4 verification rungs overdue")
            },
            None,
        )
        .await
        .unwrap();
        assert!(lapsing.expires_at.is_some_and(|at| at > lapsing.raised_at));
        assert_eq!(active(&store, &scope, "orders").await.unwrap().len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        assert!(active(&store, &scope, "orders").await.unwrap().is_empty());

        let long_title = "t".repeat(TITLE_MAX + 1);
        for (bad, field) in [
            (warning("HLT 2", "t"), "`key`"),
            (warning("x", "  \n "), "`title`"),
            (warning("x", &long_title), "`title`"),
            (
                Raise {
                    detail: Some("d".repeat(DETAIL_MAX + 1)),
                    ..warning("x", "t")
                },
                "`detail`",
            ),
            (
                Raise {
                    link: Some("javascript:alert(1)".into()),
                    ..warning("x", "t")
                },
                "`link`",
            ),
            (
                Raise {
                    link: Some("//evil.example/path".into()),
                    ..warning("x", "t")
                },
                "`link`",
            ),
            (
                Raise {
                    link: Some("/\t/evil.example".into()),
                    ..warning("x", "t")
                },
                "`link`",
            ),
            (
                Raise {
                    ttl_secs: Some(0),
                    ..warning("x", "t")
                },
                "`ttl_secs`",
            ),
            (
                Raise {
                    ttl_secs: Some(TTL_MAX_SECS + 1),
                    ..warning("x", "t")
                },
                "`ttl_secs`",
            ),
        ] {
            let err = raise(&store, &scope, "orders", bad, None)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Invalid(_)) && err.to_string().contains(field),
                "{field}: {err}"
            );
        }
        assert!(matches!(
            clear(&store, &scope, "orders", "Bad Key", None).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(
            active(&store, &scope, "orders").await.unwrap().is_empty(),
            "a refused notice writes nothing"
        );
        let after_lapse = raise(
            &store,
            &scope,
            "orders",
            warning("flow-2", "4 verification rungs overdue"),
            None,
        )
        .await
        .unwrap();
        assert!(
            after_lapse.raised_at > lapsing.raised_at,
            "a notice raised after it lapsed is new"
        );
    }

    #[test]
    fn a_link_is_an_http_url_or_a_hub_path_with_nothing_a_browser_rewrites() {
        for (link, safe) in [
            ("https://github.com/acme/app/pull/1702", true),
            ("http://localhost:8080/pipelines", true),
            ("/pipelines/agent:example/orders?item=ORD-1", true),
            ("//evil.example", false),
            ("/\\evil.example", false),
            ("/\t/evil.example", false),
            ("/\n/evil.example", false),
            ("/\r/evil.example", false),
            ("/pipelines\\orders", false),
            ("/pipelines/orders\u{0}", false),
            ("https://evil.example/a b", false),
            ("javascript:alert(1)", false),
            ("pipelines/orders", false),
            ("", false),
        ] {
            assert_eq!(safe_link(link), safe, "{link:?}");
        }
    }

    #[tokio::test]
    async fn a_notice_written_around_raise_shows_no_unsafe_link() {
        let store = MemoryDocumentStore::new();
        let scope = example_agent();
        store
            .set(
                &scope,
                COLLECTION,
                &doc_id("orders", "hlt-2"),
                json!({"pipeline": "orders", "key": "hlt-2", "tone": "warn", "title": "t", "link": "javascript:alert(1)"}),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let shown = active(&store, &scope, "orders").await.unwrap();
        assert_eq!((shown.len(), shown[0].link.as_deref()), (1, None));
    }
}
