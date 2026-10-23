//! Pure helpers for the `aws.s3.*` tools: content-type classification and
//! CSV / spreadsheet → row parsing. Byte-cap enforcement lives in
//! [`super::client`] where it can stream the SDK `ByteStream` and abort
//! before the body is materialised.
//!
//! Kept free of SDK types so the routing + decoding logic is unit-testable
//! without an S3 client.

use bytes::Bytes;
use calamine::{Sheet, SheetType, SheetVisible};
use csv::ReaderBuilder;
use eyre::{eyre, Result};
use serde_json::{Map, Value};
use workforce_agent::artifacts::normalize_columns;
use workforce_agent::spreadsheet::{display_cell, Workbook};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Csv,
    Json,
    Text,
    Spreadsheet,
    Binary,
}

const TEXT_PREFIXES: &[&str] = &[
    "text/",
    "application/x-ndjson",
    "application/xml",
    "text/xml",
];

/// OOXML workbooks only — the formats [`parse_spreadsheet`]'s streaming
/// reader can bound in memory. Legacy `xls`/`xlsb`/`ods` stay `Binary`
/// (refused, as before this feature) because calamine's only API for them
/// materialises a dense range sized by cell *coordinates* before any cap
/// could run.
const SPREADSHEET_TYPES: &[&str] = &[
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-excel.sheet.macroenabled.12",
];

/// Classify a payload from its content-type header (lower-cased,
/// parameter-stripped) and key suffix. Suffix wins when the header is
/// missing or generic (`application/octet-stream`) because uploaders
/// frequently mislabel CSVs and JSON files.
pub fn classify(content_type: Option<&str>, key: &str) -> ContentKind {
    let suffix = key.rsplit('.').next().map(|s| s.to_ascii_lowercase());
    let header = content_type.map(|h| {
        h.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    });

    if matches!(suffix.as_deref(), Some("csv"))
        || matches!(
            header.as_deref(),
            Some("text/csv") | Some("application/csv")
        )
    {
        return ContentKind::Csv;
    }
    if matches!(
        suffix.as_deref(),
        Some("json") | Some("jsonl") | Some("ndjson")
    ) || matches!(
        header.as_deref(),
        Some("application/json") | Some("text/json") | Some("application/x-ndjson")
    ) {
        return ContentKind::Json;
    }
    if let Some(h) = header.as_deref() {
        if SPREADSHEET_TYPES.contains(&h) {
            return ContentKind::Spreadsheet;
        }
        if TEXT_PREFIXES.iter().any(|p| h.starts_with(p)) {
            return ContentKind::Text;
        }
        if h == "application/octet-stream" || h.is_empty() {
            return kind_from_suffix(suffix.as_deref());
        }
        return ContentKind::Binary;
    }
    kind_from_suffix(suffix.as_deref())
}

fn kind_from_suffix(suffix: Option<&str>) -> ContentKind {
    match suffix {
        Some("txt" | "log" | "md" | "yaml" | "yml" | "toml") => ContentKind::Text,
        Some("xlsx" | "xlsm") => ContentKind::Spreadsheet,
        _ => ContentKind::Binary,
    }
}

/// Parse CSV bytes into a `(columns, rows)` pair. The first record is the
/// header. Empty input returns empty columns and zero rows. Non-UTF-8
/// bytes are rejected with a clear error rather than lossily coerced.
pub fn parse_csv(bytes: &Bytes) -> Result<(Vec<String>, Vec<Value>)> {
    if bytes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if std::str::from_utf8(bytes).is_err() {
        return Err(eyre!("csv body is not valid UTF-8"));
    }
    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(bytes.as_ref());
    let columns: Vec<String> = reader
        .headers()
        .map_err(|e| eyre!("csv header read failed: {e}"))?
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = reader
        .records()
        .map(|res| {
            let record = res.map_err(|e| eyre!("csv row read failed: {e}"))?;
            let row: Map<String, Value> = columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let cell = record.get(i).unwrap_or("").to_string();
                    (col.clone(), Value::String(cell))
                })
                .collect();
            Ok::<Value, eyre::Error>(Value::Object(row))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((columns, rows))
}

/// One parsed worksheet, in the same `(columns, rows)` shape as [`parse_csv`]
/// plus the workbook's sheet inventory so tools can report and retarget.
#[derive(Debug)]
pub struct SheetParse {
    pub sheet: String,
    pub sheets: Vec<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
}

