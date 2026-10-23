//! A run's step history passes between builds during every rolling deploy
//! and every rollback. `decide_first_history.json` is what this build's hub
//! stored for runs whose step asked the decision model first (settled onto
//! its default route and onto a branch, in shadow, below the floor);
//! `…_resaved_by_f085ab0f4.json` is the same runs as the build before
//! `decide_first` read and saved them again. Neither side may lose a step.

use serde_json::Value;
use workforce_orchestrator::workflow::instance::{OutcomeBy, StepRecord};

const WRITTEN: &str = include_str!("fixtures/decide_first_history.json");
const RESAVED_BY_OLDER: &str =
    include_str!("fixtures/decide_first_history_resaved_by_f085ab0f4.json");

fn runs(fixture: &str) -> serde_json::Map<String, Value> {
    serde_json::from_str(fixture).unwrap()
}

/// What `instance_of` reads and `save_instance` writes.
fn read_and_resave(history: &Value) -> (Vec<StepRecord>, Value) {
    let records: Vec<StepRecord> = serde_json::from_value(history.clone()).unwrap();
    let saved = serde_json::to_value(&records).unwrap();
    (records, saved)
}

#[test]
fn an_older_build_keeps_every_step_this_build_wrote_and_drops_only_decided_first() {
    let resaved = runs(RESAVED_BY_OLDER);
    for (name, written) in runs(WRITTEN) {
        let mut expected = written.as_array().unwrap().clone();
        for record in &mut expected {
            record.as_object_mut().unwrap().remove("decided_first");
        }
        assert_eq!(resaved[&name], Value::Array(expected), "{name}");
    }
}

#[test]
fn this_build_reads_what_an_older_build_saved_and_saves_it_back_unchanged() {
    for (name, history) in runs(RESAVED_BY_OLDER) {
        let (records, saved) = read_and_resave(&history);
        assert_eq!(saved, history, "{name}");
        assert!(records.iter().all(|r| r.provenance.decided_first.is_none()));
        if name.starts_with("settled") {
            assert_eq!(
                records[0].provenance.outcome_by,
                Some(OutcomeBy::DecisionModel),
                "{name}: read as the decision model's pick, which teaches nothing"
            );
        }
    }
}

#[test]
fn this_build_saves_back_exactly_what_it_wrote() {
    for (name, history) in runs(WRITTEN) {
        let (records, saved) = read_and_resave(&history);
        assert_eq!(saved, history, "{name}");
        assert_eq!(
            records[0].provenance.settled_first(),
            name.starts_with("settled"),
            "{name}"
        );
    }
}
