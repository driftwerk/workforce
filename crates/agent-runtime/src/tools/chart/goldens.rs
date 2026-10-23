//! Parity with the widget, held by the shared golden corpus.
//!
//! `fixtures/charts/*.json` is generated from the TypeScript validator (the
//! shipped implementation, and therefore the reference) by `npm run goldens`
//! in `crates/edge/widget-ui`. This suite asserts the Rust port accepts,
//! normalises and rejects every case exactly as the widget does.

use serde_json::json;

use super::spec::validate;
use crate::tools::goldens::{canonical, corpus, first_difference};

#[test]
fn every_chart_golden_validates_exactly_as_the_widget_does() {
    for (name, body) in corpus("charts", 12) {
        let ours = match validate(&body["input"]) {
            Ok(spec) => {
                json!({ "ok": true, "spec": serde_json::to_value(spec).expect("serializable") })
            }
            Err(error) => json!({ "ok": false, "error": error }),
        };
        if let Some(where_) =
            first_difference(&canonical(&ours), &canonical(&body["result"]), &name)
        {
            panic!("validation differs from the widget at {where_}");
        }
    }
}