/// Parse one sheet of an OOXML workbook (xlsx/xlsm) into the [`parse_csv`]
/// row shape: the bounding box's first row is the header
/// (normalised — xlsx headers routinely contain blanks and duplicates, which
/// would silently drop cells as object keys), and every cell renders to a
/// `Value::String`. All-strings is deliberate parity with CSV, not an
/// oversight: `collect`'s filter matches cells via `Value::as_str`, and typed
/// numbers would make it silently blind.
///
/// Cells stream through the shared bounded reader
/// (`workforce_agent::spreadsheet`) — never calamine's dense
/// `worksheet_range`, whose allocation is sized by cell coordinates.
pub fn parse_spreadsheet(bytes: &Bytes, sheet: Option<&str>) -> Result<SheetParse> {
    let mut workbook = Workbook::open(bytes.as_ref())?;
    let sheets = workbook.sheet_names();
    let name = resolve_sheet(sheet, &sheets, workbook.sheets_metadata())?;
    let grid = workbook.read_grid(&name, display_cell)?;
    let mut grid_rows = grid.rows.into_iter();
    let Some(first) = grid_rows.next() else {
        return Ok(SheetParse {
            sheet: name,
            sheets,
            columns: Vec::new(),
            rows: Vec::new(),
        });
    };
    let columns = normalize_columns(first);
    let rows = grid_rows
        .map(|row| {
            let object: Map<String, Value> = columns
                .iter()
                .cloned()
                .zip(row.into_iter().map(Value::String))
                .collect();
            Value::Object(object)
        })
        .collect();
    Ok(SheetParse {
        sheet: name,
        sheets,
        columns,
        rows,
    })
}

