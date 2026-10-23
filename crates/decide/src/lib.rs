//! Small, typed decisions from a System One model.
//!
//! A System One model does not write text. It takes a *state* (any JSON) and
//! questions whose answers are fixed in advance — pick one of these labels,
//! yes or no, a level on a scale — and returns a probability for every
//! answer in one pass. TypeSafe's Jev is the hosted one; Kev is the
//! self-hostable reconstruction that speaks the same `/v1/systemone` API. To
//! this crate they are two [`Backend`]s that differ in URL and model; each
//! call brings the caller's own key through [`Keys`].
//!
//! [`Decider`] is the policy over backends: a primary, an optional fallback
//! for when the primary fails, and an optional comparison that asks both and
//! logs disagreement so a second model can be evaluated without acting on
//! it. Each backend takes a bounded number of requests at once and every
//! [`Call`] has a time budget, so an overloaded model costs its caller a
//! bounded wait and an error, never a slow turn.
//!
//! The models read short states, so each use has a pure state builder that
//! keeps what decides the answer within a [`StateBudget`], the share of
//! the [`RequestBudget`] its questions leave: [`branch`]
//! (choosing a workflow branch for an agent's step that ended with no route
//! of its own), [`request`] for [`scope`] (the pre-agent on-topic gate)
//! and [`routing`] (a request's use case), and [`supplied`] for states the
//! caller wrote, which is what [`first`] (an agent step's outcome, asked
//! before its turn runs) reads too. Every request is built in one place, [`Input::request`],
//! which the hub calls to ask and [`export`] calls over recorded examples
//! to produce training data.

pub mod branch;
pub mod call;
pub mod client;
pub mod decider;
pub mod export;
pub mod first;
pub mod input;
pub mod question;
pub mod request;
pub mod routing;
pub mod scope;
pub mod supplied;
pub mod text;

pub use call::{Call, Use};
pub use client::{Backend, DecideError, Keys, SystemOneClient};
pub use decider::{Decider, DeciderPolicy, Decision};
pub use input::{Input, Request};
pub use question::{Answer, NoulCriteria, Question, Response, Usage};
pub use text::{RequestBudget, StateBudget};
