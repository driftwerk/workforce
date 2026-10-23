//! `xero.export` — bulk Xero data → `.xlsx`, delivered as a Slack file.
//!
//! The interactive `xero.*_list` tools return one 100-row page each, straight
//! into the model's context — so dumping a whole ledger means hundreds of model
//! round-trips and no spreadsheet at the end. This tool does the whole job
//! server-side in one call: fetch every page (bounded-concurrency, rate-limit
//! aware), flatten the nested Xero rows to a table, optionally reshape it with a
//! caller-supplied [`Layout`], write an `.xlsx`, and stage it as an artifact the
//! runner delivers to the originating channel — the same path `browser.pdf`
//! uses. The row data never enters the model's context.
//!
//! Per-user scope: credentials resolve per request from `CURRENT_USER_ID` (a
//! tokio task-local). Concurrency is driven by `buffer_unordered` **inside**
//! `execute` — never `tokio::spawn`, which would drop the task-local and resolve
//! the wrong user's credentials.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::{json, Value};

use workforce_agent::artifacts::{
    download_filename, download_marker, export_csv, global_store, normalize_columns, stage_table,
    staged_to_output, zip_table, ArtifactBody, ArtifactDescriptor, ArtifactStore, ContentType,
    Flow, RowProducer, RowSink, StreamCaps, DEFAULT_INLINE_THRESHOLD,
};
use workforce_agent::consolidation::resolve_consolidation_template;
use workforce_agent::export::{
    apply_layout, build_xlsx, xlsx_filename, Cell, ColFormat, Column, Layout, Sheet, Table,
    EXCEL_MAX_ROWS, XLSX_MIME,
};
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::XeroClient;
use super::report::{
    consolidate, consolidate_mapped, parse_report, render, ConsolidationTemplate, ReportLayout,
    ReportModel,
};
use crate::adapters::utils::coerce_to_u64;

type Clients = Arc<HashMap<String, Arc<XeroClient>>>;

pub(super) const EXPORT_TOOL_NAME: &str = "xero.export";

/// Default per-dataset cap on *source records* fetched (invoices, contacts, …);
/// bounds Xero API spend and memory. Overridable via `max_records` up to
/// [`HARD_MAX_RECORDS`]. Note this caps records, not spreadsheet rows: at
/// line-item grain one record can expand to several rows (also why the output
/// is independently bounded by [`EXCEL_MAX_ROWS`]).
const DEFAULT_MAX_RECORDS: usize = 50_000;
const HARD_MAX_RECORDS: usize = 250_000;

/// Concurrent page fetches per wave. Xero allows ~5 concurrent requests per
/// tenant; staying at the ceiling is "as fast as Xero permits" without
/// tripping 429s on every wave.
const EXPORT_CONCURRENCY: usize = 5;

/// Xero's max page size; also the short-page sentinel for pagination
/// termination (a page with fewer rows is the last one).
const EXPORT_PAGE_SIZE: u64 = 100;

const EXPORT_DESCRIPTION: &str = "Export Xero data to an .xlsx spreadsheet and deliver it to this \
    conversation as a downloadable file. Fetches the data server-side and writes it straight to \
    Excel — use this instead of paging through the xero.*_list tools whenever the user wants a bulk \
    download or a spreadsheet. `datasets` selects what to export, one worksheet each. Transactional \
    datasets — invoices, contacts, bank_transactions, manual_journals — accept `where`/`order` \
    filters, `grain` (line_item or document), and an optional `layout` reshape (select/rename/\
    reorder, group_by + aggregate, sort, totals_row), e.g. to match the headers of an example \
    workbook the user uploaded (those headers are visible to you as text). Report datasets — \
    profit_and_loss, balance_sheet, trial_balance — come out properly structured (sections, \
    indented line items, bold subtotals) from `from_date`/`to_date` (P&L) or `date` (balance \
    sheet / trial balance) plus optional `periods`/`timeframe`; list `entities` to put each org on \
    its own tab in one workbook, and set `consolidate:true` to also append a summed Consolidated \
    tab per report — sum-by-label, or name a saved `consolidation_template` to map each entity's \
    accounts to your own canonical lines (e.g. one consolidated P&L across a group's entities). \
    Don't mix report and \
    transactional datasets in one call. The file is delivered out-of-band: you will NOT see it and \
    the rows do NOT enter your context, so don't re-fetch or try to read/verify the data — just \
    tell the user the file is ready. `format:\"artifact\"` stages each sheet as a tabular_artifact \
    instead of delivering a file — use it when the data needs transforming with data.query first. \
    `format:\"csv\"` streams one transactional dataset straight to a CSV file without buffering it \
    in memory — prefer it for very large pulls; it takes a single dataset and no `layout`.";

/// One row of a dataset is sourced either from the record itself or from one of
/// its line items.
enum Src {
    Record(&'static [&'static str]),
    Line(&'static [&'static str]),
}

/// A single output column: where its value comes from and how it renders.
struct ColSpec {
    key: &'static str,
    header: &'static str,
    format: ColFormat,
    src: Src,
}

/// A bulk-exportable Xero dataset.
struct DatasetDef {
    path: &'static str,
    /// Top-level array key in the response envelope (e.g. `"Invoices"`).
    envelope: &'static str,
    /// Whether the endpoint accepts `pageSize` (else `page`-only paging).
    supports_page_size: bool,
    /// The line-item array key, when rows can be expanded per line.
    line_array: Option<&'static str>,
    cols: &'static [ColSpec],
}

/// Row grain for datasets that carry line items.
#[derive(Clone, Copy, PartialEq)]
enum Grain {
    LineItem,
    Document,
}

struct Filter {
    where_: Option<String>,
    order: Option<String>,
}

static INVOICE_COLS: &[ColSpec] = &[
    ColSpec {
        key: "Type",
        header: "Type",
        format: ColFormat::General,
        src: Src::Record(&["Type"]),
    },
    ColSpec {
        key: "InvoiceNumber",
        header: "Invoice Number",
        format: ColFormat::General,
        src: Src::Record(&["InvoiceNumber"]),
    },
    ColSpec {
        key: "Reference",
        header: "Reference",
        format: ColFormat::General,
        src: Src::Record(&["Reference"]),
    },
    ColSpec {
        key: "Contact",
        header: "Contact",
        format: ColFormat::General,
        src: Src::Record(&["Contact", "Name"]),
    },
    ColSpec {
        key: "Date",
        header: "Date",
        format: ColFormat::Date,
        src: Src::Record(&["Date"]),
    },
    ColSpec {
        key: "DueDate",
        header: "Due Date",
        format: ColFormat::Date,
        src: Src::Record(&["DueDate"]),
    },
    ColSpec {
        key: "Status",
        header: "Status",
        format: ColFormat::General,
        src: Src::Record(&["Status"]),
    },
    ColSpec {
        key: "Currency",
        header: "Currency",
        format: ColFormat::General,
        src: Src::Record(&["CurrencyCode"]),
    },
    ColSpec {
        key: "Total",
        header: "Total",
        format: ColFormat::Currency,
        src: Src::Record(&["Total"]),
    },
    ColSpec {
        key: "AmountDue",
        header: "Amount Due",
        format: ColFormat::Currency,
        src: Src::Record(&["AmountDue"]),
    },
    ColSpec {
        key: "AmountPaid",
        header: "Amount Paid",
        format: ColFormat::Currency,
        src: Src::Record(&["AmountPaid"]),
    },
    ColSpec {
        key: "InvoiceID",
        header: "Invoice ID",
        format: ColFormat::General,
        src: Src::Record(&["InvoiceID"]),
    },
    ColSpec {
        key: "LineDescription",
        header: "Line Description",
        format: ColFormat::General,
        src: Src::Line(&["Description"]),
    },
    ColSpec {
        key: "LineQuantity",
        header: "Qty",
        format: ColFormat::General,
        src: Src::Line(&["Quantity"]),
    },
    ColSpec {
        key: "LineUnitAmount",
        header: "Unit Amount",
        format: ColFormat::Currency,
        src: Src::Line(&["UnitAmount"]),
    },
    ColSpec {
        key: "LineAccountCode",
        header: "Account Code",
        format: ColFormat::General,
        src: Src::Line(&["AccountCode"]),
    },
    ColSpec {
        key: "LineTaxType",
        header: "Tax Type",
        format: ColFormat::General,
        src: Src::Line(&["TaxType"]),
    },
    ColSpec {
        key: "LineAmount",
        header: "Line Amount",
        format: ColFormat::Currency,
        src: Src::Line(&["LineAmount"]),
    },
];

