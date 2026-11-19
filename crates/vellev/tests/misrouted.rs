//! A node that does not serve its tenant, told and counted. Alone in its
//! binary: a log is read from a subscriber of this thread, and a callsite
//! another test's thread reached first would be cached as unheard here.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vellev_core::{CollectionName, DataType, Property};
use workforce_vellev::{
    collection, connect, failure, local_embedding, Asked, Ensured, Tenants, Unusable, RETRY_AFTER,
};

/// Every counter's count, by its name and labels, as a scrape reads them:
/// `name{key=value,…}`.
#[derive(Clone, Default)]
struct Counts(Arc<Mutex<BTreeMap<String, u64>>>);

/// One counter of [`Counts`].
struct Tally {
    named: String,
    counts: Counts,
}

impl metrics::CounterFn for Tally {
    fn increment(&self, value: u64) {
        *self
            .counts
            .0
            .lock()
            .unwrap()
            .entry(self.named.clone())
            .or_default() += value;
    }

    fn absolute(&self, value: u64) {
        self.counts
            .0
            .lock()
            .unwrap()
            .insert(self.named.clone(), value);
    }
}

impl metrics::Recorder for Counts {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }

    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }

    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }

    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let labels: Vec<String> = key
            .labels()
            .map(|label| format!("{}={}", label.key(), label.value()))
            .collect();
        metrics::Counter::from_arc(Arc::new(Tally {
            named: format!("{}{{{}}}", key.name(), labels.join(",")),
            counts: self.clone(),
        }))
    }

    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }

    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

impl Counts {
    fn misroutes(&self) -> u64 {
        let counts = self.0.lock().unwrap();
        let counted = counts.get("workforce.vellev.misrouted_total{tenant=harbor}");
        counted.copied().unwrap_or_default()
    }
}

/// What is logged, as text.
#[derive(Clone, Default)]
struct Logged(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logged {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logged {
    fn errors(&self) -> Vec<String> {
        let text = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        text.lines()
            .filter(|line| line.contains("ERROR"))
            .map(str::to_string)
            .collect()
    }
}

/// A node that answers every request saying it does not serve the tenant,
/// counting the requests.
async fn not_served() -> (Tenants, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&asked);
    let app = axum::Router::new().fallback(move || {
        let counting = Arc::clone(&counting);
        async move {
            counting.fetch_add(1, Ordering::SeqCst);
            (
                axum::http::StatusCode::MISDIRECTED_REQUEST,
                axum::Json(serde_json::json!({ "error": {
                    "code": "not_served_here",
                    "message": "tenant harbor is not served by this node; ask a node that serves it",
                }})),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (connect(&url, "key", "harbor").unwrap(), asked)
}

fn misrouted(report: &eyre::Report, at: &Tenants) -> bool {
    report.downcast_ref::<Unusable>()
        == Some(&Unusable::Misrouted {
            tenant: "harbor".into(),
            node: at.node.clone(),
        })
}

/// A node that does not serve the tenant is told once, by its tenant and
/// its URL, counted, and not asked again until the recheck; a request
/// made outside a collection's checks is told and counted too. Run on a
/// runtime of this thread, as `Ensured` spawns its check, with what it
/// counts and logs kept as the hub's recorder and logs would have them.
#[test]
fn a_tenant_the_node_does_not_serve_is_told_once_named_and_counted() {
    let (counts, logged) = (Counts::default(), Logged::default());
    let writer = logged.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    metrics::with_local_recorder(&counts.clone(), || {
        runtime.block_on(async {
            let (tenants, asked) = not_served().await;
            let requests = || asked.load(Ordering::SeqCst);
            let wanted = collection(
                CollectionName::new("Notes").unwrap(),
                vec![Property::new("body", DataType::Text)],
                local_embedding(&["body"]),
            );
            let ensured = Ensured::new(tenants.clone(), wanted.clone(), "put it right");
            let first = ensured.records().await.unwrap_err();
            assert!(misrouted(&first, &tenants), "{first:?}");
            assert!(first.to_string().starts_with("misrouted: "), "{first}");
            assert_eq!(requests(), 1);
            assert_eq!(counts.misroutes(), 1);
            let errors = logged.errors();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(
                errors[0].contains("tenant=harbor")
                    && errors[0].contains(&format!("node={}", tenants.node)),
                "{errors:?}"
            );

            tokio::time::sleep(RETRY_AFTER + Duration::from_millis(100)).await;
            let again = ensured.records().await.unwrap_err();
            let written = ensured.run(|| tenants.writes.apply(Vec::new())).await;
            for cached in [again, written.unwrap_err()] {
                assert!(misrouted(&cached, &tenants), "{cached:?}");
            }
            assert_eq!(requests(), 1, "told again without asking");
            assert_eq!(counts.misroutes(), 1);

            let direct = tenants.writes.apply(Vec::new()).await.unwrap_err();
            let direct = failure(direct, &tenants, Asked::request(&wanted));
            assert!(misrouted(&direct, &tenants), "{direct:?}");
            assert_eq!(requests(), 2);
            assert_eq!(counts.misroutes(), 2);
            assert_eq!(logged.errors().len(), 2);
        })
    });
}
