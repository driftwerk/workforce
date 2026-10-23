//! Live, region-keyed catalog of AWS Bedrock models.
//!
//! Combines `ListInferenceProfiles` and `ListFoundationModels` into a single
//! address book that `set_provider` validates against and `system.list_models`
//! reports from. Graceful under missing IAM permissions: a failed listing
//! produces an `Unavailable` (or partial) state, never blocks the runtime path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use eyre::{eyre, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Mutex as AsyncMutex;

use super::bedrock_capabilities::strip_region_prefix;
use super::bedrock_credentials::CredentialResolver;

const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const PARTIAL_TTL: Duration = Duration::from_secs(60);
const DEFAULT_SUGGESTION_LIMIT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    InferenceProfile,
    FoundationModel,
}

#[derive(Clone, Debug)]
pub struct BedrockModel {
    pub id: String,
    pub name: Option<String>,
    pub kind: ModelKind,
}

#[derive(Clone)]
pub struct BedrockCatalog {
    fetched_at: Instant,
    models: Arc<[BedrockModel]>,
    by_id: Arc<HashMap<String, usize>>,
    notice: Option<Arc<str>>,
}

impl BedrockCatalog {
    pub fn models(&self) -> &[BedrockModel] {
        &self.models
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }
}

#[derive(Clone)]
pub enum CatalogState {
    Ready(BedrockCatalog),
    Unavailable { reason: Arc<str>, since: Instant },
}

