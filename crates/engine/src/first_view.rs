//! Where the host's read-only first view was left, as a place in the workbook (OneOS progressive open, 2026-10-27).
//!
//! The viewer showed the sheet as a scrolling table; the host says how far across and down it was (fractions of the scrollable
//! extent) and which text, if any, was selected. The editor opens at the same CELLS, not the same pixels: the top-left visible cell is
//! the one at that fraction of the used range, and the active cell is the cell whose text is the selected text (the one nearest the top
//! edge), or else the top-left visible cell. Pure: the sheet in, two cells out.

use gridcraft_core::CellRef;
use gridcraft_model::{Sheet, Workbook};

/// The first view as cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstViewPlace {
    /// The cell to select.
    pub cell: CellRef,
    /// The first visible row and column.
    pub top_left: CellRef,
}

fn along(first: u32, last: u32, fraction: f64) -> u32 {
    let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    first + (f64::from(last.saturating_sub(first)) * f).round() as u32
}

/// The place in `sheet` for a first view scrolled to `(x_fraction, y_fraction)` with `selection` selected. `None` for an empty sheet.
pub fn locate(wb: &Workbook, sheet: &Sheet, selection: &str, x_fraction: f64, y_fraction: f64) -> Option<FirstViewPlace> {
    let used = sheet.used_range()?;
    let top_left = CellRef::new(along(used.start.row, used.end.row, y_fraction), along(used.start.col, used.end.col, x_fraction));
    // The first piece of the selection that says anything (a selection across cells reads as tab- and line-separated text).
    let wanted = selection.split(['\t', '\n', '\r']).map(str::trim).find(|p| !p.is_empty());
    if let Some(wanted) = wanted {
        let mut best: Option<(u32, CellRef)> = None;
        for (at, _) in sheet.cells.iter() {
            let distance = at.row.abs_diff(top_left.row);
            if best.is_some_and(|(d, b)| (distance, at.row, at.col) >= (d, b.row, b.col)) {
                continue;
            }
            if crate::display::cell_text(wb, sheet, at).trim() == wanted {
                best = Some((distance, at));
            }
        }
        if let Some((_, cell)) = best {
            return Some(FirstViewPlace { cell, top_left });
        }
    }
    Some(FirstViewPlace { cell: top_left, top_left })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use serde_json::json;

    fn book() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        // 100 rows: A is the row's name, B its number.
        for r in 1..=100u32 {
            s.execute("cell.set", json!({"cell": format!("A{r}"), "input": format!("item{r}")})).unwrap();
            s.execute("cell.set", json!({"cell": format!("B{r}"), "input": (r * 10).to_string()})).unwrap();
        }
        s
    }

    fn place(s: &Session, selection: &str, x: f64, y: f64) -> Option<FirstViewPlace> {
        let d = s.doc().unwrap();
        locate(&d.wb, d.wb.active().unwrap(), selection, x, y)
    }

    #[test]
    fn the_top_left_cell_is_the_fraction_of_the_used_range() {
        let s = book();
        assert_eq!(place(&s, "", 0.0, 0.0).map(|p| p.top_left), Some(CellRef::new(0, 0)));
        assert_eq!(place(&s, "", 0.0, 1.0).map(|p| p.top_left), Some(CellRef::new(99, 0)));
        assert_eq!(place(&s, "", 1.0, 0.5).map(|p| p.top_left), Some(CellRef::new(50, 1)));
        assert_eq!(place(&s, "", f64::NAN, f64::INFINITY).map(|p| p.top_left), Some(CellRef::new(0, 0)), "hostile numbers are the top-left");
    }

    #[test]
    fn the_selected_text_is_the_cell_nearest_the_top_edge() {
        let mut s = book();
        // The same text twice: the one nearest the scroll position wins.
        s.execute("cell.set", json!({"cell": "C90", "input": "item5"})).unwrap();
        assert_eq!(place(&s, "item5", 0.0, 0.0).map(|p| p.cell), Some(CellRef::parse("A5").unwrap()));
        assert_eq!(place(&s, "item5", 0.0, 1.0).map(|p| p.cell), Some(CellRef::parse("C90").unwrap()));
        // A selection across cells is placed by its first piece.
        assert_eq!(place(&s, "item7\t70\nitem8\t80", 0.0, 0.0).map(|p| p.cell), Some(CellRef::parse("A7").unwrap()));
        assert_eq!(place(&s, "300", 0.0, 0.0).map(|p| p.cell), Some(CellRef::parse("B30").unwrap()), "a number as it is shown");
    }

    #[test]
    fn text_that_is_not_in_the_sheet_selects_the_top_left_cell_and_an_empty_sheet_has_no_place() {
        let s = book();
        let p = place(&s, "nowhere", 0.0, 0.5).unwrap();
        assert_eq!(p.cell, p.top_left);
        let mut empty = Session::new();
        empty.new_workbook();
        assert_eq!(place(&empty, "x", 0.0, 0.0), None);
    }
}
