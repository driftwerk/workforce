//! What is kept for the person: credentials, know-how, decision records,
//! documents, corpora, artifacts and skills; and the categories that are
//! kept, expire, or are only reported.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use workforce_common::settings::VaultActor;
use workforce_core::access::{AccessWrite, Grant, OwnerWrite, PrincipalKind};
use workforce_orchestrator::artifact_store::{
    Artifact, ArtifactBackend, ArtifactFilter, Caller, FsArtifactBackend,
    Visibility as ArtifactVisibility,
};
use workforce_orchestrator::workflow::definition::{TriggerDefinition, WorkflowDefinition};
use workforce_training::model::CorpusScope;
use workforce_wasm_runtime::types::OwnedSkill;

use super::{Context, Holding, Owners, Person, Tally};
use crate::hub_core::pipelines::Scope;
use crate::skill_access::SkillRegistry;
use crate::vault::{Vault as SecretVault, VaultOp};

/// Who the vault's audit log records a lifecycle operation's writes against.
const VAULT_ACTOR: VaultActor<'static> = VaultActor::System("identity-lifecycle");

pub struct Vault;

/// Where a vault path puts a person.
enum Slot {
    /// A per-user path, as it reads for the identity merged into.
    PerUser(String),
    /// A sign-in in flight, which nobody resumes once its identity has gone.
    Transient,
}

/// The slot `path` gives anyone in `names`: the segment after a `users`
/// segment names the person, at the head of the path (`users/<id>/…`) or
/// inside a skill's namespace (`wasm-skills/<skill>/users/<id>/…`,
/// `…/grants/users/<id>/…`); a device sign-in is kept at
/// `subscription/device_login/<id>`.
fn slot(path: &str, names: &[String], to: &str) -> Option<Slot> {
    let segments: Vec<&str> = path.split('/').collect();
    if let ["subscription", "device_login", id] = segments.as_slice() {
        return names.iter().any(|n| n == id).then_some(Slot::Transient);
    }
    let at = segments
        .windows(2)
        .position(|pair| pair[0] == "users" && names.iter().any(|n| n == pair[1]))?;
    let mut rewritten = segments;
    rewritten[at + 1] = to;
    Some(Slot::PerUser(rewritten.join("/")))
}

/// Whom `path` puts a person's: the id of a device sign-in, and the segment
/// after each `users` segment.
pub(super) fn path_owners(path: &str) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').collect();
    if let ["subscription", "device_login", id] = segments.as_slice() {
        return vec![id.to_string()];
    }
    segments
        .windows(2)
        .filter(|pair| pair[0] == "users")
        .map(|pair| pair[1].to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl Vault {
    /// Every vault the hub holds credentials in: the global one and each
    /// agent's, each once.
    fn vaults(cx: &Context<'_>) -> Result<Vec<(String, Arc<SecretVault>)>> {
        let hub = cx.hub()?;
        let mut vaults: Vec<(String, Arc<SecretVault>)> = Vec::new();
        let candidates = hub
            .global_vault
            .clone()
            .map(|vault| ("global".to_string(), vault))
            .into_iter()
            .chain(cx.services().into_iter().filter_map(|service| {
                service
                    .vault
                    .clone()
                    .map(|vault| (service.agent_id.to_string(), vault))
            }));
        for (label, vault) in candidates {
            if !vaults
                .iter()
                .any(|(_, kept)| kept.location() == vault.location())
            {
                vaults.push((label, vault));
            }
        }
        Ok(vaults)
    }

    fn held_in(vault: &SecretVault, who: &Person) -> Vec<(String, Slot)> {
        vault
            .list(None)
            .into_iter()
            .filter_map(|path| slot(&path, &who.names, &who.key()).map(|slot| (path, slot)))
            .collect()
    }
}

#[async_trait]
impl Holding for Vault {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for (_, vault) in Self::vaults(cx)? {
            owners.absorb(Owners::of(
                vault.list(None).iter().flat_map(|path| path_owners(path)),
            ));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for (label, vault) in Self::vaults(cx)? {
            for (path, _) in Self::held_in(&vault, who) {
                tally.item(format!("{label}: {path}"));
            }
        }
        Ok(tally)
    }

    /// Each entry moves to the same path for the identity merged into, in
    /// one commit per vault; where that path already holds a value it is
    /// kept, and the source's path is reported (never its value) and removed.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for (label, vault) in Self::vaults(cx)? {
            tally.absorb(merge_in(&vault, &label, from, into, apply)?);
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for (label, vault) in Self::vaults(cx)? {
            tally.absorb(delete_in(&vault, &label, who, apply)?);
        }
        Ok(tally)
    }
}

