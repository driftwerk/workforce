//! Making sure a collection exists in a shape its store can use, and
//! telling a node that is not there yet from one an operator must fix.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{eyre, Report, Result};
use tokio::sync::watch;
use tracing::{error, info};
use vellev_client::api::{Code, Nearest};
use vellev_client::Records;
use vellev_core::{Collection, CollectionName, DataType, EmbeddingProvider, Property};

use crate::shape::{checked, Refused};
use crate::signals::no_tenant;
use crate::Tenants;

/// How long a collection found unusable stays so before it is looked at
/// again, so that one an operator has put right is used without a restart.
pub const RECHECK_AFTER: Duration = Duration::from_secs(60);

/// Why a store cannot use its collection however often it asks: an
/// operator has to act. Anything else, such as a node not answering, may
/// pass by itself.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unusable {
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error("vellev has no tenant '{tenant}' that the key reaches: create the tenant with an admin key, or give [vellev] api_key (or [vellev.code] api_key, for code vectors) a key for it")]
    NoTenant { tenant: String },
    #[error(
        "vellev does not accept the key for tenant '{tenant}': {message}; check [vellev] api_key (or [vellev.code] api_key, for code vectors)"
    )]
    Key { tenant: String, message: String },
    #[error("the key for tenant '{tenant}' may not do this: {message}; give [vellev] api_key (or [vellev.code] api_key, for code vectors) a key that may")]
    Forbidden { tenant: String, message: String },
    #[error(
        "vellev cannot serve collection '{collection}' of tenant '{tenant}' as asked: {message}"
    )]
    Invalid {
        tenant: String,
        collection: String,
        message: String,
    },
    #[error("the vellev node cannot run '{model}', which embeds collection '{collection}' of tenant '{tenant}': {message}; run a node at [vellev] url (or [vellev.code] url, for code vectors) that has {model}")]
    Model {
        tenant: String,
        collection: String,
        model: String,
        message: String,
    },
    #[error("misrouted: the vellev node at {node} does not serve tenant '{tenant}' (it answered 421 not_served_here): point [vellev] url, or [vellev.code] url for code vectors, at a node that serves it, or list the tenant in that node's VELLEV_TENANTS")]
    Misrouted { tenant: String, node: String },
}

/// The node cannot embed with a collection's model just now. Not
/// [`Unusable`]: a node may be loading its model, so this is asked about
/// again; but its message says what to check. A node that says it cannot
/// run the model at all is [`Unusable::Model`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the vellev node cannot embed with '{model}' for collection '{collection}' of tenant '{tenant}': {message}; check that the node runs {model}")]
pub struct ModelUnavailable {
    pub tenant: String,
    pub collection: String,
    pub model: String,
    pub message: String,
}

/// What a failed request asked about, which decides what its failure means.
#[derive(Debug, Clone, Copy)]
pub enum Asked<'a> {
    /// The tenant itself, as listing its collections does: not found means
    /// no such tenant, or one the key does not reach.
    Tenant,
    /// The shape of collection `name`, embedded with `model`, as making
    /// sure of it does: invalid means the node will not hold it as wanted.
    Collection {
        name: &'a CollectionName,
        model: &'a str,
    },
    /// One ordinary request about collection `name`, embedded with
    /// `model`: what is invalid about it is the request's own fault, for
    /// its caller.
    Request {
        name: &'a CollectionName,
        model: &'a str,
    },
}

impl<'a> Asked<'a> {
    /// Making sure of `wanted`.
    pub fn collection(wanted: &'a Collection) -> Self {
        Self::Collection {
            name: &wanted.name,
            model: &wanted.embedding.model,
        }
    }

    /// An ordinary request about `wanted`.
    pub fn request(wanted: &'a Collection) -> Self {
        Self::Request {
            name: &wanted.name,
            model: &wanted.embedding.model,
        }
    }
}

