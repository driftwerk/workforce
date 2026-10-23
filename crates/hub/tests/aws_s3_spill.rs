//! Spill integration test for `aws.s3.get` (design §2.6, §4).
//!
//! Constructs the output shape the `get` tool emits on a CSV body and
//! drives it through `spill_tool_output`. The shape must trigger
//! a rows path (`rows`) so the row bytes land in the artifact store
//! and only the digest envelope reaches the transcript — this is the
//! load-bearing proof of the PII-surface claim.

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{json, Value};
use tempfile::TempDir;

use workforce_agent::artifacts::store::{set_global_store, FsArtifactStore};
use workforce_agent::artifacts::TABULAR_ARTIFACT_KIND;
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_ID};
use workforce_agent::spill::{spill_tool_output, RetrievalSurface};
use workforce_common::session::SpillConfig;
use workforce_hub::aws::content::parse_csv;

fn install_store() -> Arc<TempDir> {
    static INIT: std::sync::Once = std::sync::Once::new();
    static DIR: std::sync::OnceLock<Arc<TempDir>> = std::sync::OnceLock::new();
    INIT.call_once(|| {
        let dir = Arc::new(TempDir::new().expect("tempdir"));
        let store = FsArtifactStore::new(dir.path().to_path_buf()).expect("artifact store");
        set_global_store(store);
        let _ = DIR.set(dir);
    });
    DIR.get().expect("init").clone()
}

fn big_csv_output_shape() -> Value {
    let header = "name,role,trust_id\n";
    let row = "alice-padded-name-x,clerk-band-5,site-0123\n";
    let mut body = String::with_capacity(header.len() + row.len() * 2000);
    body.push_str(header);
    for _ in 0..2000 {
        body.push_str(row);
    }
    let (columns, rows) = parse_csv(&Bytes::from(body)).expect("csv parses");
    json!({
        "bucket": "example-dashboard-staging-bucket",
        "key": "trust-0123/main.csv",
        "content_type": "text/csv",
        "kind": "csv",
        "row_count": rows.len(),
        "columns": columns,
        "rows": rows,
    })
}

#[test]
fn csv_get_output_spills_rows_to_artifact_store() {
    let _dir = install_store();
    let output = big_csv_output_shape();
    let output_str = output.to_string();
    assert!(
        output_str.len() > SpillConfig::default().threshold_bytes,
        "test fixture must exceed spill threshold"
    );

    let (spilled_str, spilled) = CURRENT_AGENT_ID.sync_scope("test-agent".to_string(), || {
        CURRENT_SESSION_ID.sync_scope("s3-spill-session".to_string(), || {
            spill_tool_output(
                "aws.s3.get",
                output_str,
                output.clone(),
                false,
                &SpillConfig::default(),
                RetrievalSurface::FULL,
            )
        })
    });

    assert_eq!(
        spilled["rows"]["kind"], TABULAR_ARTIFACT_KIND,
        "rows field replaced by tabular artifact envelope; raw rows no longer in transcript",
    );
    assert!(
        spilled["rows"]["artifact_id"].is_string(),
        "envelope carries an artifact_id",
    );

    assert_eq!(spilled["kind"], "csv");
    assert_eq!(spilled["row_count"], 2000);
    assert_eq!(spilled["columns"], json!(["name", "role", "trust_id"]));
    assert_eq!(spilled["bucket"], output["bucket"]);
    assert_eq!(spilled["key"], output["key"]);

    assert_eq!(
        spilled_str,
        spilled.to_string(),
        "in-flight string and invocation value share the same digest",
    );
    assert!(
        spilled_str.len() < output.to_string().len() / 4,
        "digest is materially smaller than the original payload",
    );
}

#[test]
fn small_csv_output_passes_through_unchanged() {
    let _dir = install_store();
    let csv = b"a,b\n1,2\n";
    let (columns, rows) = parse_csv(&Bytes::from_static(csv)).expect("csv parses");
    let output = json!({
        "bucket": "example-dashboard-staging-bucket",
        "key": "tiny.csv",
        "content_type": "text/csv",
        "kind": "csv",
        "row_count": rows.len(),
        "columns": columns,
        "rows": rows,
    });
    let output_str = output.to_string();

    let (out_str, out) = CURRENT_AGENT_ID.sync_scope("test-agent".to_string(), || {
        spill_tool_output(
            "aws.s3.get",
            output_str.clone(),
            output.clone(),
            false,
            &SpillConfig::default(),
            RetrievalSurface::FULL,
        )
    });

    assert_eq!(out, output, "below threshold → unchanged");
    assert_eq!(out_str, output_str);
}