pub(super) fn merge_in(
    vault: &SecretVault,
    label: &str,
    from: &Person,
    into: &Person,
    apply: bool,
) -> Result<Tally> {
    let mut tally = Tally::default();
    let mut moves: Vec<(String, Option<(String, String)>)> = Vec::new();
    for path in vault.list(None) {
        let Some(slot) = slot(&path, &from.names, &into.key()) else {
            continue;
        };
        tally.item(format!("{label}: {path}"));
        let destination = match slot {
            Slot::Transient => None,
            Slot::PerUser(target) if vault.get(&target).is_some() => {
                tally.conflicts.push(format!(
                    "{label}: {target} kept; the merged copy was dropped"
                ));
                None
            }
            Slot::PerUser(target) => vault.get(&path).map(|value| (target, value)),
        };
        moves.push((path, destination));
    }
    if apply && !moves.is_empty() {
        let ops: Vec<VaultOp<'_>> = moves
            .iter()
            .filter_map(|(_, to)| {
                to.as_ref()
                    .map(|(path, value)| VaultOp::Set { path, value })
            })
            .chain(moves.iter().map(|(path, _)| VaultOp::Delete { path }))
            .collect();
        vault.apply(&ops, VAULT_ACTOR)?;
    }
    Ok(tally)
}

pub(super) fn delete_in(
    vault: &SecretVault,
    label: &str,
    who: &Person,
    apply: bool,
) -> Result<Tally> {
    let held = Vault::held_in(vault, who);
    let tally = Tally::of(held.iter().map(|(path, _)| format!("{label}: {path}")));
    if apply && !held.is_empty() {
        let ops: Vec<VaultOp<'_>> = held
            .iter()
            .map(|(path, _)| VaultOp::Delete { path })
            .collect();
        vault.apply(&ops, VAULT_ACTOR)?;
    }
    Ok(tally)
}

pub struct KnowHow;

#[async_trait]
impl Holding for KnowHow {
    /// Kept under a per-agent hash of the person, which cannot be read back.
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::probed())
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let know_how = crate::know_how::KnowHow::shared(&cx.hub()?.config).await;
        Ok(Tally::counted(know_how.identity_entries(&who.key()).await?))
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            crate::know_how::KnowHow::shared(&cx.hub()?.config)
                .await
                .reassign_identity(&from.key(), &into.key())
                .await?;
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if apply {
            crate::know_how::KnowHow::shared(&cx.hub()?.config)
                .await
                .forget_identity(&who.key())
                .await?;
        }
        Ok(tally)
    }
}

pub struct DecisionRecords;

#[async_trait]
impl Holding for DecisionRecords {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::counted(
            crate::decision_store::people(&cx.hub()?.config)
                .await?
                .into_iter()
                .map(|(person, held)| (person, usize::try_from(held).unwrap_or(usize::MAX))),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let held = crate::decision_store::person_records(&cx.hub()?.config, &who.names).await?;
        Ok(Tally::counted(usize::try_from(held).unwrap_or(usize::MAX)))
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            crate::decision_store::reassign_person(&cx.hub()?.config, &from.names, &into.key())
                .await?;
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if apply {
            crate::decision_store::forget_person(&cx.hub()?.config, &who.names).await?;
        }
        Ok(tally)
    }
}

pub struct Documents;