/// `error` from a request of the tenant `at`, as [`Unusable`] when an
/// operator has to act, otherwise as it is. This is the one place a node's
/// answer is classified. A node that does not serve the tenant is
/// [`Unusable::Misrouted`] whatever was asked. A tenant the key does not
/// reach is to it one that does not exist, so either is `NoTenant`; it is
/// told by what was asked, never by the message. A node that cannot run a
/// collection's model at all (its `model_unavailable`, until it or the
/// collection is set up otherwise) is named with the model asked for.
pub fn failure(error: vellev_client::Error, at: &Tenants, asked: Asked) -> Report {
    let tenant = at.reads.id().to_string();
    let message = error.to_string();
    let unusable = match (error.code(), asked) {
        (Some(Code::NotServedHere), _) => misrouted(at),
        (_, Asked::Collection { name, model } | Asked::Request { name, model })
            if error.is_model_unavailable() =>
        {
            Unusable::Model {
                tenant,
                collection: name.to_string(),
                model: model.to_string(),
                message,
            }
        }
        (_, Asked::Tenant) if no_tenant(&error, true) => Unusable::NoTenant { tenant },
        (Some(Code::Unauthorized), _) => Unusable::Key { tenant, message },
        (Some(Code::Forbidden), _) => Unusable::Forbidden { tenant, message },
        (Some(Code::Invalid), Asked::Collection { name, .. }) => Unusable::Invalid {
            tenant,
            collection: name.to_string(),
            message,
        },
        (_, Asked::Request { name, .. }) => {
            return Report::new(error).wrap_err(format!(
                "vellev request about collection '{name}' of tenant '{tenant}' failed"
            ))
        }
        _ => return Report::new(error),
    };
    Report::new(unusable)
}

/// `error` from a request of the tenant `at` that [`failure`] does not
/// classify, as markers and seeding make: [`Unusable::Misrouted`] when the
/// node does not serve the tenant, otherwise as it is.
pub fn classified(error: vellev_client::Error, at: &Tenants) -> Report {
    match error.code() {
        Some(Code::NotServedHere) => Report::new(misrouted(at)),
        _ => Report::new(error),
    }
}

/// The node at `at` does not serve its tenant: told, counted and named,
/// here alone.
fn misrouted(at: &Tenants) -> Unusable {
    let (tenant, node) = (at.reads.id().to_string(), at.node.clone());
    error!(
        tenant = %tenant,
        node = %node,
        "vellev node does not serve the tenant: misrouted"
    );
    metrics::counter!("workforce.vellev.misrouted_total", "tenant" => tenant.clone()).increment(1);
    Unusable::Misrouted { tenant, node }
}

/// Embed one probe query in `records`, when its node embeds for it, so that
/// a node that cannot run the model is told at once rather than by every
/// write after. Nothing is written.
pub async fn probe_model(at: &Tenants, records: &Records, wanted: &Collection) -> Result<()> {
    if wanted.embedding.provider == EmbeddingProvider::None {
        return Ok(());
    }
    let probe = Nearest::to_text("model probe", 1);
    match records.nearest(&probe).await {
        Ok(_) => Ok(()),
        Err(e) if matches!(e.code(), Some(Code::Unavailable | Code::Invalid)) => {
            Err(Report::new(ModelUnavailable {
                tenant: at.reads.id().to_string(),
                collection: wanted.name.to_string(),
                model: wanted.embedding.model.clone(),
                message: e.to_string(),
            }))
        }
        Err(e) => Err(failure(e, at, Asked::request(wanted))),
    }
}

/// What the property that tells one making of a collection from the next
/// is named by, a random id after it.
const INCARNATION: &str = "incarnation_";

/// A property naming a new making of a collection. It holds no value:
/// vellev's schema has nowhere else to keep what a collection is, and a
/// collection dropped, or left out of a restored backup, loses it with
/// everything else it held.
fn incarnation() -> Property {
    let name = format!("{INCARNATION}{}", uuid::Uuid::new_v4().simple());
    Property {
        filterable: false,
        ..Property::new(&name, DataType::Bool)
    }
}

/// Which making of the collection `found` is, if it was made or adopted
/// by [`Ensured::identified`]: what is recorded of its contents elsewhere,
/// such as that they are complete, holds for this making only. Two stores
/// that adopt one collection at once may each give it a name; the lowest
/// is its identity, whichever store reads it.
pub fn identity(found: &Collection) -> Option<String> {
    found
        .properties
        .iter()
        .filter_map(|p| p.name.strip_prefix(INCARNATION))
        .min()
        .map(str::to_string)
}