static CONTACT_COLS: &[ColSpec] = &[
    ColSpec {
        key: "Name",
        header: "Name",
        format: ColFormat::General,
        src: Src::Record(&["Name"]),
    },
    ColSpec {
        key: "FirstName",
        header: "First Name",
        format: ColFormat::General,
        src: Src::Record(&["FirstName"]),
    },
    ColSpec {
        key: "LastName",
        header: "Last Name",
        format: ColFormat::General,
        src: Src::Record(&["LastName"]),
    },
    ColSpec {
        key: "Email",
        header: "Email",
        format: ColFormat::General,
        src: Src::Record(&["EmailAddress"]),
    },
    ColSpec {
        key: "Status",
        header: "Status",
        format: ColFormat::General,
        src: Src::Record(&["ContactStatus"]),
    },
    ColSpec {
        key: "AccountNumber",
        header: "Account Number",
        format: ColFormat::General,
        src: Src::Record(&["AccountNumber"]),
    },
    ColSpec {
        key: "IsCustomer",
        header: "Customer",
        format: ColFormat::General,
        src: Src::Record(&["IsCustomer"]),
    },
    ColSpec {
        key: "IsSupplier",
        header: "Supplier",
        format: ColFormat::General,
        src: Src::Record(&["IsSupplier"]),
    },
    ColSpec {
        key: "ContactID",
        header: "Contact ID",
        format: ColFormat::General,
        src: Src::Record(&["ContactID"]),
    },
];

static BANK_TXN_COLS: &[ColSpec] = &[
    ColSpec {
        key: "Type",
        header: "Type",
        format: ColFormat::General,
        src: Src::Record(&["Type"]),
    },
    ColSpec {
        key: "Date",
        header: "Date",
        format: ColFormat::Date,
        src: Src::Record(&["Date"]),
    },
    ColSpec {
        key: "Status",
        header: "Status",
        format: ColFormat::General,
        src: Src::Record(&["Status"]),
    },
    ColSpec {
        key: "Reference",
        header: "Reference",
        format: ColFormat::General,
        src: Src::Record(&["Reference"]),
    },
    ColSpec {
        key: "Contact",
        header: "Contact",
        format: ColFormat::General,
        src: Src::Record(&["Contact", "Name"]),
    },
    ColSpec {
        key: "BankAccount",
        header: "Bank Account",
        format: ColFormat::General,
        src: Src::Record(&["BankAccount", "Name"]),
    },
    ColSpec {
        key: "Total",
        header: "Total",
        format: ColFormat::Currency,
        src: Src::Record(&["Total"]),
    },
    ColSpec {
        key: "BankTransactionID",
        header: "Transaction ID",
        format: ColFormat::General,
        src: Src::Record(&["BankTransactionID"]),
    },
    ColSpec {
        key: "LineDescription",
        header: "Line Description",
        format: ColFormat::General,
        src: Src::Line(&["Description"]),
    },
    ColSpec {
        key: "LineAmount",
        header: "Line Amount",
        format: ColFormat::Currency,
        src: Src::Line(&["LineAmount"]),
    },
    ColSpec {
        key: "LineAccountCode",
        header: "Account Code",
        format: ColFormat::General,
        src: Src::Line(&["AccountCode"]),
    },
];

static MANUAL_JOURNAL_COLS: &[ColSpec] = &[
    ColSpec {
        key: "Date",
        header: "Date",
        format: ColFormat::Date,
        src: Src::Record(&["Date"]),
    },
    ColSpec {
        key: "Status",
        header: "Status",
        format: ColFormat::General,
        src: Src::Record(&["Status"]),
    },
    ColSpec {
        key: "Narration",
        header: "Narration",
        format: ColFormat::General,
        src: Src::Record(&["Narration"]),
    },
    ColSpec {
        key: "ManualJournalID",
        header: "Journal ID",
        format: ColFormat::General,
        src: Src::Record(&["ManualJournalID"]),
    },
    ColSpec {
        key: "LineDescription",
        header: "Line Description",
        format: ColFormat::General,
        src: Src::Line(&["Description"]),
    },
    ColSpec {
        key: "LineAccountCode",
        header: "Account Code",
        format: ColFormat::General,
        src: Src::Line(&["AccountCode"]),
    },
    ColSpec {
        key: "LineAmount",
        header: "Line Amount",
        format: ColFormat::Currency,
        src: Src::Line(&["LineAmount"]),
    },
    ColSpec {
        key: "LineTaxType",
        header: "Tax Type",
        format: ColFormat::General,
        src: Src::Line(&["TaxType"]),
    },
];

/// The field paths above target the documented Xero JSON shapes; an absent
/// field degrades to a blank cell (never a panic), so a schema drift shows up
/// as an empty column rather than a failed export.
fn dataset_def(name: &str) -> Option<DatasetDef> {
    Some(match name {
        "invoices" => DatasetDef {
            path: "/Invoices",
            envelope: "Invoices",
            supports_page_size: true,
            line_array: Some("LineItems"),
            cols: INVOICE_COLS,
        },
        "contacts" => DatasetDef {
            path: "/Contacts",
            envelope: "Contacts",
            supports_page_size: true,
            line_array: None,
            cols: CONTACT_COLS,
        },
        "bank_transactions" => DatasetDef {
            path: "/BankTransactions",
            envelope: "BankTransactions",
            supports_page_size: true,
            line_array: Some("LineItems"),
            cols: BANK_TXN_COLS,
        },
        "manual_journals" => DatasetDef {
            path: "/ManualJournals",
            envelope: "ManualJournals",
            supports_page_size: false,
            line_array: Some("JournalLines"),
            cols: MANUAL_JOURNAL_COLS,
        },
        _ => return None,
    })
}

fn sheet_label(name: &str) -> String {
    match name {
        "invoices" => "Invoices",
        "contacts" => "Contacts",
        "bank_transactions" => "Bank Transactions",
        "manual_journals" => "Manual Journals",
        other => other,
    }
    .to_string()
}

// ── Fetch ──

/// Fetch records of `def` for the current caller, up to `max_records`. Page 1
/// is fetched alone (the common single-page case stays one request); larger
/// datasets fan out in `buffer_unordered` waves of [`EXPORT_CONCURRENCY`].
///
/// Returns the records plus whether the fetch was cut short by `max_records`
/// with more data likely remaining (a *full* page reaching the cap) — distinct
/// from a clean finish on a short final page, so the caller only warns about a
/// real truncation.
async fn fetch_records(
    client: &XeroClient,
    def: &DatasetDef,
    filter: &Filter,
    max_records: usize,
) -> Result<(Vec<Value>, bool)> {
    // A page shorter than EXPORT_PAGE_SIZE is the last one. This holds for
    // `pageSize`-less endpoints (manual_journals) too: Xero defaults those to
    // 100 rows/page, matching the sentinel.
    let mut records: Vec<Value> = Vec::new();
    let mut first = client
        .get_for_export(def.path, &export_query(def, filter, 1))
        .await?;
    let n = take_records(&mut records, &mut first, def.envelope);
    if records.len() >= max_records {
        records.truncate(max_records);
        return Ok((records, true));
    }
    if n < EXPORT_PAGE_SIZE as usize {
        return Ok((records, false));
    }

    let mut next = 2u64;
    loop {
        let pages: Vec<u64> = (next..next + EXPORT_CONCURRENCY as u64).collect();
        // buffer_unordered polls these page futures on THIS task, so the
        // CURRENT_USER_ID task-local stays in scope and every page resolves the
        // caller's own credentials. Do not tokio::spawn here.
        let mut wave: Vec<(u64, Value)> = stream::iter(pages)
            .map(|p| {
                let q = export_query(def, filter, p);
                async move { client.get_for_export(def.path, &q).await.map(|v| (p, v)) }
            })
            .buffer_unordered(EXPORT_CONCURRENCY)
            .try_collect()
            .await?;
        wave.sort_by_key(|(p, _)| *p);

        for (_, mut v) in wave {
            let n = take_records(&mut records, &mut v, def.envelope);
            if records.len() >= max_records {
                records.truncate(max_records);
                return Ok((records, true));
            }
            if n < EXPORT_PAGE_SIZE as usize {
                return Ok((records, false));
            }
        }
        next += EXPORT_CONCURRENCY as u64;
    }
}

