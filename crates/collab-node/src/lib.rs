//! The collaboration shim for node: one [`Window`] is one editor window's side of the host bridge, with the workbook held as the
//! model so a test (OneOS's `craft-sheets-convergence.test.ts`) can drive two windows through the host's real adapter and CRDT
//! engine and compare what they end up showing. Everything the browser build shares is here: sheets, rows and columns, cells,
//! styles, sizes, merges, comments and cursors.

use std::sync::Arc;

use gridcraft_collab::comments::CommentMsg;
use gridcraft_collab::{Batch, Change, Shim, changes_of, digest};
use gridcraft_core::{CellRef, RangeRef, Value as Cv};
use gridcraft_model::{Cell, CellStore, Comment, Formula, LineInfo, Sheet, StyleId, Workbook};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn batches_json(batches: &[Batch]) -> String {
    Value::Array(batches.iter().map(|b| json!({"seq": b.seq, "ops": b.ops, "label": b.label, "undo": if b.join { "join" } else { "step" }, "seed": b.seed})).collect())
        .to_string()
}

/// `B7` as a cell.
fn a1(s: &str) -> Result<CellRef, JsError> {
    let letters: String = s.chars().take_while(char::is_ascii_alphabetic).collect();
    let digits = &s[letters.len()..];
    let col = letters.chars().fold(0u32, |n, c| n * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1));
    let row: u32 = digits.parse().map_err(|_| JsError::new(&format!("not a cell: {s}")))?;
    if col == 0 || row == 0 {
        return Err(JsError::new(&format!("not a cell: {s}")));
    }
    Ok(CellRef::new(row - 1, col - 1))
}

/// What a person types into a cell: a number, `=formula`, `true`/`false` or text.
fn cell_from_input(input: &str, style: StyleId) -> Cell {
    let mut cell = if let Some(f) = input.strip_prefix('=') {
        Cell::formula(Formula::new(f))
    } else if let Ok(n) = input.parse::<f64>() {
        Cell::value(Cv::Number(n))
    } else if input == "true" || input == "false" {
        Cell::value(Cv::Bool(input == "true"))
    } else {
        Cell::value(Cv::Text(Arc::from(input)))
    };
    cell.style = style;
    cell
}

#[wasm_bindgen]
pub struct Window {
    shim: Shim,
    wb: Workbook,
    me: String,
}

impl Window {
    fn sheet_mut(&mut self, i: u32) -> Result<&mut Sheet, JsError> {
        self.wb.sheets.get_mut(i as usize).map(Arc::make_mut).ok_or_else(|| JsError::new("no such sheet"))
    }

    /// Shift the rows (`axis_rows`) or columns of a sheet from `at` by `by` (negative: the lines `at..at-by` are deleted).
    fn shift(&mut self, sheet: u32, rows: bool, at: u32, by: i64) -> Result<(), JsError> {
        let s = self.sheet_mut(sheet)?;
        let map = |p: CellRef| -> Option<CellRef> {
            let v = if rows { p.row } else { p.col };
            let nv = if by >= 0 {
                if v >= at { v + by as u32 } else { v }
            } else {
                let n = (-by) as u32;
                if v >= at + n {
                    v - n
                } else if v >= at {
                    return None;
                } else {
                    v
                }
            };
            Some(if rows { CellRef::new(nv, p.col) } else { CellRef::new(p.row, nv) })
        };
        let cells: Vec<(CellRef, Cell)> = s.cells.iter().map(|(p, c)| (p, c.clone())).collect();
        s.cells = CellStore::new();
        for (p, c) in cells {
            if let Some(q) = map(p) {
                s.cells.set(q, c);
            }
        }
        s.comments = std::mem::take(&mut s.comments).into_iter().filter_map(|(p, c)| map(p).map(|q| (q, c))).collect();
        // Merged cells move with the lines; one that loses a corner goes.
        s.merges = std::mem::take(&mut s.merges).into_iter().filter_map(|m| Some(RangeRef::new(map(m.start)?, map(m.end)?))).collect();
        let lines = if rows { &mut s.rows } else { &mut s.cols };
        *lines = std::mem::take(lines)
            .into_iter()
            .filter_map(|(i, l)| {
                let p = if rows { CellRef::new(i, 0) } else { CellRef::new(0, i) };
                map(p).map(|q| (if rows { q.row } else { q.col }, l))
            })
            .collect();
        Ok(())
    }
}

