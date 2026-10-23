//! Content-addressed store for large tool outputs.
//!
//! Producers (Metabase queries, future bulk-data tools) call
//! [`store::maybe_store`] to keep small results inline and offload large
//! ones. Consumers (Sheets / Drive write APIs) accept either inline data
//! or an `artifact_id` via [`resolve::resolve_rows_input`] /
//! [`resolve::resolve_bytes_input`]. The agent passes the small handle
//! envelope across rounds without ever holding the bulk payload in its
//! LLM context — which is what prevents the re-query loop the runtime
//! used to trigger when emergency truncation erased the bulk row list.

pub mod blob;
pub mod disposition;
pub mod resolve;
#[cfg(feature = "s3")]
pub mod s3_store;
pub mod store;
pub mod stream;
pub mod tabular;
pub mod types;

pub use blob::{stage_blob, value_to_text, CLIPPED_ARTIFACT_KIND};
pub use resolve::{
    read_session_scoped, resolve_bytes_input, resolve_rows_input, BytesRequest, ResolvedBytes,
    Rows, RowsRequest,
};
pub use store::{
    global_store, maybe_store, set_global_store, ArtifactStore, FsArtifactStore, ATTACHMENT_TTL,
    DEFAULT_INLINE_THRESHOLD, DEFAULT_TTL,
};
pub use stream::{
    download_filename, download_marker, export_csv, stage_file, staging_tempfile, CsvExport, Flow,
    RowProducer, RowSink, StreamCaps,
};
pub use tabular::{
    derive_schema, normalize_columns, rows_to_cells, stage_table, stage_table_with_ttl,
    staged_to_output, zip_table, StagedTable, TABULAR_ARTIFACT_KIND,
};
pub use types::{
    ArtifactBody, ArtifactDescriptor, ArtifactHandle, ArtifactId, ContentType, MaybeArtifact,
};
