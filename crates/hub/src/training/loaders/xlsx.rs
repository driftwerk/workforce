//! Spreadsheet loader: one `RawChunk` per sheet, rendered as a
//! markdown table. Locator format: `sheet:<name>`.
//!
//! Each sheet becomes a single chunk for now — agents recall whole
//! sheets when relevant terms hit. A future refinement can split
//! long sheets into row-range chunks (`Sheet1!A1:B500`), but the
//! prompt budget rarely benefits from that for the corpus sizes we
//! see today.
//!
//! Backed by the shared bounded reader (`workforce_agent::spreadsheet`),
//! so a crafted or stray-cell workbook cannot blow up the hub; only
//! xlsx/xlsm parse (legacy formats error with a re-save hint).

use eyre::{eyre, Result};
use workforce_agent::spreadsheet::{display_cell, Workbook};

use super::RawChunk;

/// Per-sheet row cap. Spreadsheets with very wide tabular data are
/// almost never useful past the first few hundred rows when used as
/// reference material — the agent isn't going to scan a 50,000-row
/// table to answer a question. Truncate with a clear marker.
const MAX_ROWS_PER_SHEET: usize = 500;

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let mut workbook = Workbook::open(bytes)?;
    let sheet_names = workbook.sheet_names();
    let mut chunks = Vec::new();

    for name in &sheet_names {
        let Ok(grid) = workbook.read_grid(name, display_cell) else {
            continue;
        };
        let height = grid.height();
        if height == 0 || grid.width() == 0 {
            continue;
        }

        let mut body = String::new();
        body.push_str(&format!("# Sheet: {}\n\n", name));
        let mut total_rows = 0;
        for (idx, row) in grid.rows.iter().enumerate() {
            if idx >= MAX_ROWS_PER_SHEET {
                body.push_str(&format!(
                    "\n_(truncated at {MAX_ROWS_PER_SHEET} rows of {height})_\n",
                ));
                break;
            }
            body.push_str("| ");
            body.push_str(&row.join(" | "));
            body.push_str(" |\n");
            if idx == 0 {
                body.push('|');
                for _ in row {
                    body.push_str("---|");
                }
                body.push('\n');
            }
            total_rows += 1;
        }

        if total_rows == 0 {
            continue;
        }
        chunks.push(RawChunk {
            title: Some(format!("Sheet {name}")),
            body: body.trim_end().to_string(),
            locator: format!("sheet:{name}"),
            tags: vec![format!("sheet:{name}")],
        });
    }

    if chunks.is_empty() {
        return Err(eyre!("spreadsheet contained no readable sheets"));
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_errors() {
        assert!(parse(b"").is_err());
        assert!(parse(b"not a spreadsheet").is_err());
    }

    #[test]
    fn parses_sheets_into_markdown_chunks() {
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Roster").unwrap();
        ws.write_string(0, 0, "Name").unwrap();
        ws.write_string(0, 1, "Role").unwrap();
        ws.write_string(1, 0, "Ann").unwrap();
        ws.write_string(1, 1, "Clerk").unwrap();
        ws.write_number(2, 0, 826281843.0).unwrap();
        let bytes = wb.save_to_buffer().unwrap();

        let chunks = parse(&bytes).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].locator, "sheet:Roster");
        let body = &chunks[0].body;
        assert!(body.contains("| Name | Role |"), "{body}");
        assert!(body.contains("| Ann | Clerk |"), "{body}");
        assert!(
            body.contains("| 826281843 |"),
            "numeric cells render as bare integers: {body}"
        );
    }
}