#[async_trait]
impl Holding for Documents {
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        match crate::store::global_document_store() {
            Some(store) => Ok(Owners::counted(store.user_scopes().await?)),
            None => Ok(Owners::default()),
        }
    }

    async fn held(&self, _cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let Some(store) = crate::store::global_document_store() else {
            return Ok(Tally::default());
        };
        let mut held = 0;
        for name in &who.names {
            held += store.scope_documents(&Scope::User(name.clone())).await?;
        }
        Ok(Tally::counted(held))
    }

    /// A document both hold under the same collection and id keeps the
    /// target's; the source's is reported and dropped.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = self.held(cx, from).await?;
        let Some(store) = crate::store::global_document_store().filter(|_| apply) else {
            return Ok(tally);
        };
        let to = Scope::User(into.key());
        for name in from.names.iter().filter(|n| **n != into.key()) {
            let moved = store.rescope(&Scope::User(name.clone()), &to).await?;
            tally.conflicts.extend(
                moved
                    .kept
                    .into_iter()
                    .map(|doc| format!("{doc}: kept the existing one")),
            );
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if let Some(store) = crate::store::global_document_store().filter(|_| apply) {
            for name in &who.names {
                store.drop_scope(&Scope::User(name.clone())).await?;
            }
        }
        Ok(tally)
    }
}

pub struct TrainingCorpora;

impl TrainingCorpora {
    fn owned(
        cx: &Context<'_>,
        who: &Person,
    ) -> Result<
        Vec<(
            Arc<crate::serve::AgentService>,
            workforce_training::model::Corpus,
        )>,
    > {
        let mut owned = Vec::new();
        for service in cx.services() {
            if let Some(store) = &service.training_store {
                for corpus in store.list_corpora(service.agent_id.as_str())? {
                    if matches!(&corpus.scope, CorpusScope::User(id) if who.names.contains(id)) {
                        owned.push((Arc::clone(&service), corpus));
                    }
                }
            }
        }
        Ok(owned)
    }
}

#[async_trait]
impl Holding for TrainingCorpora {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            if let Some(store) = &service.training_store {
                owners.absorb(Owners::of(
                    store
                        .list_corpora(service.agent_id.as_str())?
                        .into_iter()
                        .filter_map(|corpus| match corpus.scope {
                            CorpusScope::User(id) => Some(id),
                            _ => None,
                        }),
                ));
            }
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Ok(Tally::of(Self::owned(cx, who)?.into_iter().map(
            |(service, corpus)| format!("{}: {}", service.agent_id, corpus.name),
        )))
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            for (service, corpus) in Self::owned(cx, from)? {
                if let Some(store) = &service.training_store {
                    store.set_corpus_scope(corpus.id, &CorpusScope::User(into.key()))?;
                }
            }
        }
        Ok(tally)
    }

    /// The corpus goes with its documents, their files and the memories its
    /// chunks were recalled from, which are purged rather than hidden.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if !apply {
            return Ok(tally);
        }
        for (service, corpus) in Self::owned(cx, who)? {
            let Some(store) = service.training_store.clone() else {
                continue;
            };
            let chunks: Vec<workforce_memory::types::MemoryId> = store
                .list_chunks_for_corpus(corpus.id)?
                .into_iter()
                .map(|chunk| workforce_memory::types::MemoryId(chunk.memory_id))
                .collect();
            if let Some(memory) = &service.memory_store {
                memory.purge_memories(&chunks).await?;
            }
            for document in store.list_documents(corpus.id)? {
                crate::training::pipeline::unlink_storage_uri(&document.storage_uri).await;
            }
            store.delete_corpus(corpus.id)?;
        }
        Ok(tally)
    }
}

pub struct Artifacts;

/// Artifacts the person owns, and those shared with them by name, in one
/// backend.
struct HeldArtifacts {
    owned: Vec<(Artifact, bool)>,
    granted: Vec<(String, String)>,
}