/// The export query for one page of `def`, carrying the caller's `where`/`order`
/// filter. Shared by the materialised fetch and the streaming CSV producer.
fn export_query(def: &DatasetDef, filter: &Filter, page: u64) -> Vec<(&'static str, String)> {
    let mut q = vec![("page", page.to_string())];
    if def.supports_page_size {
        q.push(("pageSize", EXPORT_PAGE_SIZE.to_string()));
    }
    if let Some(w) = &filter.where_ {
        q.push(("where", w.clone()));
    }
    if let Some(o) = &filter.order {
        q.push(("order", o.clone()));
    }
    q
}

/// Move a page's rows out of its response envelope into `records` (no clone),
/// returning the page's row count for pagination termination.
fn take_records(records: &mut Vec<Value>, page: &mut Value, envelope: &str) -> usize {
    match page.get_mut(envelope).and_then(Value::as_array_mut) {
        Some(arr) => {
            let n = arr.len();
            records.append(arr);
            n
        }
        None => 0,
    }
}

// ── Flatten ──

/// At line-item grain a line-less dataset stays at document grain; otherwise the
/// requested grain holds.
fn effective_grain(def: &DatasetDef, grain: Grain) -> Grain {
    if def.line_array.is_some() {
        grain
    } else {
        Grain::Document
    }
}

/// Columns of `def` active at `effective` grain — line-item columns drop out at
/// document grain. Shared by [`build_table`] and the streaming CSV producer.
fn active_cols(def: &DatasetDef, effective: Grain) -> Vec<&ColSpec> {
    def.cols
        .iter()
        .filter(|c| !(effective == Grain::Document && matches!(c.src, Src::Line(_))))
        .collect()
}

fn build_table(def: &DatasetDef, records: &[Value], grain: Grain) -> Table {
    let effective = effective_grain(def, grain);
    let active = active_cols(def, effective);
    let columns = active
        .iter()
        .map(|c| Column::new(c.key, c.header, c.format))
        .collect();
    let rows = records
        .iter()
        .flat_map(|rec| expand_record(&active, def, rec, effective))
        .collect();
    Table { columns, rows }
}

/// Streams a transactional dataset to the CSV export kernel page by page: each
/// page's records are expanded to rows and pushed straight into the sink, so a
/// large pull never materialises in memory. Paging stops at the short final
/// page, at `max_records` (the fetch-side bound — without it a 1:1-expanding
/// dataset would chase the sink's millions-of-rows cap and hammer the API), or
/// when the sink's own ceiling returns [`Flow::Stop`].
struct XeroCsvProducer<'a> {
    client: &'a XeroClient,
    def: &'a DatasetDef,
    filter: &'a Filter,
    grain: Grain,
    max_records: usize,
    capped: AtomicBool,
}

#[async_trait]
impl RowProducer for XeroCsvProducer<'_> {
    async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
        let effective = effective_grain(self.def, self.grain);
        let active = active_cols(self.def, effective);
        let headers: Vec<String> = active.iter().map(|c| c.header.to_string()).collect();
        sink.columns(&headers)?;

        let mut records_seen = 0usize;
        let mut page = 1u64;
        loop {
            let mut resp = self
                .client
                .get_for_export(self.def.path, &export_query(self.def, self.filter, page))
                .await?;
            let rows = resp
                .get_mut(self.def.envelope)
                .and_then(Value::as_array_mut)
                .map(std::mem::take)
                .unwrap_or_default();
            let page_len = rows.len();
            for rec in &rows {
                if records_seen >= self.max_records {
                    self.capped.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                records_seen += 1;
                for row in expand_record(&active, self.def, rec, effective) {
                    let cells: Vec<Value> = row.iter().map(Cell::to_json).collect();
                    if sink.push(cells)? == Flow::Stop {
                        return Ok(());
                    }
                }
            }
            if page_len < EXPORT_PAGE_SIZE as usize {
                return Ok(());
            }
            if records_seen >= self.max_records {
                self.capped.store(true, Ordering::Relaxed);
                return Ok(());
            }
            page += 1;
        }
    }
}

/// Reject the `format:"csv"` requests the streaming path can't honour: it
/// delivers one file (one dataset) of raw rows (no layout reshaping).
fn csv_export_guard(datasets: &[String], layout_present: bool) -> Result<()> {
    if datasets.len() > 1 {
        return Err(eyre!(
            "format:\"csv\" exports a single dataset per call ({}); request one at a time, or use \
             format:\"xlsx\" to put several in one workbook",
            datasets.join(", ")
        ));
    }
    if layout_present {
        return Err(eyre!(
            "format:\"csv\" streams raw rows and does not support `layout` reshaping; omit \
             `layout`, or use format:\"xlsx\"/\"artifact\" for grouped or aggregated output"
        ));
    }
    Ok(())
}

/// Stream one transactional dataset to a CSV artifact and emit the delivery
/// marker. The rows never enter the agent context.
#[allow(clippy::too_many_arguments)]
async fn deliver_csv(
    store: &dyn ArtifactStore,
    agent_id: &str,
    client: &XeroClient,
    def: &DatasetDef,
    filter: &Filter,
    grain: Grain,
    max_records: usize,
    dataset: &str,
    requested_filename: Option<&str>,
    start: Instant,
) -> ToolResult {
    let producer = XeroCsvProducer {
        client,
        def,
        filter,
        grain,
        max_records,
        capped: AtomicBool::new(false),
    };
    let export = match export_csv(
        store,
        agent_id,
        EXPORT_TOOL_NAME,
        StreamCaps::default(),
        &producer,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            return ToolResult::from_report(
                &e.wrap_err(format!("Xero CSV export failed for {dataset}")),
                elapsed_ms(start),
            )
        }
    };
    let capped = export.truncated || producer.capped.load(Ordering::Relaxed);
    let stem = requested_filename
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches(".csv").to_string())
        .unwrap_or_else(|| format!("xero-{dataset}"));
    let filename = download_filename(&stem, "csv");
    let marker = download_marker(&export.artifact_id, "text/csv", &filename, export.bytes);
    let cap_note = if capped {
        " The export hit a row/record ceiling and was truncated — narrow the date range or \
         `where` filter, or raise `max_records`, to export the rest."
    } else {
        ""
    };
    let note = format!(
        "Exported {} row(s) of {dataset} to a CSV file, delivered to this conversation as a \
         downloadable attachment — it happens outside your context, so you will NOT see the file. \
         Tell the user it is ready; do not try to read or re-upload it.{cap_note}",
        export.rows
    );
    ToolResult::success(
        json!({
            GENERATED_IMAGES_KEY: [marker],
            "rows": export.rows,
            "dataset": dataset,
            "truncated": capped,
            "note": note,
        }),
        elapsed_ms(start),
    )
}

/// Expand one Xero record into table rows: one per line item at line-item grain,
/// otherwise a single row (also the fallback when a record carries no lines).
fn expand_record(
    active: &[&ColSpec],
    def: &DatasetDef,
    rec: &Value,
    grain: Grain,
) -> Vec<Vec<Cell>> {
    match (grain, def.line_array) {
        (Grain::LineItem, Some(arr_key)) => match rec[arr_key].as_array() {
            Some(lines) if !lines.is_empty() => lines
                .iter()
                .map(|l| row_for(active, rec, Some(l)))
                .collect(),
            _ => vec![row_for(active, rec, None)],
        },
        _ => vec![row_for(active, rec, None)],
    }
}

fn row_for(active: &[&ColSpec], rec: &Value, line: Option<&Value>) -> Vec<Cell> {
    active
        .iter()
        .map(|c| match &c.src {
            Src::Record(p) => cell_from_json(extract(rec, p), c.format),
            Src::Line(p) => match line {
                Some(l) => cell_from_json(extract(l, p), c.format),
                None => Cell::Empty,
            },
        })
        .collect()
}

