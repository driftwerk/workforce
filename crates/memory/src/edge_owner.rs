//! Which identities the customer-facing edge keys memories by.
//!
//! The edge keeps three kinds of object in its memory store, each under an
//! agent of its own: a customer's memories, under the customer's owner key
//! ([`external_owner`]); its lesson book, under [`LESSON_IDENTITY`]; and its
//! knowledge chunks, under [`KNOWLEDGE_IDENTITY`]. They are defined here, not
//! in the edge, so that the hub's one-off move of the edge's memories out of
//! the hub's collections (DW-676 (b)) picks them out by exactly the edge's
//! rule.

/// What every customer's owner key starts with.
const CUSTOMER_PREFIX: &str = "ext:";

/// The fixed identity scope every edge lesson is written and recalled under.
/// One shared lesson book per agent — never a customer's identity.
pub const LESSON_IDENTITY: &str = "edge:lessons";

/// The fixed identity every edge knowledge chunk is stored and recalled under.
pub const KNOWLEDGE_IDENTITY: &str = "edge:knowledge";

/// The collision-free session owner for a verified customer: `ext:` + the realm
/// issuer + the subject. Distinct from any internal UUID owner, and unique
/// across realms, so a customer can only ever address their own sessions.
pub fn external_owner(issuer: &str, sub: &str) -> String {
    format!("{CUSTOMER_PREFIX}{issuer}\u{1f}{sub}")
}

/// Whether `key` is a customer's owner key, as [`external_owner`] makes it: an
/// issuer and a subject, neither empty. Lessons, training data and any other
/// identity the edge keys by have another shape, so erasure that reaches only
/// these reaches only customers.
pub fn is_customer_owner(key: &str) -> bool {
    key.strip_prefix(CUSTOMER_PREFIX)
        .and_then(|rest| rest.split_once('\u{1f}'))
        .is_some_and(|(issuer, sub)| !issuer.is_empty() && !sub.is_empty())
}

/// Whether the edge writes memories under `identity`: a customer's, or the
/// edge's own lesson book or knowledge.
pub fn is_edge_identity(identity: &str) -> bool {
    is_customer_owner(identity) || identity == LESSON_IDENTITY || identity == KNOWLEDGE_IDENTITY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_customer_key_has_an_issuer_and_a_subject() {
        assert!(is_customer_owner(&external_owner("https://iss", "sub-1")));
        assert!(!is_customer_owner("ext:\u{1f}sub"));
        assert!(!is_customer_owner("ext:iss\u{1f}"));
        assert!(!is_customer_owner("ext:cust-9"), "a hub payer's key");
        assert!(!is_customer_owner("allan"));
    }

    #[test]
    fn the_edge_writes_under_customers_lessons_and_knowledge_alone() {
        assert!(is_edge_identity(&external_owner("iss", "sub")));
        assert!(is_edge_identity(LESSON_IDENTITY));
        assert!(is_edge_identity(KNOWLEDGE_IDENTITY));
        assert!(!is_edge_identity("edge:other"));
        assert!(!is_edge_identity("ext:cust-9"));
        assert!(!is_edge_identity(""));
    }
}
