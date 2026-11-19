//! What a node's refusal says beyond its code, each read in one function.
//!
//! A node older than 0.2 gives a refusal's status, code and message only,
//! and these read what it says from them. Once every node is 0.2 or later,
//! each becomes the one call its comment names, with no text read at all.

use vellev_client::api::Code;
use vellev_client::Error;

/// The most results the node said it gives for one request, from its
/// refusal of more: "limit must be from 1 to {most}, not {asked}".
///
/// With every node 0.2 or later: `error.limit_max(Bounded::Results)`, the
/// cap starting from `Client::limits()`; this text is then not read.
pub fn most_results(error: &Error) -> Option<usize> {
    if error.code() != Some(Code::Invalid) {
        return None;
    }
    let text = error.to_string();
    let (_, rest) = text.split_once("limit must be from 1 to ")?;
    let most: String = rest.chars().take_while(char::is_ascii_digit).collect();
    most.parse().ok().filter(|most| *most > 0)
}

/// Whether a refusal says the tenant does not exist, or is not one the key
/// reaches: a request `of_tenant` itself (listing its collections) that was
/// not found. One about a collection or an object says nothing of the
/// tenant, whatever its message.
///
/// With every node 0.2 or later: `error.missing() == Some(Missing::Tenant)`
/// for any request; an older node names nothing as missing, and what was
/// asked decides, as here.
pub fn no_tenant(error: &Error, of_tenant: bool) -> bool {
    of_tenant && error.is_not_found()
}