impl Artifacts {
    /// The shared knowledge store, or each agent's file fallback that
    /// exists: opening one creates it, which a dry run must not.
    fn backends(cx: &Context<'_>) -> Vec<Arc<dyn ArtifactBackend>> {
        if let Some(store) = &cx.artifacts {
            return vec![Arc::clone(store) as Arc<dyn ArtifactBackend>];
        }
        cx.services()
            .iter()
            .filter_map(|service| FsArtifactBackend::existing_at(&service.workspace))
            .map(|backend| Arc::new(backend) as Arc<dyn ArtifactBackend>)
            .collect()
    }

    async fn held_in(backend: &dyn ArtifactBackend, who: &Person) -> Result<HeldArtifacts> {
        let mut owned = Vec::new();
        for artifact in backend
            .list(&Caller::admin(), &ArtifactFilter::default())
            .await?
            .into_iter()
            .filter(|a| who.names.contains(&a.owner_id))
        {
            let shared = artifact.visibility != ArtifactVisibility::Private
                || !backend.shares_of(&artifact.id).await?.is_empty();
            owned.push((artifact, shared));
        }
        let mut granted = Vec::new();
        for name in &who.names {
            for id in backend.grants_of(name).await? {
                granted.push((id, name.clone()));
            }
        }
        Ok(HeldArtifacts { owned, granted })
    }

    fn tally(held: &HeldArtifacts) -> Tally {
        let mut tally = Tally::of(held.owned.iter().map(|(a, _)| a.title.clone()));
        tally.transferable = held.owned.iter().filter(|(_, shared)| *shared).count();
        tally.absorb(Tally::of(
            held.granted
                .iter()
                .map(|(id, _)| format!("shared with them: {id}")),
        ));
        tally
    }
}

#[async_trait]
impl Holding for Artifacts {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for backend in Self::backends(cx) {
            for artifact in backend
                .list(&Caller::admin(), &ArtifactFilter::default())
                .await?
            {
                owners.absorb(Owners::of(backend.shares_of(&artifact.id).await?));
                owners.add(artifact.owner_id, 1);
            }
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for backend in Self::backends(cx) {
            tally.absorb(Self::tally(&Self::held_in(backend.as_ref(), who).await?));
        }
        Ok(tally)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let to = into.key();
        let mut tally = Tally::default();
        for backend in Self::backends(cx) {
            let held = Self::held_in(backend.as_ref(), from).await?;
            tally.absorb(Self::tally(&held));
            if !apply {
                continue;
            }
            for (artifact, _) in &held.owned {
                backend.set_owner(&artifact.id, &to).await?;
            }
            for (id, name) in &held.granted {
                backend.revoke_share(id, name).await?;
                let owner = backend
                    .read(id, &Caller::admin())
                    .await?
                    .map(|a| a.owner_id);
                if owner.as_deref() != Some(to.as_str()) {
                    backend.share_with(id, &to, &Caller::admin()).await?;
                }
            }
        }
        Ok(tally)
    }

    /// Private artifacts are erased; shared ones go to the recipient, or are
    /// erased when the administrator chose erase. A private artifact is
    /// never handed to anyone: it would show them the person's own notes.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        to: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for backend in Self::backends(cx) {
            let held = Self::held_in(backend.as_ref(), who).await?;
            tally.absorb(Self::tally(&held));
            if !apply {
                continue;
            }
            for (artifact, shared) in &held.owned {
                match (shared, to) {
                    (true, Some(to)) => backend.set_owner(&artifact.id, &to.key()).await?,
                    _ => backend.delete(&artifact.id, &Caller::admin()).await?,
                }
            }
            for (id, name) in &held.granted {
                backend.revoke_share(id, name).await?;
            }
        }
        Ok(tally)
    }
}

/// Installed skills are code others may run, never the person's data, so a
/// delete hands the ones they own to the recipient and never uninstalls one;
/// their own credentials and consent for a skill are in the vault and go
/// with it.
pub struct Skills;