/// Create `wanted` in `tenant`, or check an existing collection and add the
/// stored properties it lacks; returns whether it was created. The tenant
/// is never created: one that is missing is [`Unusable::NoTenant`], for an
/// operator to create deliberately. A collection of the wrong shape, as it
/// would be with those properties added, is [`Unusable::Refused`], with
/// `remedy` (see [`crate::mismatches`]), and is left as it was found: a
/// property that is embedded but not stored cannot be added to it.
pub async fn ensure(
    at: &Tenants,
    records: &Records,
    wanted: &Collection,
    remedy: &str,
) -> Result<bool> {
    ensure_as(at, records, wanted, remedy, false).await
}

/// As [`ensure`]; `identified` gives a collection it creates, or finds
/// without one, an [`identity`].
async fn ensure_as(
    at: &Tenants,
    records: &Records,
    wanted: &Collection,
    remedy: &str,
    identified: bool,
) -> Result<bool> {
    let failed = |e| failure(e, at, Asked::collection(wanted));
    at.reads
        .collections()
        .await
        .map_err(|e| failure(e, at, Asked::Tenant))?;
    let found = match records.schema().await.map_err(failed)? {
        Some(found) => found,
        None => {
            info!(collection = %wanted.name, "Creating vellev collection");
            let mut made = wanted.clone();
            if identified {
                made.properties.push(incarnation());
            }
            match records.create(&made).await {
                Ok(_) => return Ok(true),
                // Made meanwhile by another store, under its own name for
                // this making: that one is the collection, and is checked
                // like any found.
                Err(e) if crate::is_conflict(&e) => records
                    .schema()
                    .await
                    .map_err(failed)?
                    .ok_or_else(|| eyre!("vellev collection '{}' vanished", wanted.name))?,
                Err(e) => return Err(failed(e)),
            }
        }
    };
    let adopted = (identified && identity(&found).is_none()).then(incarnation);
    let missing: Vec<Property> = wanted
        .properties
        .iter()
        .filter(|p| p.stored && found.property(&p.name).is_none())
        .chain(&adopted)
        .cloned()
        .collect();
    let widened = Collection {
        properties: found.properties.iter().chain(&missing).cloned().collect(),
        ..found
    };
    checked(&widened, wanted, remedy).map_err(as_unusable)?;
    for property in &missing {
        info!(collection = %wanted.name, property = %property.name, "Adding property to vellev collection");
        records.add_property(property).await.map_err(failed)?;
    }
    Ok(false)
}

/// Check an existing collection against `wanted` without changing it.
pub async fn doctor(
    at: &Tenants,
    records: &Records,
    wanted: &Collection,
    remedy: &str,
) -> Result<()> {
    let found = records
        .schema()
        .await
        .map_err(|e| failure(e, at, Asked::collection(wanted)))?
        .ok_or_else(|| eyre!("vellev collection '{}' does not exist", wanted.name))?;
    checked(&found, wanted, remedy).map_err(as_unusable)
}

/// A [`Refused`] as the [`Unusable`] it is.
fn as_unusable(report: Report) -> Report {
    match report.downcast::<Refused>() {
        Ok(refused) => Report::new(Unusable::Refused(refused)),
        Err(report) => report,
    }
}

/// How making sure of a collection came out, as every caller waiting on it
/// is told: an error that cannot be shared is told in its words.
#[derive(Clone)]
enum Outcome {
    Ready,
    Unusable(Unusable),
    Model(ModelUnavailable),
    Failed(String),
}

impl Outcome {
    fn of(checked: &Result<()>) -> Self {
        let Err(e) = checked else {
            return Self::Ready;
        };
        if let Some(unusable) = e.downcast_ref::<Unusable>() {
            Self::Unusable(unusable.clone())
        } else if let Some(model) = e.downcast_ref::<ModelUnavailable>() {
            Self::Model(model.clone())
        } else {
            Self::Failed(format!("{e:#}"))
        }
    }