impl CatalogState {
    fn since(&self) -> Instant {
        match self {
            CatalogState::Ready(c) => c.fetched_at,
            CatalogState::Unavailable { since, .. } => *since,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(
        "Bedrock model {id:?} is not available in region {region}. \
         Suggested alternatives: {}",
        format_suggestions(suggestions)
    )]
    UnknownModel {
        id: String,
        region: String,
        suggestions: Vec<BedrockModel>,
    },
}

fn format_suggestions(suggestions: &[BedrockModel]) -> String {
    if suggestions.is_empty() {
        return "<none — check that the model is enabled in the Bedrock console>".to_string();
    }
    suggestions
        .iter()
        .map(|m| format!("{} ({:?})", m.id, m.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
pub trait BedrockControlPlane: Send + Sync {
    async fn list_inference_profiles(&self, region: &str) -> Result<Vec<BedrockModel>>;
    async fn list_foundation_models(&self, region: &str) -> Result<Vec<BedrockModel>>;
}

pub struct BedrockCatalogStore {
    ttl: Duration,
    regions: RwLock<HashMap<String, CatalogState>>,
    refresh_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    control_plane: Arc<dyn BedrockControlPlane>,
}

impl BedrockCatalogStore {
    pub fn new(resolver: CredentialResolver) -> Arc<Self> {
        Self::with_control_plane(Arc::new(AwsControlPlane::new(resolver)), DEFAULT_TTL)
    }

    pub fn with_control_plane(
        control_plane: Arc<dyn BedrockControlPlane>,
        ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            ttl,
            regions: RwLock::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            control_plane,
        })
    }

    pub async fn get(&self, region: &str) -> CatalogState {
        match self.cached(region) {
            Some(state) if !self.is_stale(&state) => state,
            _ => self.refresh(region, false).await,
        }
    }

    pub async fn snapshot(&self, region: &str) -> CatalogState {
        self.get(region).await
    }

    /// Best-effort canonicalisation for hot-path invocation. Reads cached
    /// state (refreshes only if TTL has expired), returns the catalog-resolved
    /// id when one is found, otherwise the input unchanged. Never errors —
    /// callers want a usable id, not a validation outcome.
    pub async fn resolve(&self, target: &str, region: &str) -> String {
        let state = self.get(region).await;
        resolve_with_state(&state, target, region).unwrap_or_else(|| target.to_string())
    }

    pub async fn validate(&self, id: &str, region: &str) -> Result<(), CatalogError> {
        let initial = self.get(region).await;
        if catalog_admits(&initial, id) {
            return Ok(());
        }
        let refreshed = self.refresh(region, true).await;
        match refreshed {
            CatalogState::Ready(catalog) if catalog.contains(id) => Ok(()),
            CatalogState::Ready(catalog) => Err(CatalogError::UnknownModel {
                id: id.to_string(),
                region: region.to_string(),
                suggestions: suggest(&catalog, id, DEFAULT_SUGGESTION_LIMIT),
            }),
            CatalogState::Unavailable { .. } => Ok(()),
        }
    }

    /// Region-aware canonicalisation of a model identifier.
    ///
    /// Operators write provider/family IDs in `default_tiers.toml` with whatever
    /// geo prefix they used during local development; at runtime the hub may
    /// live in a different AWS region whose inference profiles carry a
    /// different prefix. Strip the input's geo prefix and look the bare suffix
    /// up against the (already region-scoped) catalog — the catalog only
    /// returns profiles available in this region, so the returned canonical ID
    /// is correct by construction. Inference profiles are preferred over
    /// foundation-model IDs because they support cross-region routing, and
    /// among profiles the region's own geo prefix (`eu.` in `eu-*`) wins over
    /// `global.` so data-residency-constrained installs never route outside
    /// their geography just because AWS listed the global profile first.
    ///
    /// When the catalog is `Unavailable` the input is returned unchanged so the
    /// existing graceful-degradation behaviour is preserved. When the catalog
    /// is `Ready` and no match exists the typed `UnknownModel` error is
    /// returned with the same suggestion list `validate` produces.
    pub async fn resolve_and_validate(
        &self,
        target: &str,
        region: &str,
    ) -> Result<String, CatalogError> {
        let initial = self.get(region).await;
        if let Some(id) = resolve_with_state(&initial, target, region) {
            return Ok(id);
        }
        if matches!(initial, CatalogState::Unavailable { .. }) {
            return Ok(target.to_string());
        }
        let refreshed = self.refresh(region, true).await;
        if let Some(id) = resolve_with_state(&refreshed, target, region) {
            return Ok(id);
        }
        match refreshed {
            CatalogState::Ready(catalog) => Err(CatalogError::UnknownModel {
                id: target.to_string(),
                region: region.to_string(),
                suggestions: suggest(&catalog, target, DEFAULT_SUGGESTION_LIMIT),
            }),
            CatalogState::Unavailable { .. } => Ok(target.to_string()),
        }
    }

    fn cached(&self, region: &str) -> Option<CatalogState> {
        self.regions.read().get(region).cloned()
    }

    fn is_stale(&self, state: &CatalogState) -> bool {
        let ttl = match state {
            CatalogState::Ready(c) if c.notice.is_none() => self.ttl,
            _ => PARTIAL_TTL.min(self.ttl),
        };
        state.since().elapsed() >= ttl
    }

    async fn refresh(&self, region: &str, force: bool) -> CatalogState {
        let lock = self
            .refresh_locks
            .lock()
            .entry(region.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        if !force {
            if let Some(state) = self.cached(region) {
                if !self.is_stale(&state) {
                    return state;
                }
            }
        }

        let (profiles, models) = tokio::join!(
            self.control_plane.list_inference_profiles(region),
            self.control_plane.list_foundation_models(region),
        );
        let state = build_state(region.to_string(), profiles, models);
        self.regions
            .write()
            .insert(region.to_string(), state.clone());
        state
    }
}

fn catalog_admits(state: &CatalogState, id: &str) -> bool {
    match state {
        CatalogState::Ready(c) => c.contains(id),
        CatalogState::Unavailable { .. } => true,
    }
}

fn resolve_with_state(state: &CatalogState, target: &str, region: &str) -> Option<String> {
    match state {
        CatalogState::Ready(catalog) => resolve_in_catalog(catalog, target, region),
        CatalogState::Unavailable { .. } => None,
    }
}

/// Geo prefix of the cross-region inference profiles native to an AWS region
/// (`eu-west-2` → `"eu."`). Used to rank same-model profiles so the region's
/// own geo profile deterministically beats `global.` — geo profiles route
/// only within their geography, which is load-bearing for data-residency
/// constraints (a `global.` profile may run inference on any continent).
fn region_geo_prefix(region: &str) -> Option<&'static str> {
    if region.starts_with("us-") {
        Some("us.")
    } else if region.starts_with("eu-") {
        Some("eu.")
    } else if region.starts_with("ap-northeast") {
        Some("jp.")
    } else if region == "ap-southeast-2" || region == "ap-southeast-4" {
        Some("au.")
    } else if region.starts_with("ap-") {
        Some("apac.")
    } else {
        None
    }
}

fn resolve_in_catalog(catalog: &BedrockCatalog, target: &str, region: &str) -> Option<String> {
    let candidates = candidate_lookup_keys(target);
    let geo = region_geo_prefix(region);
    // Rank same-model inference profiles: the region's own geo profile first,
    // then any other geo-scoped profile, then `global.`. Rank ties break to
    // the lexicographically greatest id, so a version-agnostic key matching
    // several dated releases resolves to the latest.
    let profile_rank = |id: &str| -> u8 {
        match geo {
            Some(g) if id.starts_with(g) => 0,
            _ if id.starts_with("global.") => 2,
            _ if strip_region_prefix(id).len() < id.len() => 1,
            _ => 3,
        }
    };
    let resolve = |normalize: fn(&str) -> &str| {
        candidates.iter().find_map(|key| {
            catalog
                .models
                .iter()
                .filter(|m| {
                    m.kind == ModelKind::InferenceProfile
                        && normalize(strip_region_prefix(&m.id)) == *key
                })
                .min_by(|a, b| {
                    profile_rank(&a.id)
                        .cmp(&profile_rank(&b.id))
                        .then_with(|| b.id.cmp(&a.id))
                })
                .or_else(|| {
                    catalog
                        .models
                        .iter()
                        .filter(|m| {
                            m.kind == ModelKind::FoundationModel
                                && normalize(strip_region_prefix(&m.id)) == *key
                        })
                        .max_by(|a, b| a.id.cmp(&b.id))
                })
                .map(|m| m.id.clone())
        })
    };
    // Exact ids win outright; only unversioned names (tier-registry friendly
    // names like "anthropic.claude-haiku-4-5") fall through to the
    // version-agnostic pass.
    resolve(|id| id).or_else(|| resolve(strip_version_suffix))
}

/// Strip a Bedrock id's version suffix — a trailing `-v{N}:{N}` plus an
/// optional preceding `-{YYYYMMDD}` — so unversioned lookup keys collate
/// with dated catalog entries:
/// `"anthropic.claude-haiku-4-5-20251001-v1:0"` → `"anthropic.claude-haiku-4-5"`.
/// Ids without that shape are returned unchanged.
fn strip_version_suffix(id: &str) -> &str {
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some(base) = id
        .rfind("-v")
        .filter(|&i| {
            id[i + 2..]
                .split_once(':')
                .is_some_and(|(major, minor)| all_digits(major) && all_digits(minor))
        })
        .map(|i| &id[..i])
    else {
        return id;
    };
    match base.rfind('-') {
        Some(i) if base[i + 1..].len() == 8 && all_digits(&base[i + 1..]) => &base[..i],
        _ => base,
    }
}

/// Catalog lookup keys to try, in priority order: the input as given, then
/// each provider-namespaced candidate when the input was bare. Region
/// prefixes are stripped so the keys collate with catalog entries.
fn candidate_lookup_keys(target: &str) -> Vec<String> {
    let bare = strip_region_prefix(target).to_string();
    let mut keys = vec![bare.clone()];
    keys.extend(super::bedrock_capabilities::infer_provider_namespace(&bare));
    keys
}

fn build_state(
    region: String,
    profiles: Result<Vec<BedrockModel>>,
    models: Result<Vec<BedrockModel>>,
) -> CatalogState {
    let mut entries: Vec<BedrockModel> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    match profiles {
        Ok(p) => entries.extend(p),
        Err(e) => notes.push(format!("ListInferenceProfiles failed: {e}")),
    }
    match models {
        Ok(m) => entries.extend(m),
        Err(e) => notes.push(format!("ListFoundationModels failed: {e}")),
    }

    if entries.is_empty() {
        let reason = if notes.is_empty() {
            Arc::from(format!("Bedrock catalog is empty for region {region}"))
        } else {
            Arc::from(notes.join("; "))
        };
        if !notes.is_empty() {
            tracing::warn!(region = %region, reason = %reason, "Bedrock catalog unavailable");
        }
        return CatalogState::Unavailable {
            reason,
            since: Instant::now(),
        };
    }

    let by_id = entries
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();
    let notice = (!notes.is_empty()).then(|| Arc::from(notes.join("; ")));
    if let Some(ref note) = notice {
        tracing::warn!(region = %region, note = %note, "Bedrock catalog is partial");
    }
    CatalogState::Ready(BedrockCatalog {
        fetched_at: Instant::now(),
        models: Arc::from(entries.into_boxed_slice()),
        by_id: Arc::new(by_id),
        notice,
    })
}

pub fn suggest(catalog: &BedrockCatalog, target: &str, limit: usize) -> Vec<BedrockModel> {
    let target_stripped = strip_region_prefix(target);
    let (target_provider, target_family) = provider_family(target_stripped);

    let mut ranked: Vec<(u8, usize, BedrockModel)> = catalog
        .models
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let stripped = strip_region_prefix(&m.id);
            let (provider, family) = provider_family(stripped);
            let rank = match (provider == target_provider, family == target_family) {
                (true, true) => 0,
                (true, false) => 1,
                (false, _) if m.kind == ModelKind::FoundationModel => 2,
                _ => 3,
            };
            (rank, idx, m.clone())
        })
        .collect();
    ranked.sort_by_key(|(rank, idx, _)| (*rank, *idx));
    ranked.into_iter().take(limit).map(|(_, _, m)| m).collect()
}