impl Skills {
    /// Give `who`'s ownership to `to` (when given) and their grants to
    /// `grantee` (when given, else drop them), skill by skill.
    fn rewrite<R: SkillRegistry>(
        registry: &R,
        who: &Person,
        owner: Option<&str>,
        grantee: Option<&str>,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for skill in registry.all_installed() {
            let owned = skill
                .owner()
                .is_some_and(|o| who.names.iter().any(|n| n == o));
            let granted = skill.shares().iter().any(|g| named(g, who));
            if !(owned || granted) {
                continue;
            }
            let name = R::skill_name(&skill).to_string();
            tally.item(name.clone());
            tally.transferable += usize::from(owned);
            if !apply {
                continue;
            }
            let new_owner = match owned {
                true => owner.map(str::to_string),
                false => None,
            };
            let reaches_owner = new_owner.as_deref().or(skill.owner());
            let mut grants: Vec<Grant> = Vec::new();
            for grant in skill.shares() {
                let grant = match (named(grant, who), grantee) {
                    (true, Some(to)) => Grant {
                        principal_id: to.to_string(),
                        ..grant.clone()
                    },
                    (true, None) => continue,
                    (false, _) => grant.clone(),
                };
                let to_owner = grant.principal_type == PrincipalKind::User
                    && Some(grant.principal_id.as_str()) == reaches_owner;
                let duplicate = grants.iter().any(|kept| {
                    kept.principal_type == grant.principal_type
                        && kept.principal_id == grant.principal_id
                });
                if !to_owner && !duplicate {
                    grants.push(grant);
                }
            }
            registry.set_access(
                &name,
                AccessWrite {
                    owner: new_owner.map_or(OwnerWrite::Unchanged, OwnerWrite::To),
                    visibility: skill.visibility(),
                    grants,
                },
            )?;
        }
        Ok(tally)
    }

    fn registries(
        cx: &Context<'_>,
    ) -> (
        Option<Arc<workforce_wasm_runtime::WasmSkillStore>>,
        Arc<crate::external_skills::ExternalSkills>,
    ) {
        (
            crate::wasm_skill_tools::global_skill_store(),
            Arc::clone(&cx.external_skills),
        )
    }

    fn each(
        cx: &Context<'_>,
        who: &Person,
        owner: Option<&str>,
        grantee: Option<&str>,
        apply: bool,
    ) -> Result<Tally> {
        let (wasm, external) = Self::registries(cx);
        let mut tally = Self::rewrite(external.as_ref(), who, owner, grantee, apply)?;
        if let Some(wasm) = wasm {
            tally.absorb(Self::rewrite(wasm.as_ref(), who, owner, grantee, apply)?);
        }
        Ok(tally)
    }
}

/// The owner and the users granted each skill in `registry`.
fn skill_owners<R: SkillRegistry>(registry: &R) -> Owners {
    Owners::of(registry.all_installed().iter().flat_map(|skill| {
        skill
            .owner()
            .map(str::to_string)
            .into_iter()
            .chain(
                skill
                    .shares()
                    .iter()
                    .filter(|grant| grant.principal_type == PrincipalKind::User)
                    .map(|grant| grant.principal_id.clone()),
            )
            .collect::<Vec<_>>()
    }))
}

fn named(grant: &Grant, who: &Person) -> bool {
    grant.principal_type == PrincipalKind::User && who.names.contains(&grant.principal_id)
}