/// Walk a dotted path into a JSON value. serde_json indexing yields `Null` for
/// any missing key, so this never panics on schema drift.
fn extract<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for k in path {
        cur = &cur[*k];
    }
    cur
}

/// Like [`Cell::from_json`], but a date-format column gets its Xero date string
/// normalised to `YYYY-MM-DD` first.
fn cell_from_json(v: &Value, format: ColFormat) -> Cell {
    if format == ColFormat::Date {
        if let Value::String(s) = v {
            return Cell::Text(normalize_date(s));
        }
    }
    Cell::from_json(v)
}

/// Normalise Xero date strings to `YYYY-MM-DD`: the legacy `/Date(ms+zone)/`
/// form and ISO timestamps both collapse to a plain date; anything else passes
/// through unchanged.
fn normalize_date(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("/Date(") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(ms) = digits.parse::<i64>() {
            if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
                return dt.format("%Y-%m-%d").to_string();
            }
        }
        return s.to_string();
    }
    match s.split_once('T') {
        Some((d, _)) => d.to_string(),
        None => s.to_string(),
    }
}

// ── Tool ──

fn resolve_client(
    clients: &Clients,
    input: &Value,
    default_entity: &str,
) -> Result<Arc<XeroClient>> {
    let name = input["entity"].as_str().unwrap_or(default_entity);
    clients.get(name).cloned().ok_or_else(|| {
        let mut available: Vec<&str> = clients.keys().map(String::as_str).collect();
        available.sort_unstable();
        eyre!("Unknown Xero entity: '{name}'. Available: {available:?}")
    })
}

pub(super) fn export_description() -> &'static str {
    EXPORT_DESCRIPTION
}

pub(super) fn export_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "entity": { "type": "string", "description": "Xero entity name (single-entity export, or the report fallback when `entities` is omitted)." },
            "entities": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Report datasets only: one worksheet (or artifact) per entity, in order, using the entity names configured under [entities.*]. Defaults to `entity` or the configured default entity."
            },
            "datasets": {
                "type": "array",
                "items": { "type": "string", "enum": ["invoices", "contacts", "bank_transactions", "manual_journals", "profit_and_loss", "balance_sheet", "trial_balance"] },
                "minItems": 1,
                "description": "What to export, one worksheet each. Transactional: invoices, contacts, bank_transactions, manual_journals. Reports: profit_and_loss, balance_sheet, trial_balance. Don't mix the two kinds in one call."
            },
            "where": { "type": "string", "description": "Transactional only: Xero filter expression applied to every dataset (e.g. Date>=DateTime(2026,01,01))." },
            "order": { "type": "string", "description": "Transactional only: Xero order-by expression." },
            "grain": { "type": "string", "enum": ["line_item", "document"], "description": "Transactional only: line_item = one row per line item; document = one row per record. Default line_item." },
            "max_records": { "type": "integer", "minimum": 1, "description": "Transactional only: cap on source records fetched per dataset (default 50000)." },
            "from_date": { "type": "string", "description": "Report (profit_and_loss): period start, YYYY-MM-DD." },
            "to_date": { "type": "string", "description": "Report (profit_and_loss): period end, YYYY-MM-DD." },
            "date": { "type": "string", "description": "Report (balance_sheet / trial_balance): as-at date, YYYY-MM-DD." },
            "periods": { "type": "integer", "description": "Report (profit_and_loss / balance_sheet): number of comparison periods." },
            "timeframe": { "type": "string", "enum": ["MONTH", "QUARTER", "YEAR"], "description": "Report (profit_and_loss / balance_sheet): comparison period size." },
            "report_layout": { "type": "string", "enum": ["statement", "analytical"], "description": "Report xlsx only: statement (reads like the financial statement; default) or analytical (flat, pivot-ready). artifact format always uses analytical." },
            "consolidate": { "type": "boolean", "description": "Report + multi-entity only: also append a Consolidated <report> tab per report, summing matching line items across the listed entities (sum-by-label, aligned on section + line label + period)." },
            "consolidation_template": { "type": "string", "description": "Report + multi-entity only: id of a saved consolidation template that maps each entity's accounts to your canonical statement lines (high-fidelity consolidation). Implies consolidate; without it the Consolidated tab is sum-by-label." },
            "format": { "type": "string", "enum": ["xlsx", "artifact", "csv"], "description": "xlsx (default) delivers a spreadsheet to the channel; artifact stages each sheet as a tabular_artifact for data.query / export_xlsx chaining; csv streams a single transactional dataset (no layout) straight to a CSV file without buffering — best for very large pulls." },
            "filename": { "type": "string", "description": "Output .xlsx filename (auto-generated if omitted; xlsx format only)." },
            "layout": {
                "type": "object",
                "description": "Transactional only: custom shape. Fields: columns:[{field,header?}], group_by:[field], aggregate:[{field,fn:sum|count|avg|min|max}], sort:[{field,dir:asc|desc}], totals_row:bool. `field`/`header` use the dataset's column keys (e.g. Contact, Total, LineAmount).",
                "properties": {
                    "columns": { "type": "array", "items": { "type": "object" } },
                    "group_by": { "type": "array", "items": { "type": "string" } },
                    "aggregate": { "type": "array", "items": { "type": "object" } },
                    "sort": { "type": "array", "items": { "type": "object" } },
                    "totals_row": { "type": "boolean" }
                }
            }
        },
        "required": ["datasets"]
    })
}

pub(super) fn export_tool(clients: Clients, default_entity: String) -> Arc<dyn Tool> {
    Arc::new(XeroExportTool {
        clients,
        default_entity,
        schema: export_schema(),
    })
}

struct XeroExportTool {
    clients: Clients,
    default_entity: String,
    schema: Value,
}

#[async_trait]
impl Tool for XeroExportTool {
    fn name(&self) -> &str {
        EXPORT_TOOL_NAME
    }

    fn description(&self) -> &str {
        EXPORT_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();

        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty());
        let Some(agent_id) = agent_id else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the export",
                elapsed_ms(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the export",
                elapsed_ms(start),
            ));
        };
        let mut datasets: Vec<String> = input["datasets"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut seen_datasets = std::collections::HashSet::new();
        datasets.retain(|d| seen_datasets.insert(d.clone()));
        if datasets.is_empty() {
            return Ok(ToolResult::failure(
                "'datasets' must list at least one of: invoices, contacts, bank_transactions, \
                 manual_journals, profit_and_loss, balance_sheet, trial_balance",
                elapsed_ms(start),
            ));
        }

        match classify_datasets(&datasets) {
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed_ms(start))),
            Ok(DatasetMode::Report(kinds)) => {
                return self
                    .run_report_export(&input, &datasets, kinds, store, agent_id, start)
                    .await
            }
            Ok(DatasetMode::Transactional) => {}
        }

        let client = match resolve_client(&self.clients, &input, &self.default_entity) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed_ms(start))),
        };

        let grain = match input["grain"].as_str() {
            Some("document") => Grain::Document,
            _ => Grain::LineItem,
        };
        let max_records = coerce_to_u64(&input["max_records"])
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_RECORDS)
            .min(HARD_MAX_RECORDS);
        let filter = Filter {
            where_: input["where"].as_str().map(str::to_string),
            order: input["order"].as_str().map(str::to_string),
        };
        let layout = Layout::from_json(&input["layout"]);
        let format = input["format"].as_str().unwrap_or("xlsx");
        if !matches!(format, "xlsx" | "artifact" | "csv") {
            return Ok(ToolResult::failure(
                "`format` must be \"xlsx\", \"csv\", or \"artifact\"",
                elapsed_ms(start),
            ));
        }

        if format == "csv" {
            let layout_present = input["layout"].as_object().is_some_and(|o| !o.is_empty());
            if let Err(e) = csv_export_guard(&datasets, layout_present) {
                return Ok(ToolResult::failure(e.to_string(), elapsed_ms(start)));
            }
            let name = &datasets[0];
            let Some(def) = dataset_def(name) else {
                return Ok(ToolResult::failure(
                    format!("unknown dataset '{name}'; choose from invoices, contacts, bank_transactions, manual_journals"),
                    elapsed_ms(start),
                ));
            };
            return Ok(deliver_csv(
                store.as_ref(),
                &agent_id,
                &client,
                &def,
                &filter,
                grain,
                max_records,
                name,
                input["filename"].as_str(),
                start,
            )
            .await);
        }

        let mut shaped_tables: Vec<(String, Table)> = Vec::new();
        let mut total_rows = 0usize;
        let mut capped = false;
        for name in &datasets {
            let Some(def) = dataset_def(name) else {
                return Ok(ToolResult::failure(
                    format!("unknown dataset '{name}'; choose from invoices, contacts, bank_transactions, manual_journals"),
                    elapsed_ms(start),
                ));
            };
            let (records, fetch_capped) =
                match fetch_records(&client, &def, &filter, max_records).await {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ToolResult::from_report(
                            &e.wrap_err(format!("Xero fetch failed for {name}")),
                            elapsed_ms(start),
                        ))
                    }
                };
            let shaped = apply_layout(&build_table(&def, &records, grain), &layout);
            capped |= fetch_capped;
            total_rows += shaped.rows.len();
            shaped_tables.push((name.clone(), shaped));
        }

        if format == "artifact" {
            return Ok(stage_datasets(
                store.as_ref(),
                &agent_id,
                shaped_tables,
                capped,
                elapsed_ms(start),
            ));
        }

        let mut sheets: Vec<Sheet> = Vec::new();
        for (name, table) in shaped_tables {
            // build_xlsx truncates to Excel's row limit; flag it for the user note.
            capped |= table.rows.len() > EXCEL_MAX_ROWS;
            sheets.push(Sheet::new(sheet_label(&name), table));
        }
        Ok(deliver_xlsx(
            store.as_ref(),
            &agent_id,
            sheets,
            input["filename"].as_str(),
            &format!("xero-{}", datasets.join("-")),
            total_rows,
            capped,
            vec![("datasets", json!(datasets))],
            start,
        ))
    }
}

