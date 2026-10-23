//! Who a heartbeat check runs as. Check files live in the agent's workspace,
//! where the agent's own file tools can write, so the hub signs a check's
//! creator together with everything else in the check. A check runs as its
//! creator only while that signature verifies for its current content; an
//! unsigned, tampered, legacy or hand-written check runs as the `heartbeat`
//! sentinel.

use std::path::Path;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::warn;
use uuid::Uuid;
use zeroize::Zeroizing;

use workforce_common::heartbeat_check::{HeartbeatCheck, HeartbeatCheckMeta};
use workforce_common::settings::WorkforceConfig;
use workforce_common::subscription::Payer;

/// HKDF `info` separating the check-signing key from every other use of the
/// vault master key.
const SIGNING_INFO: &[u8] = b"workforce-heartbeat/v1/creator";

/// Signs and verifies the creators of one agent's heartbeat checks.
#[derive(Clone)]
pub struct CheckSigner {
    agent_id: String,
    mac: Hmac<Sha256>,
}

impl CheckSigner {
    /// The signer for `agent_id`'s checks, keyed by an HKDF-SHA256 subkey of
    /// the hub's vault master key (`WORKFORCE_VAULT_KEY`, else the vault key
    /// file), which every instance of a hub shares. `None` when the hub has
    /// no vault key: no check then runs as a person.
    pub fn from_config(config: &WorkforceConfig, agent_id: &str) -> Option<Self> {
        crate::vault::Vault::read_master_key(
            config.vault.key_file.as_deref().map(Path::new),
            Path::new(&config.hub.data_dir),
        )
        .inspect_err(|e| warn!(error = %e, "Heartbeat check signing key unavailable"))
        .ok()
        .flatten()
        .map(|master| Self::from_key(master.as_bytes(), agent_id))
    }

    fn from_key(master: &[u8], agent_id: &str) -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        Hkdf::<Sha256>::new(None, master)
            .expand(SIGNING_INFO, key.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self {
            agent_id: agent_id.to_string(),
            mac: <Hmac<Sha256> as Mac>::new_from_slice(key.as_slice())
                .expect("HMAC takes a key of any length"),
        }
    }

    fn mac_over(&self, check: &HeartbeatCheck) -> Hmac<Sha256> {
        let mut mac = self.mac.clone();
        mac.update(&signed_content(&self.agent_id, check));
        mac
    }

    /// The person `check` runs as: its creator, while the check's signature
    /// verifies for its current content.
    pub fn creator(&self, check: &HeartbeatCheck) -> Option<Uuid> {
        let signature = hex::decode(check.meta.creator_sig.as_deref()?).ok()?;
        self.mac_over(check).verify_slice(&signature).ok()?;
        Payer::classify(check.meta.creator.as_deref()).person()
    }
}

/// Stamp `check` as written through the hub by `editor`: its creator becomes
/// the editor when that is a person, signed over the check as it now stands;
/// any other editor leaves it with no creator, so it runs as the `heartbeat`
/// sentinel. Every hub path that creates or edits a check calls this last,
/// just before saving.
pub fn stamp_creator(
    signer: Option<&CheckSigner>,
    check: &mut HeartbeatCheck,
    editor: Option<&str>,
) {
    check.meta.creator = Payer::classify(editor)
        .person()
        .map(|person| person.to_string());
    check.meta.creator_sig = None;
    check.meta.creator_sig = signer
        .filter(|_| check.meta.creator.is_some())
        .map(|signer| hex::encode(signer.mac_over(check).finalize().into_bytes()));
}

/// What a check's signature covers: a version, the agent, the check's id, and
/// every frontmatter field but the signature itself, plus the body as it reads
/// back from disk. Unset fields are left out, so a field added later leaves
/// existing signatures valid.
fn signed_content(agent_id: &str, check: &HeartbeatCheck) -> Vec<u8> {
    let mut content = serde_json::json!({
        "v": 1,
        "agent": agent_id,
        "id": check.id,
        "meta": HeartbeatCheckMeta {
            creator_sig: None,
            ..check.meta.clone()
        },
        "body": check.body.trim(),
    });
    if let Some(meta) = content["meta"].as_object_mut() {
        meta.retain(|_, value| !value.is_null());
    }
    content.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::heartbeat_check::{parse_heartbeat_check, serialize_heartbeat_check};

    const ALICE: &str = "6f1c1c3e-2b8a-4c55-9a3e-0d8f4b1e7a10";

    fn check(body: &str) -> HeartbeatCheck {
        parse_heartbeat_check("inbox", &format!("+++\nname = \"Inbox\"\n+++\n\n{body}")).unwrap()
    }

    fn reloaded(check: &HeartbeatCheck) -> HeartbeatCheck {
        parse_heartbeat_check(&check.id, &serialize_heartbeat_check(check).unwrap()).unwrap()
    }

    #[test]
    fn a_stamped_check_verifies_after_a_round_trip_through_its_file() {
        let signer = CheckSigner::from_key(b"master", "allan");
        let mut stamped = HeartbeatCheck {
            body: "Anything new?\n\n".to_string(),
            ..check("")
        };
        stamp_creator(Some(&signer), &mut stamped, Some(ALICE));

        assert_eq!(
            signer.creator(&reloaded(&stamped)),
            Some(Uuid::parse_str(ALICE).unwrap())
        );
    }

    #[test]
    fn a_signature_is_bound_to_its_key_agent_and_check_id() {
        let signer = CheckSigner::from_key(b"master", "allan");
        let mut stamped = check("Anything new?");
        stamp_creator(Some(&signer), &mut stamped, Some(ALICE));

        assert_eq!(
            CheckSigner::from_key(b"other", "allan").creator(&stamped),
            None
        );
        assert_eq!(
            CheckSigner::from_key(b"master", "sarah").creator(&stamped),
            None
        );
        let copied = HeartbeatCheck {
            id: "copy".into(),
            ..stamped
        };
        assert_eq!(signer.creator(&copied), None);
    }

    #[test]
    fn a_non_person_editor_leaves_no_creator_or_signature() {
        let signer = CheckSigner::from_key(b"master", "allan");
        let mut stamped = check("Anything new?");
        stamp_creator(Some(&signer), &mut stamped, Some(ALICE));
        stamp_creator(Some(&signer), &mut stamped, Some("heartbeat"));

        assert_eq!(stamped.meta.creator, None);
        assert_eq!(stamped.meta.creator_sig, None);
        assert_eq!(signer.creator(&stamped), None);
    }
}
