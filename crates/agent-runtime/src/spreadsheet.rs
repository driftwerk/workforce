//! Bounded reading of untrusted OOXML workbooks (xlsx/xlsm), shared by every
//! site that feeds calamine bytes it did not author: uploaded attachments,
//! training corpus files, Xero consolidation templates, and the `aws.s3.*`
//! body tools.
//!
//! calamine's `worksheet_range` is UNSAFE on untrusted bytes: it
//! dense-allocates a `Vec<Data>` sized by the cell *coordinate* bounding box,
//! so a few-KB workbook with one cell at `XFD1048576` demands hundreds of
//! gigabytes before any caller check could run. This module streams cells
//! through calamine's lazy reader instead, enforcing caps per cell, and is
//! why only the OOXML formats are supported — legacy `xls`/`xlsb`/`ods` have
//! no bounded parse path in calamine. calamine can also *panic* (rather than
//! error) on some malformed workbooks, so every parse here runs under a
//! panic guard.

use std::collections::BTreeMap;
use std::io::Cursor;

use calamine::{DataRef, Reader, Sheet, Xlsx};
use eyre::{eyre, Result};

/// Hard cap on one sheet, enforced DURING the streaming cell scan — on both
/// the count of parsed cells and the content bounding box. Nothing larger
/// than the cap is ever allocated, keeping worst-case transient memory in
/// the same class as parsing a byte-capped CSV.
pub const MAX_SHEET_CELLS: usize = 4_000_000;

/// An open workbook borrowing the caller's bytes. Sheet metadata is parsed
/// eagerly (bounded by the byte cap the caller applied to the fetch); cell
/// data streams per [`read_grid`](Self::read_grid) call.
pub struct Workbook<'a> {
    inner: Xlsx<Cursor<&'a [u8]>>,
}

impl<'a> Workbook<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let inner = catch_parse_panic(|| {
            Xlsx::new(Cursor::new(bytes)).map_err(|e| {
                // Only suggest re-saving when the bytes aren't a zip at all —
                // a corrupt genuine .xlsx should not be told to become one.
                if bytes.starts_with(b"PK") {
                    eyre!("cannot open workbook: {e}")
                } else {
                    eyre!(
                        "cannot open workbook (only xlsx/xlsm are supported; re-save as .xlsx): {e}"
                    )
                }
            })
        })?;
        Ok(Self { inner })
    }

    pub fn sheet_names(&self) -> Vec<String> {
        self.inner.sheet_names()
    }

    pub fn sheets_metadata(&self) -> &[Sheet] {
        self.inner.sheets_metadata()
    }

    /// Stream one sheet's cells into a dense bounding-box grid of `T`, with
    /// both caps enforced per streamed cell BEFORE anything coordinate-sized
    /// is allocated: the count of parsed cells (duplicate coordinates
    /// included, so the scan is time-bounded too) and the content
    /// bounding-box area. Style-only `Empty` cells — the classic Ctrl+End
    /// artifact — are skipped so they can't inflate the box.
    ///
    /// Contract: absent and `Empty` cells are indistinguishable — both yield
    /// `T::default()`.
    pub fn read_grid<T: Default>(
        &mut self,
        sheet: &str,
        cell: impl Fn(&DataRef<'_>) -> T,
    ) -> Result<Grid<T>> {
        catch_parse_panic(|| self.stream_grid(sheet, cell))
    }

    fn stream_grid<T: Default>(
        &mut self,
        sheet: &str,
        cell: impl Fn(&DataRef<'_>) -> T,
    ) -> Result<Grid<T>> {
        let mut reader = self
            .inner
            .worksheet_cells_reader(sheet)
            .map_err(|e| eyre!("cannot read sheet {sheet:?}: {e}"))?;
        let mut cells: BTreeMap<(u32, u32), T> = BTreeMap::new();
        let mut bounds: Option<Bounds> = None;
        let mut parsed = 0usize;
        while let Some(c) = reader
            .next_cell()
            .map_err(|e| eyre!("cannot read sheet {sheet:?}: {e}"))?
        {
            if matches!(c.get_value(), DataRef::Empty) {
                continue;
            }
            parsed += 1;
            if parsed > MAX_SHEET_CELLS {
                return Err(over_cap(sheet));
            }
            let pos = c.get_position();
            let b = bounds.get_or_insert(Bounds::at(pos));
            b.widen(pos);
            if b.cell_count() > MAX_SHEET_CELLS {
                return Err(over_cap(sheet));
            }
            cells.insert(pos, cell(c.get_value()));
        }
        let Some(b) = bounds else {
            return Ok(Grid { rows: Vec::new() });
        };
        let rows = (0..b.height())
            .map(|offset| {
                let row = b.min.0 + offset as u32;
                (0..b.width())
                    .map(|i| cells.remove(&(row, b.min.1 + i as u32)).unwrap_or_default())
                    .collect()
            })
            .collect();
        Ok(Grid { rows })
    }
}

/// One sheet's content bounding box as dense rows; `rows[0]` is the first
/// row that contains any content (leading empty rows/columns are not part of
/// the box).
#[derive(Debug)]
pub struct Grid<T> {
    pub rows: Vec<Vec<T>>,
}

impl<T> Grid<T> {
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    pub fn width(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }
}

/// calamine can panic rather than error on some malformed workbooks (e.g. an
/// out-of-range shared-string index). Callers feed it bytes from uploads and
/// customer-controlled buckets, so a corrupt file must surface as that
/// file's parse error, never as a turn-killing unwind.
fn catch_parse_panic<T>(parse: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(parse))
        .unwrap_or_else(|_| Err(eyre!("workbook parse panicked; the file is malformed")))
}

