//! Server-side `wf-diagram`: the widget's diagram engine, ported so hub agents
//! can put a diagram in a report or hand one over as a file.
//!
//! The port is held to the widget by a shared golden corpus rather than by
//! description — see `goldens.rs`.

pub mod draw;
pub mod excalidraw;
pub mod layout;
pub mod spec;
pub mod svg;
pub mod tool;

pub use tool::{DiagramTool, DIAGRAM_TOOL_NAME};

#[cfg(test)]
mod goldens;