    fn told(self) -> Result<()> {
        match self {
            Self::Ready => Ok(()),
            Self::Unusable(unusable) => Err(Report::new(unusable)),
            Self::Model(model) => Err(Report::new(model)),
            Self::Failed(said) => Err(eyre!(said)),
        }
    }
}

enum State {
    Unknown,
    Ready,
    Unusable(Unusable, Instant),
    /// The node did not answer, or cannot embed yet: told again without
    /// asking for a short while, then asked again.
    Failed(Outcome, Instant),
    /// Being made sure of now, by one request all callers wait on.
    Checking(watch::Receiver<Option<Outcome>>),
}

/// What [`Ensured::identity`] last read, and when.
type Identity = Option<(String, Instant)>;

/// How long a node that did not answer is taken not to, before it is asked
/// again: long enough that callers queued behind one failure do not each
/// wait out a timeout of their own, short enough that a node coming up is
/// used at once.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// What is known of the collection, shared with the task making sure of it.
struct Known {
    state: std::sync::Mutex<State>,
    created: AtomicBool,
    identity: std::sync::Mutex<Identity>,
}

impl Known {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn identity(&self) -> std::sync::MutexGuard<'_, Identity> {
        self.identity.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A collection made sure of on first use rather than at start, its model
/// probed once when its node embeds for it. A node that does not answer, or
/// cannot embed yet, is tried again on a use after [`RETRY_AFTER`], until
/// it does. One that cannot be used ([`Unusable`]) is reported as such
/// without asking again for [`RECHECK_AFTER`], then looked at again, as it
/// is on every [`Self::recheck`]; and a collection found gone mid-use is
/// made again.
///
/// However many callers ask at once, the node is asked by one of them: the
/// rest wait for what it learns, and none holds a lock while the node is
/// asked, so a node that hangs costs them all one timeout between them.
pub struct Ensured {
    tenants: Tenants,
    records: Records,
    wanted: Collection,
    remedy: &'static str,
    known: Arc<Known>,
    identified: bool,
    recheck_after: Duration,
    retry_after: Duration,
}

impl Ensured {
    /// `wanted` in the tenant `tenants` reach, refused with `remedy` (see
    /// [`crate::mismatches`]).
    pub fn new(tenants: Tenants, wanted: Collection, remedy: &'static str) -> Self {
        Self {
            records: tenants.reads.collection(&wanted.name),
            tenants,
            wanted,
            remedy,
            known: Arc::new(Known {
                state: std::sync::Mutex::new(State::Unknown),
                created: AtomicBool::new(false),
                identity: std::sync::Mutex::new(None),
            }),
            identified: false,
            recheck_after: RECHECK_AFTER,
            retry_after: RETRY_AFTER,
        }
    }

    /// Give the collection an [`identity`] when it is made, or found
    /// without one, for [`Self::identity`] to tell.
    pub fn identified(mut self) -> Self {
        self.identified = true;
        self
    }

    /// Look again after `recheck_after` rather than [`RECHECK_AFTER`], and
    /// ask a node that did not answer again no later than that.
    pub fn rechecking(mut self, recheck_after: Duration) -> Self {
        self.recheck_after = recheck_after;
        self.retry_after = self.retry_after.min(recheck_after);
        self
    }

    /// Which making of the collection the node holds (see [`identity`]),
    /// read from the node at most every [`RECHECK_AFTER`], and again at
    /// once after this found the collection gone. A collection gone is
    /// made again, and is then another.
    pub async fn identity(&self) -> Result<String> {
        if let Some((identity, at)) = &*self.known.identity() {
            if at.elapsed() < self.recheck_after {
                return Ok(identity.clone());
            }
        }
        let failed = |e| failure(e, &self.tenants, Asked::collection(&self.wanted));
        self.records().await?;
        let found = match self.records.schema().await.map_err(failed)? {
            Some(found) => found,
            None => {
                self.recheck().await?;
                self.records
                    .schema()
                    .await
                    .map_err(failed)?
                    .ok_or_else(|| eyre!("vellev collection '{}' vanished", self.wanted.name))?
            }
        };
        let identity = match identity(&found) {
            Some(identity) => identity,
            // Made by a store that gave it none: adopt it.
            None => {
                self.recheck().await?;
                let found = self.records.schema().await.map_err(failed)?;
                found.as_ref().and_then(identity).ok_or_else(|| {
                    eyre!(
                        "vellev collection '{}' cannot be told from another making of it",
                        self.wanted.name
                    )
                })?
            }
        };
        *self.known.identity() = Some((identity.clone(), Instant::now()));
        Ok(identity)
    }