/// A Xero report dataset and where to fetch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportKind {
    ProfitAndLoss,
    BalanceSheet,
    TrialBalance,
}

impl ReportKind {
    fn from_dataset(name: &str) -> Option<Self> {
        match name {
            "profit_and_loss" => Some(Self::ProfitAndLoss),
            "balance_sheet" => Some(Self::BalanceSheet),
            "trial_balance" => Some(Self::TrialBalance),
            _ => None,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::ProfitAndLoss => "/Reports/ProfitAndLoss",
            Self::BalanceSheet => "/Reports/BalanceSheet",
            Self::TrialBalance => "/Reports/TrialBalance",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ProfitAndLoss => "P&L",
            Self::BalanceSheet => "Balance Sheet",
            Self::TrialBalance => "Trial Balance",
        }
    }

    fn query(self, input: &Value) -> Vec<(&'static str, String)> {
        let pairs: &[(&str, &'static str)] = match self {
            Self::ProfitAndLoss => &[
                ("from_date", "fromDate"),
                ("to_date", "toDate"),
                ("periods", "periods"),
                ("timeframe", "timeframe"),
            ],
            Self::BalanceSheet => &[
                ("date", "date"),
                ("periods", "periods"),
                ("timeframe", "timeframe"),
            ],
            Self::TrialBalance => &[("date", "date")],
        };
        super::tools::map_query(input, pairs)
    }
}

/// All-transactional, all-report, or an error when a call mixes the two — they
/// take different parameters and produce differently-shaped sheets.
enum DatasetMode {
    Transactional,
    Report(Vec<ReportKind>),
}

fn classify_datasets(datasets: &[String]) -> Result<DatasetMode> {
    let kinds: Vec<Option<ReportKind>> = datasets
        .iter()
        .map(|d| ReportKind::from_dataset(d))
        .collect();
    let reports = kinds.iter().filter(|k| k.is_some()).count();
    if reports == 0 {
        Ok(DatasetMode::Transactional)
    } else if reports == datasets.len() {
        Ok(DatasetMode::Report(kinds.into_iter().flatten().collect()))
    } else {
        Err(eyre!(
            "don't mix report datasets (profit_and_loss, balance_sheet, trial_balance) with \
             transactional datasets in one call"
        ))
    }
}

/// Entities to export reports for: an explicit `entities` list, else the single
/// `entity` (or the default).
fn report_entities(input: &Value, default_entity: &str) -> Vec<String> {
    let mut entities: Vec<String> = input["entities"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            vec![input["entity"]
                .as_str()
                .unwrap_or(default_entity)
                .to_string()]
        });
    let mut seen = std::collections::HashSet::new();
    entities.retain(|e| seen.insert(e.clone()));
    entities
}

/// A worksheet/artifact name: the report label alone for a single entity, else
/// prefixed with the entity so multi-entity tabs stay distinct.
fn report_tab_name(entity: &str, kind: ReportKind, multi: bool) -> String {
    if multi {
        format!("{entity} {}", kind.label())
    } else {
        kind.label().to_string()
    }
}

/// Resolve a consolidation template by id for the current user (with a shared
/// fallback), via the hub-installed resolver. `None` when no resolver is wired
/// (CLI/test) or the id is unknown / malformed.
fn load_consolidation_template(id: &str) -> Option<ConsolidationTemplate> {
    let user = CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty());
    let json = resolve_consolidation_template(id, user.as_deref())?;
    serde_json::from_str(&json).ok()
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Build the workbook from `sheets`, stage it, and return the standard
/// out-of-band delivery result. Shared by the transactional and report paths;
/// `extra` carries each path's identifying fields (datasets / entities).
fn deliver_xlsx(
    store: &dyn ArtifactStore,
    agent_id: &str,
    sheets: Vec<Sheet>,
    requested_filename: Option<&str>,
    default_stem: &str,
    total_rows: usize,
    capped: bool,
    extra: Vec<(&str, Value)>,
    start: Instant,
) -> ToolResult {
    let bytes = match build_xlsx(&sheets) {
        Ok(b) => b,
        Err(e) => {
            return ToolResult::failure(
                format!("failed to build the spreadsheet: {e}"),
                elapsed_ms(start),
            )
        }
    };
    let size = bytes.len();
    let filename = xlsx_filename(requested_filename, default_stem);
    let descriptor = ArtifactDescriptor {
        row_count: Some(total_rows),
        ..ArtifactDescriptor::from_context(
            EXPORT_TOOL_NAME,
            ContentType::OctetStream,
            agent_id.to_string(),
        )
    };
    let handle = match store.write(agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
        Ok(h) => h,
        Err(e) => {
            return ToolResult::failure(
                format!("failed to stage the spreadsheet for delivery: {e}"),
                elapsed_ms(start),
            )
        }
    };
    let cap_note = if capped {
        " One or more datasets were truncated to stay within limits — narrow the date \
         range or `where` filter, or raise `max_records`, to export the rest."
    } else {
        ""
    };
    let note = format!(
        "Spreadsheet generated ({total_rows} rows across {} sheet(s)). The system is delivering it \
         to this conversation as a downloadable .xlsx automatically — it happens outside your \
         context, so you will NOT see the file and the rows are NOT visible to you. This is \
         expected, NOT a failure: do not re-fetch with the list tools, do not try to read or \
         verify the data. Just tell the user the file is ready.{cap_note}",
        sheets.len()
    );
    let mut output = json!({
        GENERATED_IMAGES_KEY: [download_marker(&handle.id, XLSX_MIME, &filename, size as u64)],
        "rows": total_rows,
        "sheets": sheets.len(),
        "filename": filename,
        "note": note,
    });
    if let Some(obj) = output.as_object_mut() {
        for (key, value) in extra {
            obj.insert(key.to_string(), value);
        }
    }
    ToolResult::success(output, elapsed_ms(start))
}