fn over_cap(sheet: &str) -> eyre::Report {
    eyre!("sheet {sheet:?} spans over {MAX_SHEET_CELLS} cells; export a narrower extract instead")
}

/// The content bounding box, grown as cells stream in. `height × width` is
/// exactly the dense area the assembled grid will cover.
struct Bounds {
    min: (u32, u32),
    max: (u32, u32),
}

impl Bounds {
    fn at(pos: (u32, u32)) -> Self {
        Self { min: pos, max: pos }
    }

    fn widen(&mut self, (row, col): (u32, u32)) {
        self.min = (self.min.0.min(row), self.min.1.min(col));
        self.max = (self.max.0.max(row), self.max.1.max(col));
    }

    fn height(&self) -> usize {
        (self.max.0 - self.min.0) as usize + 1
    }

    fn width(&self) -> usize {
        (self.max.1 - self.min.1) as usize + 1
    }

    fn cell_count(&self) -> usize {
        self.height().saturating_mul(self.width())
    }
}

/// The one display rendering for spreadsheet cells: every variant to the
/// string a human would type when searching for it. Integral floats collapse
/// (`826281843.0` → `"826281843"`, preserving ID greppability), datetimes
/// render ISO, and error cells render the Excel-native form (`#DIV/0!`).
pub fn display_cell(cell: &DataRef<'_>) -> String {
    match cell {
        DataRef::Empty => String::new(),
        DataRef::String(s) => s.clone(),
        DataRef::SharedString(s) => (*s).to_string(),
        DataRef::Int(i) => i.to_string(),
        DataRef::Float(f) if *f == (*f as i64) as f64 => (*f as i64).to_string(),
        DataRef::Float(f) => format!("{f}"),
        DataRef::Bool(b) => b.to_string(),
        DataRef::DateTime(dt) => datetime_string(dt),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.clone(),
        DataRef::Error(e) => e.to_string(),
    }
}