fn provider_family(stripped: &str) -> (&str, &str) {
    let provider = stripped.split('.').next().unwrap_or("");
    let rest = stripped
        .get(provider.len()..)
        .and_then(|s| s.strip_prefix('.'))
        .unwrap_or("");
    let family = rest.split('-').take(2).collect::<Vec<_>>().join("-");
    let family_idx = family.len();
    (provider, rest.get(..family_idx).unwrap_or(""))
}

struct AwsControlPlane {
    resolver: CredentialResolver,
}

impl AwsControlPlane {
    fn new(resolver: CredentialResolver) -> Self {
        Self { resolver }
    }

    async fn client(&self, region: &str) -> Result<aws_sdk_bedrock::Client> {
        let provider = self
            .resolver
            .shared_provider()
            .await
            .map_err(|e| eyre!("Bedrock catalog: failed to resolve AWS credentials: {e}"))?;
        let cfg = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(provider)
            .region(Region::new(region.to_string()))
            .load()
            .await;
        Ok(aws_sdk_bedrock::Client::new(&cfg))
    }
}

#[async_trait]
impl BedrockControlPlane for AwsControlPlane {
    async fn list_inference_profiles(&self, region: &str) -> Result<Vec<BedrockModel>> {
        let client = self.client(region).await?;
        let summaries: Vec<_> = client
            .list_inference_profiles()
            .into_paginator()
            .items()
            .send()
            .collect::<Result<Vec<_>, _>>()
            .await
            .map_err(|e| eyre!("ListInferenceProfiles failed: {e}"))?;
        Ok(summaries
            .into_iter()
            .filter(|s| {
                matches!(
                    s.status(),
                    aws_sdk_bedrock::types::InferenceProfileStatus::Active
                )
            })
            .map(|s| BedrockModel {
                id: s.inference_profile_id().to_string(),
                name: Some(s.inference_profile_name().to_string()),
                kind: ModelKind::InferenceProfile,
            })
            .collect())
    }