impl XeroExportTool {
    /// Report export: fetch each requested report for each entity, flatten and
    /// render it, and emit one worksheet per (entity, report) in a single
    /// workbook — or, for `format:"artifact"`, one queryable tabular artifact per
    /// (entity, report) using the flat analytical shape.
    async fn run_report_export(
        &self,
        input: &Value,
        datasets: &[String],
        kinds: Vec<ReportKind>,
        store: Arc<dyn ArtifactStore>,
        agent_id: String,
        start: Instant,
    ) -> Result<ToolResult> {
        let format = input["format"].as_str().unwrap_or("xlsx");
        if !matches!(format, "xlsx" | "artifact") {
            return Ok(ToolResult::failure(
                "`format` must be \"xlsx\" or \"artifact\"",
                elapsed_ms(start),
            ));
        }
        let layout = match input["report_layout"].as_str() {
            Some("analytical") => ReportLayout::Analytical,
            _ => ReportLayout::Statement,
        };
        let entities = report_entities(input, &self.default_entity);
        let multi = entities.len() > 1;
        let template = match input["consolidation_template"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            Some(id) => match load_consolidation_template(id) {
                Some(t) => Some(t),
                None => {
                    return Ok(ToolResult::failure(
                        format!("consolidation template '{id}' not found"),
                        elapsed_ms(start),
                    ))
                }
            },
            None => None,
        };
        let with_consolidated =
            input["consolidate"].as_bool().unwrap_or(false) || template.is_some();

        let mut entity_clients = Vec::with_capacity(entities.len());
        for entity in &entities {
            match resolve_client(
                &self.clients,
                &json!({ "entity": entity }),
                &self.default_entity,
            ) {
                Ok(client) => entity_clients.push((entity.clone(), client)),
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed_ms(start))),
            }
        }

        // Group by report kind so each consolidated tab sits with its entities.
        let mut named: Vec<(String, ReportModel)> = Vec::new();
        for kind in kinds.iter().copied() {
            let mut models: Vec<(String, ReportModel)> = Vec::with_capacity(entity_clients.len());
            for (entity, client) in &entity_clients {
                let value = match client.get_for_export(kind.path(), &kind.query(input)).await {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolResult::from_report(
                            &e.wrap_err(format!("Xero {} fetch failed for {entity}", kind.label())),
                            elapsed_ms(start),
                        ))
                    }
                };
                let model = parse_report(&value);
                let tab = report_tab_name(entity, kind, multi);
                if with_consolidated && multi {
                    models.push((entity.clone(), model.clone()));
                }
                named.push((tab, model));
            }
            if with_consolidated && models.len() > 1 {
                let consolidated = match &template {
                    Some(t) => consolidate_mapped(&models, t),
                    None => consolidate(&models.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>()),
                };
                named.push((format!("Consolidated {}", kind.label()), consolidated));
            }
        }

        let mut sheets: Vec<Sheet> = Vec::new();
        let mut shaped: Vec<(String, Table)> = Vec::new();
        let mut total_rows = 0usize;
        for (name, model) in named {
            let render_layout = if format == "artifact" {
                ReportLayout::Analytical
            } else {
                layout
            };
            let sheet = render(&model, name, render_layout);
            total_rows += sheet.table.rows.len();
            if format == "artifact" {
                shaped.push((sheet.name, sheet.table));
            } else {
                sheets.push(sheet);
            }
        }

        if format == "artifact" {
            return Ok(stage_datasets(
                store.as_ref(),
                &agent_id,
                shaped,
                false,
                elapsed_ms(start),
            ));
        }
        Ok(deliver_xlsx(
            store.as_ref(),
            &agent_id,
            sheets,
            input["filename"].as_str(),
            &format!("xero-{}", datasets.join("-")),
            total_rows,
            false,
            vec![("entities", json!(entities)), ("reports", json!(datasets))],
            start,
        ))
    }
}