    pub fn name(&self) -> &CollectionName {
        &self.wanted.name
    }

    /// The collection as it is wanted.
    pub fn wanted(&self) -> &Collection {
        &self.wanted
    }

    /// The collection, not yet made sure of: for requests inside
    /// [`Self::run`].
    pub fn collection(&self) -> &Records {
        &self.records
    }

    /// The collection, once it is made sure of.
    pub async fn records(&self) -> Result<&Records> {
        let mut checking = {
            let mut state = self.known.state();
            match &*state {
                State::Ready => return Ok(&self.records),
                State::Unusable(unusable, at) if at.elapsed() < self.recheck_after => {
                    return Err(Report::new(unusable.clone()))
                }
                State::Failed(failed, at) if at.elapsed() < self.retry_after => {
                    return failed.clone().told().map(|()| &self.records)
                }
                State::Checking(checking) => checking.clone(),
                State::Unusable(..) | State::Failed(..) | State::Unknown => {
                    let (told, checking) = watch::channel(None);
                    *state = State::Checking(checking.clone());
                    tokio::spawn(self.check(told));
                    checking
                }
            }
        };
        let outcome = match checking.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone(),
            // The task making sure of it ended without saying.
            Err(_) => {
                let mut state = self.known.state();
                if matches!(&*state, State::Checking(_)) {
                    *state = State::Unknown;
                }
                None
            }
        };
        outcome
            .ok_or_else(|| {
                eyre!(
                    "vellev collection '{}' could not be made sure of",
                    self.wanted.name
                )
            })?
            .told()
            .map(|()| &self.records)
    }

    /// Make sure of the collection, record what was learnt and tell every
    /// caller waiting: a task of its own, so that a caller that gives up
    /// waiting leaves the others their answer.
    fn check(
        &self,
        told: watch::Sender<Option<Outcome>>,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (tenants, wanted, remedy) = (self.tenants.clone(), self.wanted.clone(), self.remedy);
        let (identified, known) = (self.identified, Arc::clone(&self.known));
        async move {
            let records = tenants.reads.collection(&wanted.name);
            // Whatever the node holds now may be another making of it.
            *known.identity() = None;
            let checked = match ensure_as(&tenants, &records, &wanted, remedy, identified).await {
                Ok(created) => {
                    known.created.fetch_or(created, Ordering::SeqCst);
                    probe_model(&tenants, &records, &wanted).await
                }
                Err(e) => Err(e),
            };
            let outcome = Outcome::of(&checked);
            *known.state() = match &outcome {
                Outcome::Ready => State::Ready,
                Outcome::Unusable(unusable) => State::Unusable(unusable.clone(), Instant::now()),
                failed => State::Failed(failed.clone(), Instant::now()),
            };
            let _ = told.send(Some(outcome));
        }
    }

    /// As if the task making sure of the collection had ended without
    /// saying how it went.
    #[cfg(test)]
    pub(crate) fn orphan_the_check(&self) {
        let (_, checking) = watch::channel(None);
        *self.known.state() = State::Checking(checking);
    }

    /// Forget what is known of the collection, unless it is being made
    /// sure of now, which will do.
    fn forget(&self) {
        let mut state = self.known.state();
        if !matches!(&*state, State::Checking(_)) {
            *state = State::Unknown;
        }
    }

    /// As [`Self::records`], looking at the collection again whatever was
    /// known of it.
    pub async fn recheck(&self) -> Result<&Records> {
        self.forget();
        self.records().await
    }