#[wasm_bindgen]
impl Window {
    /// A window showing `book_json`: `{sheets: [{name, cells: {"A1": 1, "B2": "text", "C1": "=A1+B1"}}]}`; its ids drawn from `seed`.
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u32, book_json: &str) -> Result<Window, JsError> {
        let book: Value = serde_json::from_str(book_json).map_err(|e| JsError::new(&format!("book: {e}")))?;
        let mut wb = Workbook::new();
        if let Some(sheets) = book.get("sheets").and_then(Value::as_array) {
            wb.sheets.clear();
            for s in sheets {
                let mut sheet = Sheet::new(s.get("name").and_then(Value::as_str).unwrap_or("Sheet1"));
                if let Some(cells) = s.get("cells").and_then(Value::as_object) {
                    for (k, v) in cells {
                        let input = match v {
                            Value::String(t) => t.clone(),
                            other => other.to_string(),
                        };
                        sheet.cells.set(a1(k)?, cell_from_input(&input, StyleId::DEFAULT));
                    }
                }
                wb.sheets.push(Arc::new(sheet));
            }
            if wb.sheets.is_empty() {
                wb.sheets.push(Arc::new(Sheet::new("Sheet1")));
            }
        }
        Ok(Window { shim: Shim::new(u64::from(seed) * 0x9e37_79b9 + 1), wb, me: format!("Window {seed}") })
    }

    /// The seed batches for the opened workbook (an array of `{seq, ops, label, undo, seed}`).
    pub fn seed(&mut self, now: f64) -> Result<String, JsError> {
        let batches = self.shim.seed_all(&self.wb, now as u64).map_err(err)?;
        self.shim.resync(&self.wb);
        Ok(batches_json(&batches))
    }

    /// The person edited the workbook: answers the batches to send.
    pub fn flush(&mut self, now: f64) -> Result<String, JsError> {
        let batches = self.shim.local_all(&self.wb, None, now as u64).map_err(err)?;
        Ok(batches_json(&batches))
    }

    /// The host sent `craft:crdt:state`: `doc_json` is its `doc`, `comments_json` its `comments`.
    pub fn state(&mut self, doc_json: &str, comments_json: &str) -> Result<(), JsError> {
        let doc: Value = serde_json::from_str(doc_json).map_err(|e| JsError::new(&format!("state: {e}")))?;
        self.shim.remote_state(&doc).map_err(err)?;
        let comments: Value = serde_json::from_str(comments_json).unwrap_or(Value::Null);
        self.shim.remote_comments(&comments);
        self.take_in();
        Ok(())
    }

    /// The host sent `craft:crdt:delta`: `changes_json` is its `changes` (paths below `doc` carry the leading `doc`; a change at
    /// `["comments"]` is the whole comments value); answers whether the workbook changed.
    pub fn delta(&mut self, changes_json: &str, ack: f64, now: f64) -> Result<bool, JsError> {
        let all: Value = serde_json::from_str(changes_json).map_err(|e| JsError::new(&format!("changes: {e}")))?;
        let changes = changes_of(&all);
        self.shim.remote_comment_changes(&changes);
        let doc_changes: Vec<Change> = changes
            .into_iter()
            .filter_map(|mut c| {
                (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                    c.path.remove(0);
                    c
                })
            })
            .collect();
        self.shim.remote_delta(&doc_changes, ack as u64, now as u64).map_err(err)?;
        Ok(self.take_in())
    }

    /// The comment messages the last local calls produced (an array of `craft:crdt:comment` bodies).
    pub fn comment_msgs(&mut self) -> String {
        Value::Array(self.shim.take_comment_msgs().iter().map(CommentMsg::body).collect()).to_string()
    }

    /// What the workbook shows of what the shim shares, as one comparable JSON string.
    pub fn digest(&self) -> String {
        digest(&self.wb).to_string()
    }

    // ── editing, as a person does ─────────────────────────────────────────────────────────────────────────────────

    /// Type `input` into a cell (`=formula`, a number, `true`/`false` or text); an empty input clears it. The style stays.
    pub fn set_cell(&mut self, sheet: u32, cell: &str, input: &str) -> Result<(), JsError> {
        let at = a1(cell)?;
        let s = self.sheet_mut(sheet)?;
        let style = s.cells.get(at).map_or(StyleId::DEFAULT, |c| c.style);
        if input.is_empty() {
            let blank = Cell { value: Cv::Empty, formula: None, style };
            s.cells.set(at, blank);
        } else {
            s.cells.set(at, cell_from_input(input, style));
        }
        Ok(())
    }

    /// Make a cell bold (or not): its style is the one it has with the flag.
    pub fn set_bold(&mut self, sheet: u32, cell: &str, bold: bool) -> Result<(), JsError> {
        let at = a1(cell)?;
        let cur = self.wb.sheets.get(sheet as usize).and_then(|s| s.cells.get(at)).cloned().unwrap_or_default();
        let id = self.wb.styles.derive(cur.style, |s| s.font.bold = bold);
        self.sheet_mut(sheet)?.cells.set(at, Cell { style: id, ..cur });
        Ok(())
    }

    /// Give a cell a number format (its style is the one it has with the format).
    pub fn set_num_fmt(&mut self, sheet: u32, cell: &str, fmt: &str) -> Result<(), JsError> {
        let at = a1(cell)?;
        let cur = self.wb.sheets.get(sheet as usize).and_then(|s| s.cells.get(at)).cloned().unwrap_or_default();
        let id = self.wb.styles.derive(cur.style, |s| s.num_fmt = gridcraft_model::NumFmt(Arc::from(fmt)));
        self.sheet_mut(sheet)?.cells.set(at, Cell { style: id, ..cur });
        Ok(())
    }

    pub fn insert_rows(&mut self, sheet: u32, at: u32, n: u32) -> Result<(), JsError> {
        self.shift(sheet, true, at, i64::from(n))
    }

    pub fn delete_rows(&mut self, sheet: u32, at: u32, n: u32) -> Result<(), JsError> {
        self.shift(sheet, true, at, -i64::from(n))
    }

    pub fn insert_cols(&mut self, sheet: u32, at: u32, n: u32) -> Result<(), JsError> {
        self.shift(sheet, false, at, i64::from(n))
    }

    pub fn delete_cols(&mut self, sheet: u32, at: u32, n: u32) -> Result<(), JsError> {
        self.shift(sheet, false, at, -i64::from(n))
    }

    pub fn add_sheet(&mut self, name: &str) {
        self.wb.sheets.push(Arc::new(Sheet::new(name)));
    }

    pub fn rename_sheet(&mut self, sheet: u32, name: &str) -> Result<(), JsError> {
        self.sheet_mut(sheet)?.name = name.to_owned();
        Ok(())
    }

    pub fn move_sheet(&mut self, from: u32, to: u32) -> Result<(), JsError> {
        if from as usize >= self.wb.sheets.len() || to as usize >= self.wb.sheets.len() {
            return Err(JsError::new("no such sheet"));
        }
        let s = self.wb.sheets.remove(from as usize);
        self.wb.sheets.insert(to as usize, s);
        Ok(())
    }

    pub fn delete_sheet(&mut self, sheet: u32) -> Result<(), JsError> {
        if sheet as usize >= self.wb.sheets.len() || self.wb.sheets.len() == 1 {
            return Err(JsError::new("a workbook keeps one sheet"));
        }
        self.wb.sheets.remove(sheet as usize);
        Ok(())
    }

    pub fn set_row_height(&mut self, sheet: u32, row: u32, px: f32) -> Result<(), JsError> {
        self.sheet_mut(sheet)?.rows.insert(row, LineInfo { size: Some(px), custom: true, ..Default::default() });
        Ok(())
    }

    pub fn set_col_width(&mut self, sheet: u32, col: u32, px: f32) -> Result<(), JsError> {
        self.sheet_mut(sheet)?.cols.insert(col, LineInfo { size: Some(px), custom: true, ..Default::default() });
        Ok(())
    }

    pub fn hide_row(&mut self, sheet: u32, row: u32, hidden: bool) -> Result<(), JsError> {
        let s = self.sheet_mut(sheet)?;
        let l = s.rows.entry(row).or_default();
        l.hidden = hidden;
        Ok(())
    }

    pub fn merge(&mut self, sheet: u32, from: &str, to: &str) -> Result<(), JsError> {
        let r = RangeRef::new(a1(from)?, a1(to)?);
        self.sheet_mut(sheet)?.merges.push(r);
        Ok(())
    }

    pub fn freeze(&mut self, sheet: u32, rows: u32, cols: u32) -> Result<(), JsError> {
        self.sheet_mut(sheet)?.freeze = (rows > 0 || cols > 0).then_some((rows, cols));
        Ok(())
    }

    /// A comment on a cell by this window's person (a note that becomes a thread in the host's comments tier).
    pub fn add_comment(&mut self, sheet: u32, cell: &str, text: &str) -> Result<(), JsError> {
        let at = a1(cell)?;
        let me = self.me.clone();
        self.sheet_mut(sheet)?.comments.insert(
            at,
            Comment { author: me, text: text.to_owned(), replies: vec![], threaded: true, resolved: false, visible: false, dates: vec![], note_box: None },
        );
        Ok(())
    }

    pub fn reply_comment(&mut self, sheet: u32, cell: &str, text: &str) -> Result<(), JsError> {
        let at = a1(cell)?;
        let me = self.me.clone();
        if let Some(c) = self.sheet_mut(sheet)?.comments.get_mut(&at) {
            c.replies.push((me, text.to_owned()));
        }
        Ok(())
    }

    pub fn resolve_comment(&mut self, sheet: u32, cell: &str, resolved: bool) -> Result<(), JsError> {
        let at = a1(cell)?;
        if let Some(c) = self.sheet_mut(sheet)?.comments.get_mut(&at) {
            c.resolved = resolved;
        }
        Ok(())
    }

    pub fn delete_comment(&mut self, sheet: u32, cell: &str) -> Result<(), JsError> {
        let at = a1(cell)?;
        self.sheet_mut(sheet)?.comments.remove(&at);
        Ok(())
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────

    /// The `craft:crdt:cursor` body for a selection on a sheet, from `anchor` to `focus`.
    pub fn cursor(&self, sheet: u32, anchor: &str, focus: &str) -> Result<String, JsError> {
        Ok(self.shim.cursor_for(sheet as usize, a1(anchor)?, a1(focus)?).map(|v| v.to_string()).unwrap_or_default())
    }

    /// Where another person is, in this window: `{sheet, anchor, focus}` as A1 text, or an empty string.
    pub fn peer(&self, cursor_json: &str) -> String {
        let Ok(v) = serde_json::from_str::<Value>(cursor_json) else { return String::new() };
        match self.shim.peer_cursor(&v) {
            Some(p) => json!({"sheet": p.sheet, "anchor": p.anchor.a1(), "focus": p.focus.a1()}).to_string(),
            None => String::new(),
        }
    }

    /// A cell as the window shows it: `{v, f}` or an empty string.
    pub fn read(&self, sheet: u32, cell: &str) -> Result<String, JsError> {
        let at = a1(cell)?;
        Ok(self
            .wb
            .sheets
            .get(sheet as usize)
            .and_then(|s| s.cells.get(at))
            .map(|c| json!({"v": serde_json::to_value(&c.value).unwrap_or(Value::Null), "f": c.formula.as_ref().map(|f| f.text.clone())}).to_string())
            .unwrap_or_default())
    }
}

impl Window {
    /// Take in what the last remote call altered; answers whether the workbook changed.
    fn take_in(&mut self) -> bool {
        let shown = self.shim.take_shown();
        let changed = !shown.is_empty();
        if changed {
            shown.apply_to(&mut self.wb);
        }
        self.shim.resync(&self.wb);
        changed
    }
}
