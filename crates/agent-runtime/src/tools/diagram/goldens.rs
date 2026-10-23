//! Parity with the widget, held by the shared golden corpus.
//!
//! `fixtures/diagrams/*.json` is generated from the TypeScript engine (the
//! shipped implementation, and therefore the reference) by `npm run goldens` in
//! `crates/edge/widget-ui`. This suite asserts the Rust port produces the same
//! layout for the same spec.
//!
//! This is the only test that can catch the failure that actually matters: a
//! diagram in a report or a Slack PNG that disagrees with the one in the widget.
//! Invariant tests cannot — two layouts can each satisfy every invariant and
//! still place things differently.

use super::layout::{layout, Metrics};
use super::spec::validate;
use crate::tools::goldens::{canonical, corpus, first_difference};

#[test]
fn every_golden_lays_out_exactly_as_the_widget_does() {
    for (name, body) in corpus("diagrams", 10) {
        let spec = validate(&body["spec"]).unwrap_or_else(|e| panic!("{name}: {e}"));
        let metrics = Metrics {
            char_width: body["metrics"]["char_width"].as_f64().expect("char_width"),
            line_height: body["metrics"]["line_height"]
                .as_f64()
                .expect("line_height"),
        };

        let ours = canonical(&serde_json::to_value(layout(&spec, metrics)).expect("serializable"));
        let theirs = canonical(&body["layout"]);
        if let Some(where_) = first_difference(&ours, &theirs, &name) {
            panic!("layout differs from the widget at {where_}");
        }
    }
}