    /// Run `op` on the collection once it is made sure of. A collection
    /// found gone is made again and `op` run once more. A node found not to
    /// serve the tenant is recorded as such, as making sure of it would,
    /// unless it is being made sure of now. Any other failure is the
    /// request's, for its caller: what is known of the collection stays as
    /// it was.
    pub async fn run<T, Fut>(&self, op: impl Fn() -> Fut) -> Result<T>
    where
        Fut: Future<Output = vellev_client::Result<T>>,
    {
        self.records().await?;
        let error = match op().await {
            Ok(done) => return Ok(done),
            Err(e) if e.is_not_found() => {
                self.forget();
                self.records().await?;
                match op().await {
                    Ok(done) => return Ok(done),
                    Err(e) => e,
                }
            }
            Err(e) => e,
        };
        let failed = failure(error, &self.tenants, Asked::request(&self.wanted));
        if let Some(misrouted @ Unusable::Misrouted { .. }) = failed.downcast_ref::<Unusable>() {
            let mut state = self.known.state();
            if !matches!(&*state, State::Checking(_)) {
                *state = State::Unusable(misrouted.clone(), Instant::now());
            }
        }
        Err(failed)
    }

    /// Whether the collection was created since this was last asked.
    pub fn take_created(&self) -> bool {
        self.known.created.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::response::IntoResponse;
    use vellev_core::Embedding;

    use super::*;
    use crate::{collection, connect, local_embedding};

    /// A node holding `found`, recording every request, that adds a
    /// property it is asked to.
    async fn holding(found: Collection) -> (Tenants, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (held, seen) = (Arc::new(Mutex::new(found)), Arc::clone(&asked));
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri, body: String| {
                let (held, seen) = (Arc::clone(&held), Arc::clone(&seen));
                async move {
                    let path = uri.path().to_string();
                    seen.lock().unwrap().push(format!("{method} {path}"));
                    if path.ends_with("/collections") {
                        return axum::Json(serde_json::json!({ "collections": [] }))
                            .into_response();
                    }
                    if path.ends_with("/properties") {
                        let property: Property = serde_json::from_str(&body).unwrap();
                        held.lock().unwrap().properties.push(property);
                        return axum::Json(serde_json::json!({
                            "incarnation": 1,
                            "position": 1,
                            "outcomes": [{ "outcome": "applied" }]
                        }))
                        .into_response();
                    }
                    axum::Json(held.lock().unwrap().clone()).into_response()
                }
            },
        );
        let url = crate::serve(app).await;
        (connect(&url, "key", "harbor").unwrap(), asked)
    }

    fn notes(properties: Vec<Property>, embedding: Embedding) -> Collection {
        collection(CollectionName::new("Notes").unwrap(), properties, embedding)
    }

    /// A found collection lacking a property it is embedded on, which
    /// cannot be added, is refused saying so, and nothing is added to it;
    /// one that lacks only what is stored has that added.
    #[tokio::test]
    async fn only_what_is_stored_is_added_and_only_to_a_collection_that_then_fits() {
        let wanted = notes(
            vec![
                Property::new("body", DataType::Text).unstored(),
                Property::new("owner", DataType::Text),
            ],
            local_embedding(&["body"]),
        );
        let imported = notes(
            Vec::new(),
            Embedding {
                provider: EmbeddingProvider::None,
                model: "none".into(),
                dimensions: 768,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
        );
        let (tenants, asked) = holding(imported.clone()).await;
        let refused = ensure(
            &tenants,
            &tenants.reads.collection(&wanted.name),
            &wanted,
            "remake it",
        )
        .await
        .unwrap_err();
        match refused.downcast_ref::<Unusable>() {
            Some(Unusable::Refused(refused)) => assert!(
                refused
                    .problems
                    .contains(&"property 'body' is missing".to_string()),
                "{refused:?}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let asked = asked.lock().unwrap().clone();
        assert!(
            !asked.iter().any(|request| request.ends_with("/properties")),
            "not widened: {asked:?}"
        );

        let lacking_owner = notes(wanted.properties[..1].to_vec(), wanted.embedding.clone());
        let (tenants, asked) = holding(lacking_owner).await;
        let created = ensure(
            &tenants,
            &tenants.reads.collection(&wanted.name),
            &wanted,
            "remake it",
        )
        .await
        .unwrap();
        assert!(!created);
        let added: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.ends_with("/properties"))
            .cloned()
            .collect();
        assert_eq!(
            added,
            ["POST /v1/tenants/harbor/collections/Notes/properties"]
        );
    }
}