#[async_trait]
impl Holding for Skills {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let (wasm, external) = Self::registries(cx);
        let mut owners = skill_owners(external.as_ref());
        if let Some(wasm) = wasm {
            owners.absorb(skill_owners(wasm.as_ref()));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::each(cx, who, None, None, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let to = into.key();
        Self::each(cx, from, Some(&to), Some(&to), apply)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        to: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let to = to.map(Person::key);
        Self::each(cx, who, to.as_deref(), None, apply)
    }
}

/// Configuration that names the person: reported for an administrator to
/// change in the config editor, never rewritten. It lives in the operator's
/// file or its revisioned overlay, and changing who owns an agent or who
/// pays for install work is a decision of its own.
pub struct ConfigReferences;

impl ConfigReferences {
    /// Every reference to a person in configuration: `(whom it names, the
    /// key that names them)`.
    async fn references(cx: &Context<'_>) -> Result<Vec<(String, String)>> {
        let config = &cx.hub()?.config;
        let mut found = Vec::new();
        for agent in &config.agents {
            if let Some(owner) = &agent.owner_id {
                found.push((owner.clone(), format!("agents.{}.owner_id", agent.id)));
            }
            for platform_id in agent.user_map.keys() {
                found.push((
                    platform_id.clone(),
                    format!("agents.{}.user_map.{platform_id}", agent.id),
                ));
            }
        }
        if let Some(operator) = config.subscription.operator {
            found.push((operator.to_string(), "subscription.operator".to_string()));
        }
        for id in &config.budget.exempt_identities {
            found.push((id.clone(), "budget.exempt_identities".to_string()));
        }
        if let Some(store) = &cx.defs {
            for def in store
                .list(workforce_orchestrator::workflow_store::KIND_WORKFLOW)
                .await?
                .into_iter()
                .filter(|def| def.owner.is_none())
            {
                let Ok(parsed) = WorkflowDefinition::from_toml(&def.toml_source) else {
                    continue;
                };
                let run_as: std::collections::BTreeSet<&String> = parsed
                    .triggers
                    .iter()
                    .filter_map(|trigger| match trigger {
                        TriggerDefinition::Schedule {
                            run_as: Some(id), ..
                        } => Some(id),
                        _ => None,
                    })
                    .collect();
                for id in run_as {
                    found.push((id.clone(), format!("workflow {} trigger run_as", def.id)));
                }
            }
        }
        Ok(found)
    }

    async fn found(cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut keys: Vec<String> = Self::references(cx)
            .await?
            .into_iter()
            .filter(|(name, _)| who.names.contains(name))
            .map(|(_, key)| key)
            .collect();
        keys.dedup();
        Ok(Tally::of(keys))
    }
}

#[async_trait]
impl Holding for ConfigReferences {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::of(
            Self::references(cx)
                .await?
                .into_iter()
                .map(|(name, _)| name),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::found(cx, who).await
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        _apply: bool,
    ) -> Result<Tally> {
        Self::found(cx, from).await
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        _apply: bool,
    ) -> Result<Tally> {
        Self::found(cx, who).await
    }
}

/// Records of who did what: the vault, capability, budget-override and
/// lifecycle audits, revision editors, approval resolutions, rule and role
/// authors, team creators, research provenance. Kept, naming an identity
/// that no longer resolves to anyone once its record is gone.
pub struct AuditRecords;

#[async_trait]
impl Holding for AuditRecords {
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::default())
    }

    async fn held(&self, _cx: &Context<'_>, _who: &Person) -> Result<Tally> {
        Ok(Tally::default())
    }

    async fn merge(
        &self,
        _cx: &Context<'_>,
        _: &Person,
        _: &Person,
        _apply: bool,
    ) -> Result<Tally> {
        Ok(Tally::default())
    }

    async fn delete(
        &self,
        _cx: &Context<'_>,
        _: &Person,
        _: Option<&Person>,
        _apply: bool,
    ) -> Result<Tally> {
        Ok(Tally::default())
    }
}

/// Stores that keep nothing past a fixed window and cannot be read by
/// person: traces (keyed by session, rotated), the chat request stream
/// (7 days), OAuth redirect state (10 minutes), browser profiles (reaped
/// when idle) and task telemetry (90 days).
pub struct RetentionBound;

#[async_trait]
impl Holding for RetentionBound {
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::default())
    }

    async fn held(&self, _cx: &Context<'_>, _who: &Person) -> Result<Tally> {
        Ok(Tally::default())
    }

    async fn merge(
        &self,
        _cx: &Context<'_>,
        _: &Person,
        _: &Person,
        _apply: bool,
    ) -> Result<Tally> {
        Ok(Tally::default())
    }

    async fn delete(
        &self,
        _cx: &Context<'_>,
        _: &Person,
        _: Option<&Person>,
        _apply: bool,
    ) -> Result<Tally> {
        Ok(Tally::default())
    }
}
