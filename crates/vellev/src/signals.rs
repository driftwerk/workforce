//! What a node's refusal says beyond its code, each read in one function.
//!
//! vellev-client 0.1 gives a refusal's status, code and message only. The
//! release after it gives the rest in fields, and each function here then
//! becomes the one call its comment names, with no text read at all.

use vellev_client::api::Code;
use vellev_client::Error;

/// Whether the node said it cannot use the collection's embedding model
/// until it, or the collection, is set up otherwise: its answer with HTTP
/// 501, which vellev gives for nothing else. Its code, `model_unavailable`,
/// is one vellev-client 0.1 does not know, so the status is gone by, which
/// holds with any client. A model the node cannot use just now is 503
/// `unavailable`, and is asked about again.
///
/// With the next vellev-client: `error.code() == Some(Code::ModelUnavailable)`.
pub fn model_unavailable(error: &Error) -> bool {
    matches!(error, Error::Api { status, .. } if status.as_u16() == 501)
}

/// The most results the node said it gives for one request, from its
/// refusal of more: "limit must be from 1 to {most}, not {asked}".
///
/// With the next vellev-client: `error.limit_max(Bounded::Results)`, the
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
/// With the next vellev-client: `error.missing() == Some(Missing::Tenant)`
/// for any request; a node older than that names nothing as missing, and
/// what was asked decides, as here.
pub fn no_tenant(error: &Error, of_tenant: bool) -> bool {
    of_tenant && error.is_not_found()
}