/// Render an Excel datetime the way a human would type it in a filter — a
/// bare date when there is no time component — falling back to the raw
/// serial for durations and out-of-range values.
fn datetime_string(dt: &calamine::ExcelDateTime) -> String {
    match dt.is_datetime().then(|| dt.as_datetime()).flatten() {
        Some(d) if d.time() == chrono::NaiveTime::MIN => d.format("%Y-%m-%d").to_string(),
        Some(d) => d.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => dt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workbook(build: impl FnOnce(&mut rust_xlsxwriter::Workbook)) -> Vec<u8> {
        let mut wb = rust_xlsxwriter::Workbook::new();
        build(&mut wb);
        wb.save_to_buffer().expect("save workbook")
    }

    #[test]
    fn read_grid_returns_dense_bounding_box_with_default_gaps() {
        let bytes = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(1, 1, "a").unwrap();
            ws.write_string(3, 2, "b").unwrap();
        });
        let mut wb = Workbook::open(&bytes).unwrap();
        let grid = wb.read_grid("Sheet1", display_cell).unwrap();
        assert_eq!(grid.height(), 3, "box spans rows 1..=3");
        assert_eq!(grid.width(), 2, "box spans cols 1..=2");
        assert_eq!(grid.rows[0], vec!["a", ""]);
        assert_eq!(grid.rows[1], vec!["", ""], "interior gap row is defaults");
        assert_eq!(grid.rows[2], vec!["", "b"]);
    }

    #[test]
    fn read_grid_stray_far_cell_errors_instead_of_exploding() {
        // Two cells implying a multi-billion-cell bounding box: the dense
        // range API would try to allocate that box; the streaming scan must
        // refuse at the cap without any large allocation.
        let bytes = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_string(0, 0, "a").unwrap();
            ws.write_string(1_000_000, 3_000, "stray").unwrap();
        });
        let mut wb = Workbook::open(&bytes).unwrap();
        let err = wb.read_grid("Sheet1", display_cell).unwrap_err();
        assert!(err.to_string().contains("cells"), "{err}");
    }

    #[test]
    fn read_grid_empty_sheet_is_an_empty_grid() {
        let bytes = workbook(|wb| {
            wb.add_worksheet();
        });
        let mut wb = Workbook::open(&bytes).unwrap();
        let grid = wb.read_grid("Sheet1", display_cell).unwrap();
        assert!(grid.rows.is_empty());
        assert_eq!(grid.width(), 0);
    }

    #[test]
    fn read_grid_unknown_sheet_errors() {
        let bytes = workbook(|wb| {
            wb.add_worksheet().write_string(0, 0, "a").unwrap();
        });
        let mut wb = Workbook::open(&bytes).unwrap();
        assert!(wb.read_grid("Nope", display_cell).is_err());
    }

    #[test]
    fn open_rejects_non_workbook_bytes_with_an_actionable_error() {
        for bytes in [
            b"a,b\n1,2\n".as_slice(),
            b"".as_slice(),
            // CFB magic — a legacy .xls container.
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1rest".as_slice(),
        ] {
            let Err(err) = Workbook::open(bytes) else {
                panic!("non-workbook bytes must not open");
            };
            assert!(err.to_string().contains("xlsx/xlsm"), "{err}");
        }
    }

    #[test]
    fn open_corrupt_zip_does_not_suggest_resaving_as_xlsx() {
        // A truncated genuine .xlsx is a zip that fails to open — telling the
        // user to re-save it as xlsx would misdiagnose the problem.
        let Err(err) = Workbook::open(b"PK\x03\x04garbage") else {
            panic!("corrupt zip must not open");
        };
        assert!(!err.to_string().contains("re-save"), "{err}");
        assert!(err.to_string().contains("cannot open workbook"), "{err}");
    }

    #[test]
    fn catch_parse_panic_converts_panics_to_errors() {
        let err = catch_parse_panic::<()>(|| panic!("boom")).unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
        assert_eq!(catch_parse_panic(|| Ok(1)).unwrap(), 1);
    }

    #[test]
    fn display_cell_covers_each_variant() {
        use calamine::CellErrorType;
        assert_eq!(display_cell(&DataRef::Empty), "");
        assert_eq!(display_cell(&DataRef::String("x".into())), "x");
        assert_eq!(display_cell(&DataRef::SharedString("shared")), "shared");
        assert_eq!(display_cell(&DataRef::Int(42)), "42");
        assert_eq!(display_cell(&DataRef::Float(826281843.0)), "826281843");
        assert_eq!(display_cell(&DataRef::Float(2.5)), "2.5");
        assert_eq!(display_cell(&DataRef::Bool(false)), "false");
        assert_eq!(
            display_cell(&DataRef::DateTimeIso("2026-08-20".into())),
            "2026-08-20"
        );
        assert_eq!(
            display_cell(&DataRef::Error(CellErrorType::Div0)),
            "#DIV/0!",
            "error cells render the Excel-native form a human would search for"
        );
    }

    #[test]
    fn display_cell_renders_dates_the_way_humans_type_them() {
        use rust_xlsxwriter::{ExcelDateTime, Format};
        let dt_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
        let bytes = workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.write_datetime_with_format(
                0,
                0,
                ExcelDateTime::from_ymd(2026, 8, 20)
                    .unwrap()
                    .and_hms(8, 10, 42)
                    .unwrap(),
                &dt_fmt,
            )
            .unwrap();
        });
        let mut wb = Workbook::open(&bytes).unwrap();
        let grid = wb.read_grid("Sheet1", display_cell).unwrap();
        assert_eq!(grid.rows[0][0], "2026-08-20 08:10:42");
    }
}