/// `format:"artifact"` terminal: each shaped dataset becomes a canonical
/// staged table (no file delivery — the point is to feed data.query).
fn stage_datasets(
    store: &dyn ArtifactStore,
    agent_id: &str,
    shaped: Vec<(String, Table)>,
    capped: bool,
    duration_ms: u64,
) -> ToolResult {
    let mut datasets_out = serde_json::Map::new();
    let mut total_rows = 0usize;
    for (name, table) in shaped {
        total_rows += table.rows.len();
        let columns = normalize_columns(table.columns.iter().map(|c| c.key.clone()));
        let cells: Vec<Vec<Value>> = table
            .rows
            .iter()
            .map(|row| row.iter().map(Cell::to_json).collect())
            .collect();
        let objects = zip_table(&columns, cells);
        match stage_table(
            store,
            agent_id,
            EXPORT_TOOL_NAME,
            Some(columns),
            objects,
            DEFAULT_INLINE_THRESHOLD,
        ) {
            Ok(staged) => {
                datasets_out.insert(name, staged_to_output(staged, EXPORT_TOOL_NAME));
            }
            Err(e) => {
                return ToolResult::failure(format!("failed to stage {name}: {e}"), duration_ms)
            }
        }
    }
    let cap_note = if capped {
        " Source records were truncated by max_records — narrow the `where` filter to capture everything."
    } else {
        ""
    };
    ToolResult::success(
        json!({
            "datasets": datasets_out,
            "rows": total_rows,
            "note": format!(
                "Datasets staged as tabular artifacts (no file delivered). Chain them: \
                 transform/consolidate with data.query using the artifact ids, then \
                 export_xlsx the result. Do not re-fetch with the list tools.{cap_note}"
            ),
        }),
        duration_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::settings::SecretStore;

    #[test]
    fn classify_datasets_separates_reports_from_transactional() {
        assert!(matches!(
            classify_datasets(&["invoices".into(), "contacts".into()]).unwrap(),
            DatasetMode::Transactional
        ));
        match classify_datasets(&["profit_and_loss".into(), "balance_sheet".into()]).unwrap() {
            DatasetMode::Report(kinds) => assert_eq!(kinds.len(), 2),
            DatasetMode::Transactional => panic!("expected report mode"),
        }
        assert!(classify_datasets(&["invoices".into(), "profit_and_loss".into()]).is_err());
    }

    #[test]
    fn report_kind_paths_and_query_mapping() {
        assert_eq!(
            ReportKind::from_dataset("balance_sheet"),
            Some(ReportKind::BalanceSheet)
        );
        assert_eq!(ReportKind::from_dataset("nope"), None);
        assert_eq!(ReportKind::ProfitAndLoss.path(), "/Reports/ProfitAndLoss");

        let pnl = ReportKind::ProfitAndLoss.query(&json!({
            "from_date": "2026-04-01", "to_date": "2026-05-31", "periods": 2
        }));
        assert!(pnl.contains(&("fromDate", "2026-04-01".to_string())));
        assert!(pnl.contains(&("toDate", "2026-05-31".to_string())));
        assert!(pnl.contains(&("periods", "2".to_string())));

        let bs = ReportKind::BalanceSheet.query(&json!({ "date": "2026-05-31", "from_date": "x" }));
        assert!(bs.contains(&("date", "2026-05-31".to_string())));
        assert!(!bs.iter().any(|(k, _)| *k == "fromDate"));
    }

    #[test]
    fn report_tab_name_single_vs_multi_entity() {
        assert_eq!(
            report_tab_name("harbor", ReportKind::ProfitAndLoss, false),
            "P&L"
        );
        assert_eq!(
            report_tab_name("beacon", ReportKind::BalanceSheet, true),
            "beacon Balance Sheet"
        );
    }

    #[test]
    fn report_entities_explicit_and_default() {
        assert_eq!(
            report_entities(&json!({}), "main"),
            vec!["main".to_string()]
        );
        assert_eq!(
            report_entities(&json!({ "entity": "beacon" }), "main"),
            vec!["beacon".to_string()]
        );
        assert_eq!(
            report_entities(&json!({ "entities": ["a", "b"] }), "main"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[tokio::test]
    async fn report_export_builds_a_delivered_workbook() {
        use workforce_agent::artifacts::FsArtifactStore;

        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Reports/ProfitAndLoss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Reports": [{
                    "ReportType": "ProfitAndLoss",
                    "ReportTitles": ["Profit and Loss", "Demo Co", "May 2026"],
                    "Rows": [
                        { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"May 2026"} ] },
                        { "RowType": "Section", "Title": "Income", "Rows": [
                            { "RowType": "Row", "Cells": [
                                {"Value":"Sales","Attributes":[{"Value":"a","Id":"account"}]},
                                {"Value":"100.00"} ] },
                            { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Income"}, {"Value":"100.00"} ] }
                        ] }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let client = Arc::new(XeroClient::with_store_and_endpoints_for_test(
            Arc::new(UserStore) as Arc<dyn SecretStore>,
            "default",
            None,
            &server.uri(),
        ));
        let mut clients = HashMap::new();
        clients.insert("default".to_string(), client);
        let tool = XeroExportTool {
            clients: Arc::new(clients),
            default_entity: "default".to_string(),
            schema: export_schema(),
        };

        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({
            "datasets": ["profit_and_loss"],
            "from_date": "2026-05-01",
            "to_date": "2026-05-31"
        });

        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                tool.run_report_export(
                    &input,
                    &["profit_and_loss".to_string()],
                    vec![ReportKind::ProfitAndLoss],
                    store,
                    "agent-1".to_string(),
                    Instant::now(),
                ),
            )
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["sheets"], 1);
        assert_eq!(result.output["entities"], json!(["default"]));
        let images = result.output[GENERATED_IMAGES_KEY].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert!(images[0]["filename"].as_str().unwrap().ends_with(".xlsx"));
    }

    #[test]
    fn report_entities_dedupes_preserving_order() {
        assert_eq!(
            report_entities(&json!({ "entities": ["a", "b", "a"] }), "main"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[tokio::test]
    async fn report_export_two_entities_two_tabs() {
        use workforce_agent::artifacts::FsArtifactStore;

        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Reports/ProfitAndLoss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Reports": [{
                    "ReportType": "ProfitAndLoss",
                    "Rows": [
                        { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"May 2026"} ] },
                        { "RowType": "Section", "Title": "Income", "Rows": [
                            { "RowType": "Row", "Cells": [
                                {"Value":"Sales","Attributes":[{"Value":"a","Id":"account"}]},
                                {"Value":"50.00"} ] }
                        ] }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let make = || {
            Arc::new(XeroClient::with_store_and_endpoints_for_test(
                Arc::new(UserStore) as Arc<dyn SecretStore>,
                "e",
                None,
                &server.uri(),
            ))
        };
        let mut clients = HashMap::new();
        clients.insert("harbor".to_string(), make());
        clients.insert("beacon".to_string(), make());
        let tool = XeroExportTool {
            clients: Arc::new(clients),
            default_entity: "harbor".to_string(),
            schema: export_schema(),
        };

        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({
            "datasets": ["profit_and_loss"],
            "entities": ["harbor", "beacon"],
            "from_date": "2026-05-01",
            "to_date": "2026-05-31"
        });

        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                tool.run_report_export(
                    &input,
                    &["profit_and_loss".to_string()],
                    vec![ReportKind::ProfitAndLoss],
                    store,
                    "agent-2".to_string(),
                    Instant::now(),
                ),
            )
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["sheets"], 2);
        assert_eq!(result.output["entities"], json!(["harbor", "beacon"]));
    }

    #[tokio::test]
    async fn report_export_consolidate_adds_a_summed_tab() {
        use workforce_agent::artifacts::FsArtifactStore;

        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Reports/ProfitAndLoss"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Reports": [{
                    "ReportType": "ProfitAndLoss",
                    "Rows": [
                        { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"May 2026"} ] },
                        { "RowType": "Section", "Title": "Income", "Rows": [
                            { "RowType": "Row", "Cells": [
                                {"Value":"Sales","Attributes":[{"Value":"a","Id":"account"}]},
                                {"Value":"50.00"} ] }
                        ] }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let make = || {
            Arc::new(XeroClient::with_store_and_endpoints_for_test(
                Arc::new(UserStore) as Arc<dyn SecretStore>,
                "e",
                None,
                &server.uri(),
            ))
        };
        let mut clients = HashMap::new();
        clients.insert("harbor".to_string(), make());
        clients.insert("beacon".to_string(), make());
        let tool = XeroExportTool {
            clients: Arc::new(clients),
            default_entity: "harbor".to_string(),
            schema: export_schema(),
        };

        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({
            "datasets": ["profit_and_loss"],
            "entities": ["harbor", "beacon"],
            "consolidate": true,
            "from_date": "2026-05-01",
            "to_date": "2026-05-31"
        });

        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                tool.run_report_export(
                    &input,
                    &["profit_and_loss".to_string()],
                    vec![ReportKind::ProfitAndLoss],
                    store,
                    "agent-3".to_string(),
                    Instant::now(),
                ),
            )
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["sheets"], 3);
    }

    #[tokio::test]
    async fn report_export_unknown_consolidation_template_errors() {
        use workforce_agent::artifacts::FsArtifactStore;

        let tool = XeroExportTool {
            clients: Arc::new(HashMap::new()),
            default_entity: "x".to_string(),
            schema: export_schema(),
        };
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({ "datasets": ["profit_and_loss"], "consolidation_template": "nope" });

        let result = tool
            .run_report_export(
                &input,
                &["profit_and_loss".to_string()],
                vec![ReportKind::ProfitAndLoss],
                store,
                "a".to_string(),
                Instant::now(),
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not found"));
    }

    #[test]
    fn stage_datasets_emits_canonical_objects_per_dataset() {
        use workforce_agent::artifacts::{ArtifactBody, ArtifactId, FsArtifactStore};

        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let def = dataset_def("invoices").unwrap();
        let records: Vec<Value> = (0..300)
            .map(|i| {
                json!({
                    "InvoiceNumber": format!("INV-{i}"),
                    "Total": 30.0,
                    "LineItems": [ { "Description": "a", "LineAmount": 10.0 } ]
                })
            })
            .collect();
        let table = build_table(&def, &records, Grain::LineItem);
        let result = stage_datasets(
            store.as_ref(),
            "agent-a",
            vec![("invoices".to_string(), table)],
            false,
            0,
        );
        assert!(result.success, "{:?}", result.error);
        assert!(
            result.output.get(GENERATED_IMAGES_KEY).is_none(),
            "artifact mode must not trigger file delivery"
        );
        let entry = &result.output["datasets"]["invoices"];
        assert_eq!(entry["kind"], "tabular_artifact");
        assert!(entry["preview"].is_string(), "preview must be wrapped");
        let id = ArtifactId::parse(entry["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(Value::Array(stored)) = store.read("agent-a", &id).unwrap() else {
            panic!("expected canonical rows");
        };
        assert_eq!(stored.len(), 300);
        assert_eq!(stored[0]["InvoiceNumber"], "INV-0");
        assert_eq!(stored[0]["LineAmount"], 10.0);
    }

    #[test]
    fn stage_datasets_inlines_small_datasets() {
        use workforce_agent::artifacts::FsArtifactStore;

        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let def = dataset_def("contacts").unwrap();
        let table = build_table(&def, &[json!({"Name": "Acme"})], Grain::Document);
        let result = stage_datasets(
            store.as_ref(),
            "agent-a",
            vec![("contacts".to_string(), table)],
            false,
            0,
        );
        assert!(result.success);
        let entry = &result.output["datasets"]["contacts"];
        assert_eq!(entry["kind"], "rows");
        assert_eq!(entry["row_count"], 1);
        assert!(entry["rows_wrapped"].as_str().unwrap().contains("Acme"));
    }

    #[test]
    fn invoices_line_item_grain_expands_lines() {
        let def = dataset_def("invoices").unwrap();
        let rec = json!({
            "InvoiceNumber": "INV-1",
            "Total": 30.0,
            "LineItems": [
                { "Description": "a", "LineAmount": 10.0 },
                { "Description": "b", "LineAmount": 20.0 }
            ]
        });
        let table = build_table(&def, std::slice::from_ref(&rec), Grain::LineItem);
        assert_eq!(table.rows.len(), 2, "one row per line item");
        // Invoice number repeats on each line row.
        let inv_col = table
            .columns
            .iter()
            .position(|c| c.key == "InvoiceNumber")
            .unwrap();
        assert_eq!(table.rows[0][inv_col], Cell::Text("INV-1".into()));
        assert_eq!(table.rows[1][inv_col], Cell::Text("INV-1".into()));
    }

    #[test]
    fn invoices_document_grain_drops_line_columns() {
        let def = dataset_def("invoices").unwrap();
        let rec = json!({ "InvoiceNumber": "INV-1", "LineItems": [ {"Description":"a"}, {"Description":"b"} ] });
        let table = build_table(&def, std::slice::from_ref(&rec), Grain::Document);
        assert_eq!(table.rows.len(), 1, "one row per invoice");
        assert!(
            !table.columns.iter().any(|c| c.key.starts_with("Line")),
            "line columns absent at document grain"
        );
    }

    #[test]
    fn record_with_no_line_items_still_yields_a_row() {
        let def = dataset_def("invoices").unwrap();
        let rec = json!({ "InvoiceNumber": "INV-1" });
        let table = build_table(&def, std::slice::from_ref(&rec), Grain::LineItem);
        assert_eq!(table.rows.len(), 1);
    }

    #[test]
    fn normalize_date_handles_ms_and_iso_and_passthrough() {
        assert_eq!(normalize_date("/Date(1672531200000+0000)/"), "2023-01-01");
        assert_eq!(normalize_date("2026-03-04T00:00:00"), "2026-03-04");
        assert_eq!(normalize_date("not a date"), "not a date");
    }

    #[test]
    fn amounts_are_numeric_cells() {
        let def = dataset_def("invoices").unwrap();
        let rec = json!({ "Total": 12.5, "LineItems": [ { "LineAmount": 12.5 } ] });
        let table = build_table(&def, std::slice::from_ref(&rec), Grain::LineItem);
        let total_col = table.columns.iter().position(|c| c.key == "Total").unwrap();
        assert_eq!(table.rows[0][total_col], Cell::Number(12.5));
    }

    // ── Per-user isolation under concurrency (the spawn-leak regression) ──
    //
    // Two users hit ONE shared XeroClient concurrently. A store resolves each
    // caller's credentials from the CURRENT_USER_ID task-local; the mock server
    // routes creds → token → tenant → data so each user has a distinct dataset.
    // The export's concurrency runs via buffer_unordered INSIDE the call, so the
    // task-local survives the multi-page wave. A spawn-based fetch would drop it
    // and resolve the wrong (or no) credentials — failing or cross-contaminating
    // this test.

    /// Resolves per-user Xero credentials from the `CURRENT_USER_ID` task-local,
    /// exactly as a `LayeredUserVault` would in production.
    struct UserStore;

    impl SecretStore for UserStore {
        fn get(&self, path: &str) -> Option<String> {
            let uid = workforce_agent::runner::CURRENT_USER_ID
                .try_with(|u| u.clone())
                .ok()
                .filter(|u| !u.is_empty())?;
            if path.ends_with("/client_id") {
                Some(format!("id-{uid}"))
            } else if path.ends_with("/client_secret") {
                Some(format!("secret-{uid}"))
            } else {
                None
            }
        }
        fn set(&self, _: &str, _: &str) -> eyre::Result<()> {
            Ok(())
        }
        fn delete(&self, _: &str) -> eyre::Result<bool> {
            Ok(false)
        }
        fn list(&self, _: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    fn invoices_body(uid: &str, count: u32) -> Value {
        let rows: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "InvoiceID": format!("{uid}-{i}"),
                    "InvoiceNumber": uid,
                    "Type": "ACCREC",
                    "Total": 10.0,
                    "LineItems": [ { "Description": "x", "LineAmount": 10.0 } ]
                })
            })
            .collect();
        json!({ "Invoices": rows })
    }

    async fn mount_user(server: &MockServer, uid: &str) {
        use base64::Engine;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("id-{uid}:secret-{uid}"))
        );
        let token = format!("tok-{uid}");
        let tenant = format!("tenant-{uid}");

        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .and(header("authorization", basic.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": token, "token_type": "Bearer", "expires_in": 1800
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/connections"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "tenantId": tenant, "tenantType": "ORGANISATION" }
            ])))
            .mount(server)
            .await;

        // Page-specific mocks mounted before the catch-all: wiremock matches in
        // mount order, so page=1/2/3 win and higher pages fall to empty.
        for (pg, count) in [("1", 100u32), ("2", 100), ("3", 5)] {
            Mock::given(method("GET"))
                .and(path("/api.xro/2.0/Invoices"))
                .and(header("xero-tenant-id", tenant.as_str()))
                .and(query_param("page", pg))
                .respond_with(ResponseTemplate::new(200).set_body_json(invoices_body(uid, count)))
                .mount(server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .and(header("xero-tenant-id", tenant.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Invoices": [] })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn concurrent_users_get_only_their_own_data() {
        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        mount_user(&server, "bob").await;

        // ONE shared client across both users — the production topology and the
        // original leak vector.
        let client = Arc::new(XeroClient::with_store_and_endpoints_for_test(
            Arc::new(UserStore) as Arc<dyn SecretStore>,
            "default",
            None,
            &server.uri(),
        ));
        let def = dataset_def("invoices").unwrap();
        let filter = Filter {
            where_: None,
            order: None,
        };

        let alice = workforce_agent::runner::CURRENT_USER_ID.scope(
            "alice".to_string(),
            fetch_records(&client, &def, &filter, 100_000),
        );
        let bob = workforce_agent::runner::CURRENT_USER_ID.scope(
            "bob".to_string(),
            fetch_records(&client, &def, &filter, 100_000),
        );
        let (a, b) = tokio::join!(alice, bob);

        let (a, _) = a.expect("alice fetch ok");
        let (b, _) = b.expect("bob fetch ok");
        assert_eq!(a.len(), 205, "alice gets her full multi-page dataset");
        assert_eq!(b.len(), 205, "bob gets his full multi-page dataset");
        assert!(
            a.iter().all(|r| r["InvoiceNumber"] == "alice"),
            "alice must see ONLY her own tenant's rows"
        );
        assert!(
            b.iter().all(|r| r["InvoiceNumber"] == "bob"),
            "bob must see ONLY his own tenant's rows"
        );
    }

    #[test]
    fn csv_export_guard_rejects_multi_dataset_and_layout() {
        assert!(csv_export_guard(&["invoices".into()], false).is_ok());
        let multi = csv_export_guard(&["invoices".into(), "contacts".into()], false)
            .unwrap_err()
            .to_string();
        assert!(multi.contains("single dataset"));
        let layout = csv_export_guard(&["invoices".into()], true)
            .unwrap_err()
            .to_string();
        assert!(layout.contains("layout"));
    }

    async fn csv_export(
        store: &dyn ArtifactStore,
        server: &MockServer,
        max_records: usize,
    ) -> workforce_agent::artifacts::CsvExport {
        let client = XeroClient::with_store_and_endpoints_for_test(
            Arc::new(UserStore) as Arc<dyn SecretStore>,
            "default",
            None,
            &server.uri(),
        );
        let def = dataset_def("invoices").unwrap();
        let filter = Filter {
            where_: None,
            order: None,
        };
        let producer = XeroCsvProducer {
            client: &client,
            def: &def,
            filter: &filter,
            grain: Grain::LineItem,
            max_records,
            capped: AtomicBool::new(false),
        };
        let export = CURRENT_USER_ID
            .scope(
                "alice".to_string(),
                export_csv(
                    store,
                    "agent-a",
                    "xero.export",
                    StreamCaps::default(),
                    &producer,
                ),
            )
            .await
            .expect("csv export ok");
        // The records-cap flag is the producer's; fold it in for the assertions
        // the caller makes on `truncated`.
        workforce_agent::artifacts::CsvExport {
            truncated: export.truncated || producer.capped.load(Ordering::Relaxed),
            ..export
        }
    }

    #[tokio::test]
    async fn csv_export_streams_all_pages_into_one_artifact() {
        use workforce_agent::artifacts::FsArtifactStore;
        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();

        let export = csv_export(store.as_ref(), &server, 100_000).await;
        assert_eq!(export.rows, 205, "every record's line item streamed");
        assert!(!export.truncated);
        let ArtifactBody::Bytes(bytes) = store.read("agent-a", &export.artifact_id).unwrap() else {
            panic!("expected csv bytes");
        };
        let csv = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 206, "header + 205 data rows");
        assert!(
            lines[0].contains("Invoice"),
            "header row present: {}",
            lines[0]
        );
    }

    #[tokio::test]
    async fn csv_export_stops_at_max_records() {
        use workforce_agent::artifacts::FsArtifactStore;
        let server = MockServer::start().await;
        mount_user(&server, "alice").await;
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();

        let export = csv_export(store.as_ref(), &server, 150).await;
        assert_eq!(export.rows, 150, "capped at the fetch-side record bound");
        assert!(export.truncated, "the record cap is reported as truncation");
    }
}
