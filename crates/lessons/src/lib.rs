//! Lessons — durable operational knowledge an agent extracts from its own
//! conversations and recalls on later turns, so it improves over time.
//!
//! Lifted from the hub so any host (hub channels, the internet-facing edge) can
//! run the same pipeline: a cheap learning-signal gate, LLM extraction +
//! validation, dedup/supersede against the memory store, and prompt-side recall
//! filtering. Lessons are ordinary memories at `MemoryLayer::Semantic` with
//! `MemorySource::Lesson`; storage is whatever `MemoryStore` the host runs.
//!
//! This crate is deliberately host-agnostic: it needs an `LlmProvider`, a
//! `MemoryStore`, and the turn's messages — no scheduler, no channels, no DB of
//! its own.

pub mod lessons;
pub mod turn_extraction;