    async fn list_foundation_models(&self, region: &str) -> Result<Vec<BedrockModel>> {
        let client = self.client(region).await?;
        let resp = client
            .list_foundation_models()
            .send()
            .await
            .map_err(|e| eyre!("ListFoundationModels failed: {e}"))?;
        Ok(resp
            .model_summaries
            .unwrap_or_default()
            .into_iter()
            .map(|m| BedrockModel {
                id: m.model_id().to_string(),
                name: m.model_name().map(str::to_string),
                kind: ModelKind::FoundationModel,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockControlPlane {
        profiles: parking_lot::Mutex<Result<Vec<BedrockModel>>>,
        models: parking_lot::Mutex<Result<Vec<BedrockModel>>>,
        profile_calls: AtomicUsize,
        model_calls: AtomicUsize,
    }

    impl MockControlPlane {
        fn new(profiles: Vec<BedrockModel>, models: Vec<BedrockModel>) -> Arc<Self> {
            Arc::new(Self {
                profiles: parking_lot::Mutex::new(Ok(profiles)),
                models: parking_lot::Mutex::new(Ok(models)),
                profile_calls: AtomicUsize::new(0),
                model_calls: AtomicUsize::new(0),
            })
        }

        fn with_profile_error(error: &str, models: Vec<BedrockModel>) -> Arc<Self> {
            Arc::new(Self {
                profiles: parking_lot::Mutex::new(Err(eyre!("{error}"))),
                models: parking_lot::Mutex::new(Ok(models)),
                profile_calls: AtomicUsize::new(0),
                model_calls: AtomicUsize::new(0),
            })
        }

        fn with_both_errors(profile_err: &str, model_err: &str) -> Arc<Self> {
            Arc::new(Self {
                profiles: parking_lot::Mutex::new(Err(eyre!("{profile_err}"))),
                models: parking_lot::Mutex::new(Err(eyre!("{model_err}"))),
                profile_calls: AtomicUsize::new(0),
                model_calls: AtomicUsize::new(0),
            })
        }

        fn set_profiles(&self, profiles: Vec<BedrockModel>) {
            *self.profiles.lock() = Ok(profiles);
        }
    }

    #[async_trait]
    impl BedrockControlPlane for MockControlPlane {
        async fn list_inference_profiles(&self, _region: &str) -> Result<Vec<BedrockModel>> {
            self.profile_calls.fetch_add(1, Ordering::SeqCst);
            self.profiles
                .lock()
                .as_ref()
                .map(|v| v.clone())
                .map_err(|e| eyre!("{e}"))
        }

        async fn list_foundation_models(&self, _region: &str) -> Result<Vec<BedrockModel>> {
            self.model_calls.fetch_add(1, Ordering::SeqCst);
            self.models
                .lock()
                .as_ref()
                .map(|v| v.clone())
                .map_err(|e| eyre!("{e}"))
        }
    }

    fn profile(id: &str) -> BedrockModel {
        BedrockModel {
            id: id.to_string(),
            name: Some(format!("{id} (profile)")),
            kind: ModelKind::InferenceProfile,
        }
    }

    fn foundation(id: &str) -> BedrockModel {
        BedrockModel {
            id: id.to_string(),
            name: Some(format!("{id} (foundation)")),
            kind: ModelKind::FoundationModel,
        }
    }

    #[tokio::test]
    async fn validate_accepts_inference_profile_and_foundation_model() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![foundation("anthropic.claude-sonnet-4-5-20250929-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert!(store
            .validate("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "us-east-1")
            .await
            .is_ok());
        assert!(store
            .validate("anthropic.claude-sonnet-4-5-20250929-v1:0", "us-east-1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn validate_rejects_unknown_with_typed_error() {
        let mock = MockControlPlane::new(
            vec![
                profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
                profile("us.anthropic.claude-opus-4-20250514-v1:0"),
            ],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let err = store
            .validate("us.anthropic.claude-sonnet-4-6", "us-east-1")
            .await
            .unwrap_err();
        match err {
            CatalogError::UnknownModel {
                id,
                region,
                suggestions,
            } => {
                assert_eq!(id, "us.anthropic.claude-sonnet-4-6");
                assert_eq!(region, "us-east-1");
                assert!(!suggestions.is_empty());
                assert!(suggestions
                    .iter()
                    .any(|m| m.id.contains("claude-sonnet-4-5")));
            }
        }
    }

    #[tokio::test]
    async fn validate_returns_ok_when_unavailable() {
        let mock = MockControlPlane::with_both_errors(
            "AccessDenied: ListInferenceProfiles",
            "AccessDenied: ListFoundationModels",
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert!(store
            .validate("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "us-east-1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn validate_triggers_one_refresh_on_miss_before_failing() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let counter = mock.clone();
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let _ = store
            .validate("us.anthropic.claude-imaginary-v9:0", "us-east-1")
            .await;
        assert_eq!(counter.profile_calls.load(Ordering::SeqCst), 2);
        let _ = store
            .validate("us.anthropic.claude-imaginary-v9:0", "us-east-1")
            .await;
        assert_eq!(counter.profile_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn validate_recovers_when_refresh_finds_new_model() {
        let mock = MockControlPlane::new(vec![], vec![]);
        let counter = mock.clone();
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let _ = store.get("us-east-1").await;
        counter.set_profiles(vec![profile("us.anthropic.claude-new-1-v1:0")]);
        assert!(store
            .validate("us.anthropic.claude-new-1-v1:0", "us-east-1")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn single_flight_dedup_under_concurrent_get() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let counter = mock.clone();
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let mut handles = Vec::new();
        for _ in 0..16 {
            let s = store.clone();
            handles.push(tokio::spawn(async move { s.get("us-east-1").await }));
        }
        for h in handles {
            let _ = h.await;
        }
        assert_eq!(counter.profile_calls.load(Ordering::SeqCst), 1);
        assert_eq!(counter.model_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn partial_catalog_when_one_listing_fails() {
        let mock = MockControlPlane::with_profile_error(
            "AccessDenied: ListInferenceProfiles",
            vec![foundation("anthropic.claude-sonnet-4-5-20250929-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let state = store.get("us-east-1").await;
        match state {
            CatalogState::Ready(c) => {
                assert!(c.contains("anthropic.claude-sonnet-4-5-20250929-v1:0"));
                assert!(c
                    .notice()
                    .is_some_and(|n| n.contains("ListInferenceProfiles")));
            }
            CatalogState::Unavailable { .. } => panic!("expected partial Ready"),
        }
    }

    #[tokio::test]
    async fn snapshot_for_unavailable_region_returns_unavailable_state() {
        let mock = MockControlPlane::with_both_errors("AccessDenied", "AccessDenied");
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        match store.snapshot("us-east-1").await {
            CatalogState::Unavailable { reason, .. } => {
                assert!(reason.contains("AccessDenied"));
            }
            CatalogState::Ready(_) => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn ttl_expiry_triggers_refresh() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let counter = mock.clone();
        let store = BedrockCatalogStore::with_control_plane(mock, Duration::ZERO);
        let _ = store.get("us-east-1").await;
        let _ = store.get("us-east-1").await;
        assert!(counter.profile_calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn suggest_groups_by_provider_then_family() {
        let mock = MockControlPlane::new(
            vec![
                profile("us.anthropic.claude-opus-4-20250514-v1:0"),
                profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
                profile("us.meta.llama4-maverick-17b-v1:0"),
            ],
            vec![foundation("anthropic.claude-sonnet-4-5-20250929-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let state = store.get("us-east-1").await;
        let catalog = match state {
            CatalogState::Ready(c) => c,
            CatalogState::Unavailable { .. } => panic!("expected Ready"),
        };
        let suggestions = suggest(&catalog, "us.anthropic.claude-sonnet-4-6", 4);
        assert_eq!(
            suggestions[0].id,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        assert_eq!(
            suggestions[1].id,
            "anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        assert!(suggestions
            .iter()
            .any(|m| m.id == "us.anthropic.claude-opus-4-20250514-v1:0"));
        assert!(suggestions
            .iter()
            .any(|m| m.id == "us.meta.llama4-maverick-17b-v1:0"));
    }

    #[tokio::test]
    async fn suggest_strips_geo_prefix_when_matching() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let state = store.get("eu-west-1").await;
        let catalog = match state {
            CatalogState::Ready(c) => c,
            CatalogState::Unavailable { .. } => panic!("expected Ready"),
        };
        let suggestions = suggest(&catalog, "anthropic.claude-sonnet-fake", 3);
        assert_eq!(
            suggestions[0].id,
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
    }

    #[tokio::test]
    async fn empty_catalog_reports_unavailable_with_helpful_reason() {
        let mock = MockControlPlane::new(vec![], vec![]);
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        match store.get("us-east-1").await {
            CatalogState::Unavailable { reason, .. } => {
                assert!(reason.contains("us-east-1"));
            }
            CatalogState::Ready(_) => panic!("expected Unavailable for empty catalog"),
        }
    }

    #[tokio::test]
    async fn resolve_unversioned_name_matches_dated_profile() {
        // The tier registry hands out undated friendly names
        // ("claude-haiku-4-5"); AWS lists only the dated profile. The
        // 2026-08-18 incident sent the bare name to Bedrock because
        // exact-match resolution could not bridge the version suffix.
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-haiku-4-5-20251001-v1:0")],
            vec![foundation("anthropic.claude-haiku-4-5-20251001-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert_eq!(
            store.resolve("claude-haiku-4-5", "eu-west-2").await,
            "eu.anthropic.claude-haiku-4-5-20251001-v1:0"
        );
    }

    #[tokio::test]
    async fn resolve_unversioned_name_never_matches_a_different_model() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-haiku-4-5-20251001-v1:0")],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert_eq!(
            store.resolve("claude-haiku-4", "eu-west-2").await,
            "claude-haiku-4"
        );
    }

    #[tokio::test]
    async fn resolve_exact_id_wins_over_version_agnostic_match() {
        let mock = MockControlPlane::new(
            vec![
                profile("eu.anthropic.claude-haiku-4-5-20251001-v1:0"),
                profile("eu.anthropic.claude-haiku-4-5"),
            ],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert_eq!(
            store
                .resolve("anthropic.claude-haiku-4-5", "eu-west-2")
                .await,
            "eu.anthropic.claude-haiku-4-5"
        );
    }

    #[tokio::test]
    async fn resolve_version_agnostic_prefers_latest_date() {
        let mock = MockControlPlane::new(
            vec![
                profile("eu.anthropic.claude-haiku-4-5-20250901-v1:0"),
                profile("eu.anthropic.claude-haiku-4-5-20251001-v1:0"),
            ],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        assert_eq!(
            store.resolve("claude-haiku-4-5", "eu-west-2").await,
            "eu.anthropic.claude-haiku-4-5-20251001-v1:0"
        );
    }

    #[test]
    fn strip_version_suffix_handles_all_shapes() {
        assert_eq!(
            strip_version_suffix("anthropic.claude-haiku-4-5-20251001-v1:0"),
            "anthropic.claude-haiku-4-5"
        );
        assert_eq!(
            strip_version_suffix("meta.llama4-maverick-17b-v1:0"),
            "meta.llama4-maverick-17b"
        );
        assert_eq!(
            strip_version_suffix("anthropic.claude-sonnet-5"),
            "anthropic.claude-sonnet-5"
        );
        assert_eq!(strip_version_suffix("model-v1:x"), "model-v1:x");
        assert_eq!(strip_version_suffix("model-v:0"), "model-v:0");
    }

    #[tokio::test]
    async fn resolve_rewrites_us_prefix_to_eu_when_only_eu_profile_exists() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, "eu.anthropic.claude-sonnet-4-5-20250929-v1:0");
    }

    #[tokio::test]
    async fn resolve_returns_input_when_already_matching() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "us-east-1")
            .await
            .unwrap();
        assert_eq!(resolved, "us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    }

    #[tokio::test]
    async fn resolve_rewrites_bare_id_to_region_profile() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![foundation("anthropic.claude-sonnet-4-5-20250929-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-sonnet-4-5-20250929-v1:0", "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, "eu.anthropic.claude-sonnet-4-5-20250929-v1:0");
    }

    #[tokio::test]
    async fn resolve_prefers_region_geo_profile_over_global_regardless_of_list_order() {
        // AWS lists the `global.` profile FIRST — the eu region's own geo
        // profile must still win. Data-residency installs (EU-only) depend on
        // this being deterministic rather than an accident of list ordering.
        let mock = MockControlPlane::new(
            vec![
                profile("global.anthropic.claude-sonnet-5"),
                profile("eu.anthropic.claude-sonnet-5"),
            ],
            vec![foundation("anthropic.claude-sonnet-5")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-sonnet-5", "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, "eu.anthropic.claude-sonnet-5");
    }

    #[tokio::test]
    async fn resolve_uses_global_profile_when_region_geo_absent() {
        // Before AWS ships a region-geo profile for a new model, `global.` is
        // the only routable profile — the resolver must still pick it over
        // the (INFERENCE_PROFILE-only) foundation-model id.
        let mock = MockControlPlane::new(
            vec![profile("global.anthropic.claude-sonnet-5")],
            vec![foundation("anthropic.claude-sonnet-5")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-sonnet-5", "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, "global.anthropic.claude-sonnet-5");
    }

    #[tokio::test]
    async fn resolve_prefers_us_geo_profile_in_us_region() {
        let mock = MockControlPlane::new(
            vec![
                profile("global.anthropic.claude-opus-4-8"),
                profile("us.anthropic.claude-opus-4-8"),
            ],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-opus-4-8", "us-east-1")
            .await
            .unwrap();
        assert_eq!(resolved, "us.anthropic.claude-opus-4-8");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_foundation_model_when_no_inference_profile_matches() {
        let mock = MockControlPlane::new(
            vec![],
            vec![foundation("anthropic.claude-3-haiku-20240307-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("us.anthropic.claude-3-haiku-20240307-v1:0", "us-east-1")
            .await
            .unwrap();
        assert_eq!(resolved, "anthropic.claude-3-haiku-20240307-v1:0");
    }

    #[tokio::test]
    async fn resolve_passes_through_when_catalog_unavailable() {
        let mock = MockControlPlane::with_both_errors("AccessDenied", "AccessDenied");
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let target = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
        let resolved = store
            .resolve_and_validate(target, "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, target);
    }

    #[tokio::test]
    async fn resolve_returns_typed_error_when_family_absent() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-opus-4-20250514-v1:0")],
            vec![],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let err = store
            .resolve_and_validate("us.anthropic.claude-fictional-v9:0", "eu-west-2")
            .await
            .unwrap_err();
        match err {
            CatalogError::UnknownModel {
                id,
                region,
                suggestions,
            } => {
                assert_eq!(id, "us.anthropic.claude-fictional-v9:0");
                assert_eq!(region, "eu-west-2");
                assert!(!suggestions.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn resolve_prefers_inference_profile_over_foundation_when_both_match() {
        let mock = MockControlPlane::new(
            vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")],
            vec![foundation("anthropic.claude-sonnet-4-5-20250929-v1:0")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-sonnet-4-5-20250929-v1:0", "us-east-1")
            .await
            .unwrap();
        assert_eq!(resolved, "us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    }

    /// Regression: when the bare foundation ID is identical to the target AND
    /// a matching geo-prefixed inference profile exists, we must rewrite to
    /// the profile. Earlier the resolver short-circuited on
    /// `catalog.contains(target)` and returned the bare foundation id, which
    /// AWS rejected with "on-demand throughput isn't supported" for Claude
    /// 4.x. This pins inference-profile-over-foundation regardless of which
    /// kind happens to match the target string exactly.
    #[tokio::test]
    async fn resolve_best_effort_returns_input_when_no_match() {
        let mock = MockControlPlane::new(vec![profile("eu.anthropic.claude-sonnet-4-6")], vec![]);
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store.resolve("anthropic.claude-unknown", "eu-west-2").await;
        assert_eq!(resolved, "anthropic.claude-unknown");
    }

    #[tokio::test]
    async fn resolve_best_effort_returns_profile_when_target_is_foundation_form() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-opus-4-7")],
            vec![foundation("anthropic.claude-opus-4-7")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve("anthropic.claude-opus-4-7", "eu-west-2")
            .await;
        assert_eq!(resolved, "eu.anthropic.claude-opus-4-7");
    }

    #[tokio::test]
    async fn partial_catalog_refreshes_sooner_than_full_ttl() {
        let mock = MockControlPlane::with_profile_error(
            "AccessDenied",
            vec![foundation("anthropic.claude-opus-4-7")],
        );
        backdate_and_verify_refresh(mock, "eu-west-2").await;
    }

    #[tokio::test]
    async fn unavailable_catalog_refreshes_sooner_than_full_ttl() {
        let mock = MockControlPlane::with_both_errors(
            "AccessDenied: ListInferenceProfiles",
            "AccessDenied: ListFoundationModels",
        );
        backdate_and_verify_refresh(mock, "eu-west-2").await;
    }

    async fn backdate_and_verify_refresh(mock: Arc<MockControlPlane>, region: &str) {
        let counter = mock.clone();
        let store = BedrockCatalogStore::with_control_plane(mock, Duration::from_secs(3600));
        let _ = store.get(region).await;
        let baseline = counter.profile_calls.load(Ordering::SeqCst);
        assert_eq!(baseline, 1);

        let stale_at = Instant::now()
            .checked_sub(PARTIAL_TTL + Duration::from_secs(1))
            .expect("test clock");
        let backdated = match store.regions.read().get(region).cloned().expect("cached") {
            CatalogState::Ready(c) => CatalogState::Ready(BedrockCatalog {
                fetched_at: stale_at,
                ..c
            }),
            CatalogState::Unavailable { reason, .. } => CatalogState::Unavailable {
                reason,
                since: stale_at,
            },
        };
        store.regions.write().insert(region.to_string(), backdated);
        let _ = store.get(region).await;
        assert!(counter.profile_calls.load(Ordering::SeqCst) > baseline);
    }

    #[tokio::test]
    async fn resolve_rewrites_foundation_target_to_profile_when_both_exist() {
        let mock = MockControlPlane::new(
            vec![profile("eu.anthropic.claude-opus-4-7")],
            vec![foundation("anthropic.claude-opus-4-7")],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);
        let resolved = store
            .resolve_and_validate("anthropic.claude-opus-4-7", "eu-west-2")
            .await
            .unwrap();
        assert_eq!(resolved, "eu.anthropic.claude-opus-4-7");
    }

    #[test]
    fn provider_family_splits_correctly() {
        assert_eq!(
            provider_family("anthropic.claude-sonnet-4-5-20250929-v1:0"),
            ("anthropic", "claude-sonnet")
        );
        assert_eq!(
            provider_family("meta.llama4-maverick-17b-v1:0"),
            ("meta", "llama4-maverick")
        );
        assert_eq!(provider_family("zai.glm-5"), ("zai", "glm-5"));
        assert_eq!(provider_family(""), ("", ""));
    }

    #[test]
    fn format_suggestions_produces_readable_string() {
        let suggestions = vec![profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0")];
        let formatted = format_suggestions(&suggestions);
        assert!(formatted.contains("InferenceProfile"));
        assert!(formatted.contains("claude-sonnet"));
        assert_eq!(
            format_suggestions(&[]),
            "<none — check that the model is enabled in the Bedrock console>"
        );
    }

    #[tokio::test]
    async fn resolve_falls_back_to_inferred_provider_namespace() {
        let mock = MockControlPlane::new(
            vec![
                profile("eu.anthropic.claude-sonnet-4-6"),
                profile("eu.zai.glm-5"),
                profile("eu.deepseek.v3.2"),
            ],
            vec![
                foundation("anthropic.claude-sonnet-4-6"),
                foundation("zai.glm-5"),
                foundation("deepseek.v3.2"),
            ],
        );
        let store = BedrockCatalogStore::with_control_plane(mock, DEFAULT_TTL);

        // Bare Anthropic id resolves via prepend + inference-profile preference.
        assert_eq!(
            store.resolve("claude-sonnet-4-6", "eu-west-2").await,
            "eu.anthropic.claude-sonnet-4-6"
        );

        // Bare marketplace id, prepend form.
        assert_eq!(store.resolve("glm-5", "eu-west-2").await, "eu.zai.glm-5");

        // Bare deepseek: prepend (`deepseek.deepseek-v3.2`) doesn't match;
        // the strip-and-replace fallback (`deepseek.v3.2`) does.
        assert_eq!(
            store.resolve("deepseek-v3.2", "eu-west-2").await,
            "eu.deepseek.v3.2"
        );

        // Already-qualified ids are unaffected by the fallback.
        assert_eq!(
            store
                .resolve("anthropic.claude-sonnet-4-6", "eu-west-2")
                .await,
            "eu.anthropic.claude-sonnet-4-6"
        );
    }

    #[test]
    fn candidate_lookup_keys_includes_namespaced_form_for_bare_ids() {
        assert_eq!(
            candidate_lookup_keys("claude-sonnet-4-6"),
            vec!["claude-sonnet-4-6", "anthropic.claude-sonnet-4-6"]
        );
        assert_eq!(
            candidate_lookup_keys("deepseek-v3.2"),
            vec!["deepseek-v3.2", "deepseek.deepseek-v3.2", "deepseek.v3.2",]
        );
        assert_eq!(
            candidate_lookup_keys("eu.anthropic.claude-sonnet-4-6"),
            vec!["anthropic.claude-sonnet-4-6"]
        );
        assert_eq!(candidate_lookup_keys("zai.glm-5"), vec!["zai.glm-5"]);
    }
}