/// A named sheet may be any sheet in the workbook; the default is the first
/// *visible* worksheet — enterprise exports routinely hide a metadata sheet
/// first — falling back to the first sheet of any kind so an all-hidden
/// workbook still parses.
fn resolve_sheet(wanted: Option<&str>, sheets: &[String], meta: &[Sheet]) -> Result<String> {
    match wanted {
        Some(w) => sheets
            .iter()
            .find(|s| s.as_str() == w)
            .cloned()
            .ok_or_else(|| eyre!("sheet {w:?} not found; workbook sheets are {sheets:?}")),
        None => meta
            .iter()
            .find(|s| {
                matches!(s.typ, SheetType::WorkSheet) && matches!(s.visible, SheetVisible::Visible)
            })
            .map(|s| s.name.clone())
            .or_else(|| sheets.first().cloned())
            .ok_or_else(|| eyre!("workbook contains no sheets")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_csv_by_header_or_suffix() {
        assert_eq!(classify(Some("text/csv"), "x"), ContentKind::Csv);
        assert_eq!(classify(Some("application/csv"), "x"), ContentKind::Csv);
        assert_eq!(
            classify(Some("text/csv; charset=utf-8"), "x"),
            ContentKind::Csv
        );
        assert_eq!(classify(None, "trust/file.csv"), ContentKind::Csv);
        assert_eq!(classify(None, "FILE.CSV"), ContentKind::Csv);
    }

    #[test]
    fn classify_json_by_header_or_suffix() {
        assert_eq!(classify(Some("application/json"), "x"), ContentKind::Json);
        assert_eq!(
            classify(Some("application/x-ndjson"), "x"),
            ContentKind::Json
        );
        assert_eq!(classify(None, "x.json"), ContentKind::Json);
        assert_eq!(classify(None, "events.jsonl"), ContentKind::Json);
    }

    #[test]
    fn classify_text_from_prefix() {
        assert_eq!(classify(Some("text/plain"), "x"), ContentKind::Text);
        assert_eq!(classify(Some("text/markdown"), "x"), ContentKind::Text);
        assert_eq!(classify(Some("application/xml"), "x"), ContentKind::Text);
        assert_eq!(classify(None, "notes.md"), ContentKind::Text);
        assert_eq!(classify(None, "config.toml"), ContentKind::Text);
    }

    #[test]
    fn classify_binary_default_when_unknown() {
        assert_eq!(classify(Some("image/png"), "x.png"), ContentKind::Binary);
        assert_eq!(classify(Some("application/pdf"), "x"), ContentKind::Binary);
        assert_eq!(classify(None, "blob"), ContentKind::Binary);
        assert_eq!(
            classify(Some("application/octet-stream"), "blob"),
            ContentKind::Binary
        );
    }

    #[test]
    fn classify_octet_stream_falls_back_to_suffix() {
        // Uploaders that send `application/octet-stream` for everything
        // shouldn't trap legitimate CSVs.
        assert_eq!(
            classify(Some("application/octet-stream"), "x.csv"),
            ContentKind::Csv
        );
        assert_eq!(
            classify(Some("application/octet-stream"), "x.json"),
            ContentKind::Json
        );
    }

    #[test]
    fn parse_csv_returns_columns_and_string_rows() {
        let body = Bytes::from_static(b"a,b\n1,2\n3,4\n");
        let (cols, rows) = parse_csv(&body).unwrap();
        assert_eq!(cols, vec!["a", "b"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["a"], "1");
        assert_eq!(rows[0]["b"], "2");
        assert_eq!(rows[1]["a"], "3");
    }

    #[test]
    fn parse_csv_handles_quoted_commas_and_ragged_rows() {
        let body = Bytes::from_static(b"name,note\nalice,\"hello, world\"\nbob,\n");
        let (_, rows) = parse_csv(&body).unwrap();
        assert_eq!(rows[0]["note"], "hello, world");
        assert_eq!(rows[1]["note"], "");
    }

    #[test]
    fn parse_csv_empty_input_returns_empty() {
        let (cols, rows) = parse_csv(&Bytes::new()).unwrap();
        assert!(cols.is_empty());
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_csv_rejects_non_utf8() {
        let body = Bytes::from_static(&[0xff, 0xfe, 0xfd]);
        assert!(parse_csv(&body).is_err());
    }

    use crate::aws::test_workbook as workbook;

    #[test]
    fn classify_spreadsheet_by_content_type_or_fallback_suffix() {
        // The archive case: S3 serves xlsx as octet-stream; the suffix decides.
        assert_eq!(
            classify(Some("application/octet-stream"), "raw/report.xlsx"),
            ContentKind::Spreadsheet
        );
        for key in ["a.xlsx", "a.xlsm", "A.XLSX"] {
            assert_eq!(classify(None, key), ContentKind::Spreadsheet, "{key}");
        }
        assert_eq!(
            classify(
                Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
                "blob"
            ),
            ContentKind::Spreadsheet
        );
        // A specific header outranks the suffix: the classic fake-xls (an HTML
        // table named .xls) keeps inlining as text instead of hard-failing in
        // a workbook parser.
        assert_eq!(classify(Some("text/html"), "report.xls"), ContentKind::Text);
        assert_eq!(
            classify(Some("text/html"), "report.xlsx"),
            ContentKind::Text
        );
        // Legacy/binary spreadsheet formats have no memory-bounded parse path
        // and stay refused.
        for key in ["a.xls", "a.xlsb", "a.ods"] {
            assert_eq!(classify(None, key), ContentKind::Binary, "{key}");
        }
        assert_eq!(classify(Some("image/png"), "x.png"), ContentKind::Binary);
    }

    #[test]
    fn parse_spreadsheet_returns_csv_shaped_string_rows() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 0, "Request Id").unwrap();
            ws.write_string(0, 1, "Reference").unwrap();
            ws.write_string(0, 2, "Filled").unwrap();
            ws.write_number(1, 0, 826281843.0).unwrap();
            ws.write_string(1, 1, "0826281843").unwrap();
            ws.write_boolean(1, 2, true).unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(parsed.sheets, vec!["Sheet1"]);
        assert_eq!(parsed.sheet, "Sheet1");
        assert_eq!(parsed.columns, vec!["Request Id", "Reference", "Filled"]);
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(
            parsed.rows[0]["Request Id"], "826281843",
            "numeric cell renders as a bare integer string, not a float"
        );
        assert_eq!(
            parsed.rows[0]["Reference"], "0826281843",
            "text cell keeps its leading zero"
        );
        assert_eq!(parsed.rows[0]["Filled"], "true");
    }

    #[test]
    fn parse_spreadsheet_renders_dates_the_way_humans_type_them() {
        use rust_xlsxwriter::{ExcelDateTime, Format};
        let date_fmt = Format::new().set_num_format("yyyy-mm-dd");
        let dt_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 0, "Date").unwrap();
            ws.write_string(0, 1, "Exported At").unwrap();
            ws.write_datetime_with_format(
                1,
                0,
                ExcelDateTime::from_ymd(2026, 8, 20).unwrap(),
                &date_fmt,
            )
            .unwrap();
            ws.write_datetime_with_format(
                1,
                1,
                ExcelDateTime::from_ymd(2026, 8, 20)
                    .unwrap()
                    .and_hms(8, 10, 42)
                    .unwrap(),
                &dt_fmt,
            )
            .unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(parsed.rows[0]["Date"], "2026-08-20");
        assert_eq!(parsed.rows[0]["Exported At"], "2026-08-20 08:10:42");
    }

    #[test]
    fn parse_spreadsheet_normalizes_blank_and_duplicate_headers() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 1, "Id").unwrap();
            ws.write_string(0, 2, "Id").unwrap();
            ws.write_string(1, 0, "a").unwrap();
            ws.write_string(1, 1, "b").unwrap();
            ws.write_string(1, 2, "c").unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(parsed.columns, vec!["col_1", "Id", "Id_2"]);
        assert_eq!(parsed.rows[0]["col_1"], "a");
        assert_eq!(parsed.rows[0]["Id"], "b");
        assert_eq!(parsed.rows[0]["Id_2"], "c");
    }

    #[test]
    fn parse_spreadsheet_pads_short_rows_with_empty_strings() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 0, "a").unwrap();
            ws.write_string(0, 1, "b").unwrap();
            ws.write_string(1, 0, "only").unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(
            parsed.rows[0]["b"], "",
            "an unwritten trailing cell is an empty string, like CSV"
        );
    }

    #[test]
    fn parse_spreadsheet_selects_sheet_by_name_and_defaults_to_first() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.set_name("Unfilled").unwrap();
            ws.write_string(0, 0, "u").unwrap();
            ws.write_string(1, 0, "1").unwrap();
            let ws = wb.add_worksheet();
            ws.set_name("Filled").unwrap();
            ws.write_string(0, 0, "f").unwrap();
            ws.write_string(1, 0, "2").unwrap();
        });
        let first = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(first.sheet, "Unfilled");
        assert_eq!(first.columns, vec!["u"]);
        assert_eq!(first.sheets, vec!["Unfilled", "Filled"]);
        let named = parse_spreadsheet(&body, Some("Filled")).unwrap();
        assert_eq!(named.sheet, "Filled");
        assert_eq!(named.rows[0]["f"], "2");
    }

    #[test]
    fn parse_spreadsheet_unknown_sheet_error_lists_the_inventory() {
        let body = workbook(|wb| {
            wb.add_worksheet().write_string(0, 0, "a").unwrap();
        });
        let err = parse_spreadsheet(&body, Some("Nope")).unwrap_err();
        assert!(err.to_string().contains("\"Nope\" not found"), "{err}");
        assert!(err.to_string().contains("Sheet1"), "{err}");
    }

    #[test]
    fn parse_spreadsheet_empty_sheet_is_an_empty_parse() {
        let body = workbook(|wb| {
            wb.add_worksheet();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert!(parsed.columns.is_empty());
        assert!(parsed.rows.is_empty());
    }

    #[test]
    fn parse_spreadsheet_rejects_non_workbook_bytes() {
        assert!(parse_spreadsheet(&Bytes::from_static(b"a,b\n1,2\n"), None).is_err());
        assert!(parse_spreadsheet(&Bytes::new(), None).is_err());
    }

    #[test]
    fn parse_spreadsheet_stray_far_cell_errors_instead_of_exploding() {
        // A tiny file whose two cells imply a multi-billion-cell bounding box.
        // The dense-range API would try to allocate that box; the streaming
        // scan must refuse at the cap without any large allocation.
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 0, "a").unwrap();
            ws.write_string(1_000_000, 3_000, "stray").unwrap();
        });
        let err = parse_spreadsheet(&body, None).unwrap_err();
        assert!(err.to_string().contains("cells"), "{err}");
    }

    #[test]
    fn parse_spreadsheet_defaults_to_the_first_visible_worksheet() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.set_name("Meta").unwrap();
            ws.set_hidden(true);
            ws.write_string(0, 0, "internal").unwrap();
            let ws = wb.add_worksheet();
            ws.set_name("Data").unwrap();
            // The writer force-unhides the active sheet at save time, and the
            // first sheet is active unless another claims it — as in any real
            // workbook whose first sheet is hidden.
            ws.set_active(true);
            ws.write_string(0, 0, "v").unwrap();
            ws.write_string(1, 0, "1").unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(
            parsed.sheet, "Data",
            "hidden first sheet is not the default"
        );
        assert_eq!(
            parsed.sheets,
            vec!["Meta", "Data"],
            "inventory still lists it"
        );
        let named = parse_spreadsheet(&body, Some("Meta")).unwrap();
        assert_eq!(
            named.columns,
            vec!["internal"],
            "but it is reachable by name"
        );
    }

    #[test]
    fn parse_spreadsheet_data_away_from_a1_keeps_its_own_origin() {
        let body = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(1, 1, "h").unwrap();
            ws.write_string(2, 1, "v").unwrap();
        });
        let parsed = parse_spreadsheet(&body, None).unwrap();
        assert_eq!(
            parsed.columns,
            vec!["h"],
            "no phantom columns/rows for the empty margin above and left of the content"
        );
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0]["h"], "v");
    }
}
