//! Sheets' collaboration shim for OneOS (the "Sheets" editor).
//!
//! The host (OneOS) keeps ONE replica of the file's shared document and speaks to the editor frame in the host bridge's
//! operation contract (`plans/mona/craft-integration-design-2026-10-25.md` section 12.3). This crate is the editor's side of it,
//! pure and testable without a browser:
//!
//! * [`Shim::seed_all`] and [`Shim::local_all`] turn the editor's workbook into ops (`new`, `ensure`, `set`, `remove`,
//!   `list.insert`, `list.delete`);
//! * [`Shim::remote_state`] and [`Shim::remote_delta`] turn what the host sends back (`craft:crdt:state`, `craft:crdt:delta`) into
//!   what the editor's workbook must take in ([`Shown`], applied by [`Shown::apply_to`]).
//!
//! ★ WHAT IS SHARED, AND HOW (the schema beneath the doc's `craft` root):
//!
//! | what | where | how |
//! |---|---|---|
//! | sheets | `sheets` (ids, in tab order), `s.<sid>` | a map per sheet: `n` name, `v` visibility, `t` tab colour, `z` frozen panes |
//! | rows, columns | `s.<sid>.rows`, `s.<sid>.cols` | lists of ids: an inserted row is an inserted id, so two people inserting rows merge |
//! | cells | `s.<sid>.cells.<rowId>:<colId>` | a MAP per cell made with `ensure` (two people typing into one empty cell merge): `v` value, `f` formula text, `s` style key |
//! | styles | `styles.<key>` | a style by CONTENT (the key is a hash of it), so windows never share style numbers |
//! | row heights, column widths, hidden | `s.<sid>.rp.<rowId>`, `s.<sid>.cp.<colId>` | one value per line |
//! | merged cells | `s.<sid>.mg.<rowId>:<colId>:<rowId>:<colId>` | a key per merge, by corner ids |
//! | comments | the host's comments tier | a thread per cell, anchored by ids ([`comments`]) |
//! | cursors | the room's awareness | [`Shim::cursor_for`] out, [`Shim::peer_cursor`] in |
//!
//! Not shared live: charts, pictures, conditional formats, validations, tables, pivots, defined names, protection, print setup and
//! a legacy array formula's cells. They are the file's, as opened; a change to them reaches other windows when the file is saved.
//!
//! The frame's own edits are not echoed back by the host, so the shim keeps its view of the shared tree itself: the host's truth
//! (`confirmed`) plus the local batches the host has not yet acknowledged (`pending`), re-applied over every delta (the contract's
//! client-side prediction). A batch counts as acknowledged when a message carries `ack >= seq`, or after [`PENDING_MS`].

pub mod comments;
pub mod mirror;
pub mod snap;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use gridcraft_core::{CellRef, MAX_COLS, RangeRef};
use gridcraft_model::{Cell, CellStore, Color, Comment, Formula, LineInfo, Sheet, Style, StyleId, Visibility, Workbook};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use comments::{CommentMsg, CommentSync, CommentsUpdate};
use mirror::{Mirror, MirrorError};
use snap::{CellRec, Hasher, Plan, Prop, align, hash_cell, hash_str};

/// A local batch is taken as applied by the host this long after it was sent, if no message acknowledged it sooner.
pub const PENDING_MS: u64 = 5_000;
/// At most this many local batches wait for an acknowledgement.
pub const MAX_PENDING: usize = 500;
/// The most ops one batch carries (the host's limit is 2000).
pub const BATCH_OPS: usize = 1_500;
/// The largest JSON value one `set` carries (the host's limit is 64 KiB).
const MAX_VALUE: usize = 60_000;
/// The most rows and columns a sheet shares live (a sheet with data beyond these is edited alone, said by name).
pub const MAX_SHARED_ROWS: usize = 50_000;
pub const MAX_SHARED_COLS: usize = 1_024;
/// The grid of row and column ids a sheet starts with in the room (rows 1 to 200, columns A to Z), whatever its content.
pub const MIN_ROWS: usize = 200;
pub const MIN_COLS: usize = 26;
/// The most cells a workbook shares live.
pub const MAX_SHARED_CELLS: usize = 250_000;

/// Why the shim could not do what was asked. It is loud: the frame says it and falls back to single-editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimError {
    /// A piece of the workbook is bigger than the room holds.
    TooLarge(String),
    /// The shared tree holds something the workbook model does not accept.
    Model(String),
    /// The shared tree and the shim disagree (an op did not apply).
    Mirror(String),
}

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShimError::TooLarge(m) | ShimError::Model(m) | ShimError::Mirror(m) => f.write_str(m),
        }
    }
}

impl From<MirrorError> for ShimError {
    fn from(e: MirrorError) -> Self {
        ShimError::Mirror(e.0)
    }
}

/// One batch of ops for `craft:crdt:ops`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub seq: u64,
    pub ops: Vec<Value>,
    /// The app's own name for the edit (its caption in the room's history).
    pub label: String,
    /// Fold into the previous undo step (a typing burst).
    pub join: bool,
    /// The first batch of a seed (`seed: true`).
    pub seed: bool,
}

/// A change the host reports: set the tree at `path` (below `doc`) to `value`, or remove it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

/// A host message's `changes` array.
#[must_use]
pub fn changes_of(v: &Value) -> Vec<Change> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let path: Vec<String> = c.get("path")?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect();
                    let removed = c.get("removed").and_then(Value::as_bool) == Some(true);
                    Some(Change { path, value: if removed { None } else { c.get("value").cloned() } })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Where another person is, resolved against this window's workbook.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerCursor {
    pub sheet: usize,
    pub anchor: CellRef,
    pub focus: CellRef,
}

fn json_of<T: serde::Serialize>(v: &T) -> Result<Value, ShimError> {
    serde_json::to_value(v).map_err(|e| ShimError::Model(e.to_string()))
}

fn bounded(what: &str, v: Value) -> Result<Value, ShimError> {
    let n = v.to_string().len();
    if n > MAX_VALUE { Err(ShimError::TooLarge(format!("{what} is {n} bytes of data - more than one shared value holds ({MAX_VALUE})"))) } else { Ok(v) }
}

/// The value a map property holds (an object property is `{"$value": v}`).
fn plain(v: &Value) -> &Value {
    v.get("$value").unwrap_or(v)
}

/// Ids: random per window, and never one the tree or this window already uses.
struct Minter {
    rng: u64,
    minted: HashSet<String>,
}

impl Minter {
    fn mint(&mut self, prefix: char, taken: &dyn Fn(&str) -> bool) -> String {
        loop {
            // xorshift64*: ids only need to differ between the windows of one file; the seed is random per window.
            let mut x = self.rng;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.rng = x;
            let id = format!("{prefix}{:012x}", x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 16);
            if !self.minted.contains(&id) && !taken(&id) {
                self.minted.insert(id.clone());
                return id;
            }
        }
    }
}

fn cell_key(row: &str, col: &str) -> String {
    format!("{row}:{col}")
}

fn index_of(ids: &[String]) -> HashMap<&str, u32> {
    ids.iter().enumerate().map(|(i, id)| (id.as_str(), i as u32)).collect()
}

/// One sheet as the shim last synced it with the room (and so with the editor's workbook).
#[derive(Debug, Clone)]
struct SheetSync {
    id: String,
    name: String,
    vis: Visibility,
    tab: Option<Color>,
    freeze: Option<(u32, u32)>,
    rows: Vec<String>,
    cols: Vec<String>,
    cells: HashMap<String, CellRec>,
    rowp: HashMap<String, Prop>,
    colp: HashMap<String, Prop>,
    merges: BTreeSet<String>,
    /// The editor's cells as last synced (cloning a store shares its bands): a local edit diffs against it.
    store: Option<CellStore>,
    /// The editor's sheet as last synced: an unchanged one is skipped without a look.
    arc: Option<Arc<Sheet>>,
}

impl SheetSync {
    fn blank(id: String, name: String) -> Self {
        SheetSync {
            id,
            name,
            vis: Visibility::Visible,
            tab: None,
            freeze: None,
            rows: Vec::new(),
            cols: Vec::new(),
            cells: HashMap::new(),
            rowp: HashMap::new(),
            colp: HashMap::new(),
            merges: BTreeSet::new(),
            store: Some(CellStore::new()),
            arc: None,
        }
    }
}

/// The kinds of change in one local edit, for its caption.
#[derive(Debug, Default, Clone, Copy)]
struct Kinds {
    content: usize,
    format: usize,
    rows_in: bool,
    rows_out: bool,
    cols_in: bool,
    cols_out: bool,
    sheets: bool,
    sizes: bool,
    merges: bool,
}

impl Kinds {
    fn label(&self) -> &'static str {
        let line = self.rows_in || self.rows_out || self.cols_in || self.cols_out;
        if self.rows_in && !self.rows_out && !self.cols_in && !self.cols_out {
            "Insert rows"
        } else if self.rows_out && !self.rows_in && !self.cols_in && !self.cols_out {
            "Delete rows"
        } else if self.cols_in && !self.cols_out && !self.rows_in && !self.rows_out {
            "Insert columns"
        } else if self.cols_out && !self.cols_in && !self.rows_in && !self.rows_out {
            "Delete columns"
        } else if self.sheets && !line && self.content + self.format == 0 && !self.sizes && !self.merges {
            "Sheets"
        } else if self.sizes && !line && self.content + self.format == 0 && !self.sheets && !self.merges {
            "Resize"
        } else if self.merges && !line && self.content + self.format == 0 && !self.sheets && !self.sizes {
            "Merge cells"
        } else if self.format > 0 && self.content == 0 && !line && !self.sheets && !self.sizes && !self.merges {
            "Formatting"
        } else if self.content == 1 && self.format == 0 && !line && !self.sheets && !self.sizes && !self.merges {
            "Edit cell"
        } else {
            "Edit"
        }
    }
}

/// A sheet as the room holds it.
#[derive(Debug, Clone, Default)]
struct RemoteSheet {
    name: String,
    vis: Visibility,
    tab: Option<Color>,
    freeze: Option<(u32, u32)>,
    rows: Vec<String>,
    cols: Vec<String>,
    cells: HashMap<String, CellRec>,
    rowp: HashMap<String, Prop>,
    colp: HashMap<String, Prop>,
    merges: BTreeSet<String>,
}

/// How the lines of a sheet moved: for each old line, the new index it has now (`None`: gone).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Remap {
    pub rows: Vec<Option<u32>>,
    pub cols: Vec<Option<u32>>,
}

/// What the room changed on one sheet, for the editor's sheet to take in.
#[derive(Debug, Clone, Default)]
pub struct SheetShown {
    pub name: Option<String>,
    pub vis: Option<Visibility>,
    pub tab: Option<Option<Color>>,
    pub freeze: Option<Option<(u32, u32)>>,
    /// The sheet's rows or columns moved: the cells and everything that sits on a cell move with them.
    pub remap: Option<Remap>,
    /// The sheet is the room's whole (a join): its cells are cleared first.
    pub replace_cells: bool,
    pub cells_set: Vec<(CellRef, CellRec)>,
    pub cells_removed: Vec<CellRef>,
    /// The room's row heights and hidden rows, by row (replacing the shared fields of every row).
    pub row_props: Option<BTreeMap<u32, Prop>>,
    pub col_props: Option<BTreeMap<u32, Prop>>,
    pub merges: Option<Vec<RangeRef>>,
}

impl SheetShown {
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.vis.is_none()
            && self.tab.is_none()
            && self.freeze.is_none()
            && self.remap.is_none()
            && !self.replace_cells
            && self.cells_set.is_empty()
            && self.cells_removed.is_empty()
            && self.row_props.is_none()
            && self.col_props.is_none()
            && self.merges.is_none()
    }
}

/// What applying a [`Shown`] to a workbook touched, for the editor to recalculate and repaint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Applied {
    /// Sheets or lines moved: recalculate everything.
    pub structure: bool,
    /// Cells set or cleared (sheet index, cell).
    pub cells: Vec<(usize, CellRef)>,
}

/// Everything a remote change altered in the editor's workbook ([`Shim::take_shown`]).
#[derive(Debug, Clone, Default)]
pub struct Shown {
    /// The room's sheets REPLACE the workbook's (a join): each is matched to a sheet of the workbook by name, then by place.
    pub replace_all: bool,
    /// The new order of the sheets: for each, the sheet's index before (`None`: a sheet the room made).
    pub layout: Option<Vec<Option<usize>>>,
    /// Changed sheets, by their index in the new order.
    pub sheets: Vec<(usize, SheetShown)>,
    /// The styles the cells refer to, by key.
    pub styles: HashMap<String, Style>,
    /// Cell comments to show, or (`None`) take away: (sheet index, cell).
    pub comments: Vec<(usize, CellRef, Option<Comment>)>,
}

impl Shown {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.replace_all && self.layout.is_none() && self.sheets.iter().all(|(_, s)| s.is_empty()) && self.comments.is_empty()
    }

    /// Apply to the editor's workbook what the remote changes altered. Nothing here is an edit of this person's, so the shim
    /// already counts it as synced (the caller then hands the workbook back with [`Shim::resync`]).
    pub fn apply_to(&self, wb: &mut Workbook) -> Applied {
        let mut applied = Applied::default();
        if self.replace_all {
            applied.structure = true;
            // The room's sheets, matched to the file's by name and then by place; a sheet the room lacks goes.
            let mut taken = vec![false; wb.sheets.len()];
            let mut order: Vec<Option<usize>> = Vec::new();
            for (_, shown) in &self.sheets {
                let by_name = shown.name.as_deref().and_then(|n| wb.sheets.iter().enumerate().position(|(i, s)| !taken[i] && s.name == n));
                if let Some(i) = by_name {
                    taken[i] = true;
                }
                order.push(by_name);
            }
            let mut free = (0..wb.sheets.len()).filter(|i| !taken[*i]).collect::<Vec<_>>().into_iter();
            for slot in &mut order {
                if slot.is_none() {
                    *slot = free.next();
                }
            }
            relayout(wb, &order, &self.sheets);
        } else if let Some(order) = &self.layout {
            applied.structure = true;
            relayout(wb, order, &self.sheets);
        }
        // The styles the cells refer to are in the workbook's table by content.
        let ids: HashMap<&str, StyleId> = self.styles.iter().map(|(k, s)| (k.as_str(), wb.styles.intern(s.clone()))).collect();
        for (idx, shown) in &self.sheets {
            if shown.is_empty() {
                continue;
            }
            let Some(slot) = wb.sheets.get_mut(*idx) else { continue };
            let sheet = Arc::make_mut(slot);
            if let Some(n) = &shown.name {
                sheet.name = n.clone();
            }
            if let Some(v) = shown.vis {
                sheet.visibility = v;
            }
            if let Some(t) = shown.tab {
                sheet.tab_color = t;
            }
            if let Some(z) = shown.freeze {
                sheet.freeze = z;
            }
            if shown.replace_cells {
                sheet.cells = CellStore::new();
                sheet.comments.clear();
                sheet.hyperlinks.clear();
                sheet.cell_pictures.clear();
            }
            if let Some(remap) = &shown.remap {
                applied.structure = true;
                remap_sheet(sheet, remap);
            }
            for at in &shown.cells_removed {
                sheet.cells.remove(*at);
                applied.cells.push((*idx, *at));
            }
            for (at, rec) in &shown.cells_set {
                if !at.is_valid() {
                    continue;
                }
                let style = rec.s.as_deref().and_then(|k| ids.get(k).copied()).unwrap_or(StyleId::DEFAULT);
                sheet.cells.set(*at, cell_of(rec, style));
                applied.cells.push((*idx, *at));
            }
            if let Some(p) = &shown.row_props {
                overlay(&mut sheet.rows, p);
            }
            if let Some(p) = &shown.col_props {
                overlay(&mut sheet.cols, p);
            }
            if let Some(m) = &shown.merges {
                sheet.merges.clone_from(m);
            }
        }
        for (idx, at, comment) in &self.comments {
            if let Some(slot) = wb.sheets.get_mut(*idx) {
                let sheet = Arc::make_mut(slot);
                match comment {
                    Some(c) => {
                        sheet.comments.insert(*at, c.clone());
                    }
                    None => {
                        sheet.comments.remove(at);
                    }
                }
            }
        }
        applied
    }
}

/// Put the workbook's sheets in the room's order: each slot is the old sheet (`Some`) or a new one the room made.
fn relayout(wb: &mut Workbook, order: &[Option<usize>], shown: &[(usize, SheetShown)]) {
    let old = std::mem::take(&mut wb.sheets);
    let mut next: Vec<Arc<Sheet>> = Vec::with_capacity(order.len());
    for (i, slot) in order.iter().enumerate() {
        match slot.and_then(|j| old.get(j)) {
            Some(s) => next.push(s.clone()),
            None => {
                let name = shown.iter().find(|(k, _)| *k == i).and_then(|(_, s)| s.name.clone()).unwrap_or_else(|| format!("Sheet{}", i + 1));
                next.push(Arc::new(Sheet::new(name)));
            }
        }
    }
    if next.is_empty() {
        next.push(Arc::new(Sheet::new("Sheet1")));
    }
    let to_new: HashMap<usize, usize> = order.iter().enumerate().filter_map(|(i, o)| o.map(|j| (j, i))).collect();
    wb.names.retain(|n| n.scope.is_none_or(|s| to_new.contains_key(&s)));
    for n in &mut wb.names {
        if let Some(s) = n.scope {
            n.scope = to_new.get(&s).copied();
        }
    }
    wb.active_sheet = to_new.get(&wb.active_sheet).copied().unwrap_or(0).min(next.len() - 1);
    wb.sheets = next;
}

/// Move everything on a sheet that sits on a cell or a line to where the room's rows and columns now put it.
fn remap_sheet(sheet: &mut Sheet, remap: &Remap) {
    let r = |i: u32| -> Option<u32> { remap.rows.get(i as usize).copied().unwrap_or(Some(i)) };
    let c = |i: u32| -> Option<u32> { remap.cols.get(i as usize).copied().unwrap_or(Some(i)) };
    let at = |p: CellRef| -> Option<CellRef> { Some(CellRef::new(r(p.row)?, c(p.col)?)) };
    let mut store = CellStore::new();
    for (p, cell) in sheet.cells.iter() {
        if let Some(q) = at(p) {
            store.set(q, cell.clone());
        }
    }
    sheet.cells = store;
    sheet.comments = std::mem::take(&mut sheet.comments).into_iter().filter_map(|(p, v)| at(p).map(|q| (q, v))).collect();
    sheet.hyperlinks = std::mem::take(&mut sheet.hyperlinks).into_iter().filter_map(|(p, v)| at(p).map(|q| (q, v))).collect();
    sheet.cell_pictures = std::mem::take(&mut sheet.cell_pictures).into_iter().filter_map(|(p, v)| at(p).map(|q| (q, v))).collect();
    sheet.rows = std::mem::take(&mut sheet.rows).into_iter().filter_map(|(i, v)| r(i).map(|j| (j, v))).collect();
    sheet.cols = std::mem::take(&mut sheet.cols).into_iter().filter_map(|(i, v)| c(i).map(|j| (j, v))).collect();
    // What a calculation spilled is derived: the recalculation makes it again.
    sheet.spill.clear();
    sheet.spill_ranges.clear();
}

/// The room's row heights and hidden rows replace the shared fields of every row.
fn overlay(lines: &mut BTreeMap<u32, LineInfo>, props: &BTreeMap<u32, Prop>) {
    for l in lines.values_mut() {
        l.size = None;
        l.hidden = false;
        l.custom = false;
    }
    for (i, p) in props {
        let l = lines.entry(*i).or_default();
        l.size = p.size;
        l.hidden = p.hidden;
        l.custom = p.custom;
    }
    lines.retain(|_, l| *l != LineInfo::default());
}

fn cell_of(rec: &CellRec, style: StyleId) -> Cell {
    Cell {
        value: rec.v.as_ref().and_then(|v| serde_json::from_value::<gridcraft_core::Value>(v.clone()).ok()).unwrap_or_default(),
        formula: rec.f.as_ref().map(|t| Arc::new(Formula::new(t))),
        style,
    }
}

/// The index-keyed form of a line map.
fn props_by_index(m: &HashMap<String, Prop>, ids: &[String]) -> BTreeMap<u32, Prop> {
    let at = index_of(ids);
    m.iter().filter_map(|(id, p)| at.get(id.as_str()).map(|i| (*i, *p))).collect()
}

fn line_props(lines: &BTreeMap<u32, LineInfo>) -> BTreeMap<u32, Prop> {
    lines.iter().filter_map(|(i, l)| Prop::of(l).map(|p| (*i, p))).collect()
}

fn row_hashes(store: &CellStore, props: &BTreeMap<u32, Prop>, from: u32, to: u32) -> Vec<u64> {
    if to <= from {
        return Vec::new();
    }
    let mut hs = vec![Hasher::default(); (to - from) as usize];
    for (at, cell) in store.iter_range(RangeRef::new(CellRef::new(from, 0), CellRef::new(to - 1, MAX_COLS - 1))) {
        hash_cell(&mut hs[(at.row - from) as usize], u64::from(at.col), cell);
    }
    for (r, p) in props.range(from..to) {
        hs[(r - from) as usize].bytes(p.json().to_string().as_bytes());
    }
    hs.into_iter().map(Hasher::finish).collect()
}

/// A column's signature over the rows `rows` (by the row ids, so a row inserted elsewhere does not change it).
fn col_hashes(store: &CellStore, props: &BTreeMap<u32, Prop>, from: u32, to: u32, rows: (u32, u32), row_ids: &[String]) -> Vec<u64> {
    if to <= from {
        return Vec::new();
    }
    let mut hs = vec![Hasher::default(); (to - from) as usize];
    for (at, cell) in store.iter_range(RangeRef::new(CellRef::new(rows.0, from), CellRef::new(rows.1, to - 1))) {
        let along = row_ids.get(at.row as usize).map_or(u64::from(at.row), |id| hash_str(id));
        hash_cell(&mut hs[(at.col - from) as usize], along, cell);
    }
    for (c, p) in props.range(from..to) {
        hs[(c - from) as usize].bytes(p.json().to_string().as_bytes());
    }
    hs.into_iter().map(Hasher::finish).collect()
}

fn sha_key(v: &Value) -> String {
    let mut h = Sha256::new();
    h.update(v.to_string().as_bytes());
    let out = h.finalize();
    out.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The editor's workbook as one comparable value: what the shim shares, by place and with styles by content (the same in every
/// window that has converged). The tests compare it between windows.
#[must_use]
pub fn digest(wb: &Workbook) -> Value {
    let sheets: Vec<Value> = wb
        .sheets
        .iter()
        .map(|s| {
            let mut cells = serde_json::Map::new();
            for (at, cell) in s.cells.iter() {
                if cell.formula.as_ref().is_some_and(|f| f.array.is_some()) {
                    continue;
                }
                let style = if cell.style == StyleId::DEFAULT { Value::Null } else { serde_json::to_value(wb.styles.get(cell.style)).unwrap_or(Value::Null) };
                let v = if cell.formula.is_none() && !cell.value.is_empty() { serde_json::to_value(&cell.value).unwrap_or(Value::Null) } else { Value::Null };
                let f = cell.formula.as_ref().map(|f| f.text.clone());
                if v.is_null() && f.is_none() && style.is_null() {
                    continue;
                }
                cells.insert(at.a1(), json!({"v": v, "f": f, "s": style}));
            }
            let props = |m: &BTreeMap<u32, LineInfo>| -> Value { Value::Object(m.iter().filter_map(|(i, l)| Prop::of(l).map(|p| (i.to_string(), p.json()))).collect()) };
            let mut merges: Vec<String> = s.merges.iter().map(|m| format!("{}:{}", m.start.a1(), m.end.a1())).collect();
            merges.sort();
            let mut comments = serde_json::Map::new();
            for (at, c) in &s.comments {
                comments.insert(at.a1(), json!({"text": c.text, "resolved": c.resolved, "replies": c.replies.iter().map(|(_, t)| t).collect::<Vec<_>>()}));
            }
            json!({
                "name": s.name, "vis": s.visibility, "tab": s.tab_color, "freeze": s.freeze,
                "cells": cells, "rows": props(&s.rows), "cols": props(&s.cols), "merges": merges, "comments": comments,
            })
        })
        .collect();
    json!({"sheets": sheets})
}

pub struct Shim {
    minter: Minter,
    sheets: Vec<SheetSync>,
    /// The styles the room holds, by key (as parsed and written again).
    styles: HashMap<String, Value>,
    /// The key of each of the editor's style ids.
    style_keys: HashMap<u32, String>,
    /// Styles to put in the room before the cells that use them.
    style_ops: Vec<Value>,
    /// The shared tree as this window shows it (the host's truth plus the batches not yet acknowledged).
    view: Mirror,
    /// The host's truth, kept only while batches are pending.
    confirmed: Option<Mirror>,
    pending: Vec<(u64, u64, Vec<Value>)>,
    seq: u64,
    comments: CommentSync,
    comment_msgs: Vec<CommentMsg>,
    host_comments: Value,
    shown: Shown,
}

fn list_of(node: Option<&Value>) -> Vec<String> {
    node.and_then(|n| n.get("$list")).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

fn map_of(node: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    node.and_then(Value::as_object)
}

impl Shim {
    /// `seed`: random bits for ids (the caller draws them from the platform's randomness).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Shim {
            minter: Minter { rng: seed | 1, minted: HashSet::new() },
            sheets: Vec::new(),
            styles: HashMap::new(),
            style_keys: HashMap::new(),
            style_ops: Vec::new(),
            view: Mirror::default(),
            confirmed: None,
            pending: Vec::new(),
            seq: 0,
            comments: CommentSync::default(),
            comment_msgs: Vec::new(),
            host_comments: Value::Null,
            shown: Shown::default(),
        }
    }

    /// The shared tree as this window shows it.
    #[must_use]
    pub fn view(&self) -> &Value {
        &self.view.root
    }

    fn push(&mut self, ops: Vec<Value>, label: &str, join: bool, seed: bool, now_ms: u64) -> Result<Batch, ShimError> {
        if self.pending.is_empty() {
            self.confirmed = Some(self.view.clone());
        }
        for op in &ops {
            self.view.apply(op)?;
        }
        self.seq += 1;
        self.pending.push((self.seq, now_ms, ops.clone()));
        self.prune(now_ms)?;
        Ok(Batch { seq: self.seq, ops, label: label.to_owned(), join, seed })
    }

    /// Send `ops` as one or more batches (the host takes at most 2000 ops a batch; every prefix of the ops leaves a valid tree).
    fn emit(&mut self, ops: Vec<Value>, label: &str, join: bool, seed: bool, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        let mut rest = ops;
        let mut first = true;
        while !rest.is_empty() {
            let take: Vec<Value> = rest.drain(..BATCH_OPS.min(rest.len())).collect();
            let (name, joined) = if first { (label.to_owned(), join) } else { (format!("{label} (more)"), false) };
            out.push(self.push(take, &name, joined, seed && first, now_ms)?);
            first = false;
        }
        Ok(())
    }

    /// Fold old and surplus pending batches into the host's truth (they were applied).
    fn prune(&mut self, now_ms: u64) -> Result<(), ShimError> {
        while let Some((_, sent, _)) = self.pending.first() {
            if self.pending.len() <= MAX_PENDING && now_ms.saturating_sub(*sent) < PENDING_MS {
                break;
            }
            let (_, _, ops) = self.pending.remove(0);
            if let Some(c) = self.confirmed.as_mut() {
                for op in &ops {
                    // An op that no longer applies was refused by the host too; the next state message resets us.
                    let _ = c.apply(op);
                }
            }
        }
        if self.pending.is_empty() {
            self.confirmed = None;
        }
        Ok(())
    }

    // ── styles ────────────────────────────────────────────────────────────────────────────────────────────────────

    /// The key of the editor's style `id`; a style the room does not hold yet is queued to be put there first.
    fn style_key(&mut self, wb: &Workbook, id: StyleId) -> Result<String, ShimError> {
        if let Some(k) = self.style_keys.get(&id.0) {
            return Ok(k.clone());
        }
        let j = bounded("a cell style", json_of(wb.styles.get(id))?)?;
        let key = sha_key(&j);
        self.style_keys.insert(id.0, key.clone());
        if !self.styles.contains_key(&key) {
            self.styles.insert(key.clone(), j.clone());
            self.style_ops.push(json!({"op": "set", "path": ["styles"], "key": key, "value": j}));
        }
        Ok(key)
    }

    fn rec_of(&mut self, wb: &Workbook, cell: &Cell) -> Result<Option<CellRec>, ShimError> {
        // A legacy array formula spans cells the room has no way to hold: it stays the file's.
        if cell.formula.as_ref().is_some_and(|f| f.array.is_some()) {
            return Ok(None);
        }
        let v = if cell.formula.is_none() && !cell.value.is_empty() { Some(bounded("a cell value", json_of(&cell.value)?)?) } else { None };
        let f = match &cell.formula {
            Some(f) if f.text.len() > MAX_VALUE => return Err(ShimError::TooLarge(format!("a formula of {} bytes is more than one shared value holds", f.text.len()))),
            Some(f) => Some(f.text.clone()),
            None => None,
        };
        let s = if cell.style == StyleId::DEFAULT { None } else { Some(self.style_key(wb, cell.style)?) };
        if v.is_none() && f.is_none() && s.is_none() {
            return Ok(None);
        }
        Ok(Some(CellRec { v, f, s }))
    }

    // ── seed ──────────────────────────────────────────────────────────────────────────────────────────────────────

    /// ★ SEED: the whole workbook as ops. The first batch is the seed (`seed: true`, one untracked change on the host); the rest
    /// follow as ordinary batches, each leaving the tree complete up to its last op.
    pub fn seed_all(&mut self, wb: &Workbook, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.sheets.clear();
        self.styles.clear();
        self.style_keys.clear();
        self.style_ops.clear();
        self.comments = CommentSync::default();
        self.comment_msgs.clear();
        self.shown = Shown::default();
        self.view = Mirror::default();
        self.confirmed = None;
        self.pending.clear();
        self.minter.minted.clear();
        let mut ops = Vec::new();
        let mut kinds = Kinds::default();
        self.diff_workbook(wb, &mut ops, &mut kinds)?;
        let mut roots = vec![
            json!({"op": "ensure", "path": [], "key": "sheets", "kind": "list"}),
            json!({"op": "ensure", "path": [], "key": "s", "kind": "map"}),
            json!({"op": "ensure", "path": [], "key": "styles", "kind": "map"}),
        ];
        roots.append(&mut ops);
        let mut batches = Vec::new();
        self.emit(roots, "Opened the file", false, true, now_ms, &mut batches)?;
        self.comments_local(wb);
        Ok(batches)
    }

    // ── local edits ───────────────────────────────────────────────────────────────────────────────────────────────

    /// ★ A LOCAL EDIT: the ops that take what the peers hold (the last synced workbook) to `wb`, or nothing when it is the same.
    /// `label` names the edit when the caller knows better than the shim does (an assistant run).
    pub fn local_all(&mut self, wb: &Workbook, label: Option<&str>, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        let mut ops = Vec::new();
        let mut kinds = Kinds::default();
        self.minter.minted.clear();
        // A room seeded before a root existed lacks it: made now (`ensure` merges two people doing it at once).
        let mut roots = Vec::new();
        for (key, kind) in [("sheets", "list"), ("s", "map"), ("styles", "map")] {
            if self.view.get(&[key]).is_none() {
                roots.push(json!({"op": "ensure", "path": [], "key": key, "kind": kind}));
            }
        }
        self.diff_workbook(wb, &mut ops, &mut kinds)?;
        roots.append(&mut ops);
        let mut batches = Vec::new();
        if !roots.is_empty() {
            let name = label.map_or_else(|| kinds.label().to_owned(), str::to_owned);
            self.emit(roots, &name, false, false, now_ms, &mut batches)?;
        }
        self.comments_local(wb);
        Ok(batches)
    }

    /// After the editor's workbook took in a remote change: its stores are what the shim now holds, so a later local diff starts there.
    pub fn resync(&mut self, wb: &Workbook) {
        for (ss, sheet) in self.sheets.iter_mut().zip(&wb.sheets) {
            ss.store = Some(sheet.cells.clone());
            ss.arc = Some(sheet.clone());
        }
    }

    fn diff_workbook(&mut self, wb: &Workbook, ops: &mut Vec<Value>, kinds: &mut Kinds) -> Result<(), ShimError> {
        let total: usize = wb.sheets.iter().map(|s| s.cells.len()).sum();
        if total > MAX_SHARED_CELLS {
            return Err(ShimError::TooLarge(format!("the workbook holds {total} cells - live editing shares up to {MAX_SHARED_CELLS}")));
        }
        let n_new = wb.sheets.len();
        let n_old = self.sheets.len();
        // Which sheet is which: by name, then by place (a renamed sheet keeps its id).
        let mut from_old: Vec<Option<usize>> = vec![None; n_new];
        let mut used = vec![false; n_old];
        for (i, s) in wb.sheets.iter().enumerate() {
            if let Some(j) = (0..n_old).find(|j| !used[*j] && self.sheets[*j].name == s.name) {
                from_old[i] = Some(j);
                used[j] = true;
            }
        }
        let mut free = (0..n_old).filter(|j| !used[*j]).collect::<Vec<_>>().into_iter();
        for slot in from_old.iter_mut() {
            if slot.is_none()
                && let Some(j) = free.next()
            {
                *slot = Some(j);
                used[j] = true;
            }
        }
        let mut olds: Vec<Option<SheetSync>> = std::mem::take(&mut self.sheets).into_iter().map(Some).collect();
        let mut ids: Vec<String> = olds.iter().flatten().map(|s| s.id.clone()).collect();
        // Sheets that are gone, from the last to the first.
        for j in (0..n_old).rev() {
            if !used[j] {
                if let Some(s) = olds[j].take() {
                    ops.push(json!({"op": "list.delete", "path": ["sheets"], "at": j, "len": 1}));
                    ops.push(json!({"op": "remove", "path": ["s"], "key": s.id}));
                    ids.remove(ids.iter().position(|i| *i == s.id).unwrap_or(0));
                    kinds.sheets = true;
                }
            }
        }
        let mut next: Vec<SheetSync> = Vec::with_capacity(n_new);
        let mut created: Vec<bool> = Vec::with_capacity(n_new);
        for (i, sheet) in wb.sheets.iter().enumerate() {
            match from_old[i].and_then(|j| olds[j].take()) {
                Some(s) => {
                    next.push(s);
                    created.push(false);
                }
                None => {
                    let taken = |id: &str| ids.iter().any(|x| x == id);
                    let id = self.minter.mint('s', &taken);
                    // Made empty and named by the diff below.
                    next.push(SheetSync::blank(id, String::new()));
                    created.push(true);
                    let _ = sheet;
                }
            }
        }
        // The order of the sheets: delete and insert ids until the list is the new one.
        let target: Vec<String> = next.iter().map(|s| s.id.clone()).collect();
        let mut cur = ids;
        for (i, id) in target.iter().enumerate() {
            if cur.get(i) == Some(id) {
                continue;
            }
            if let Some(pos) = cur.iter().position(|x| x == id) {
                ops.push(json!({"op": "list.delete", "path": ["sheets"], "at": pos, "len": 1}));
                cur.remove(pos);
                kinds.sheets = true;
            }
            ops.push(json!({"op": "list.insert", "path": ["sheets"], "at": i, "value": id}));
            cur.insert(i, id.clone());
            kinds.sheets |= created[i];
        }
        for (i, sheet) in wb.sheets.iter().enumerate() {
            let mut ss = std::mem::replace(&mut next[i], SheetSync::blank(String::new(), String::new()));
            if created[i] {
                let sid = ss.id.clone();
                ops.push(json!({"op": "new", "path": ["s"], "key": sid, "kind": "map"}));
                for (key, kind) in [("rows", "list"), ("cols", "list"), ("cells", "map"), ("rp", "map"), ("cp", "map"), ("mg", "map")] {
                    ops.push(json!({"op": "new", "path": ["s", &sid], "key": key, "kind": kind}));
                }
                ops.push(json!({"op": "set", "path": ["s", &sid], "key": "n", "value": sheet.name}));
                ss.name = sheet.name.clone();
            }
            self.local_sheet(wb, &mut ss, sheet, ops, kinds)?;
            next[i] = ss;
        }
        self.sheets = next;
        // The styles the cells now use go first.
        if !self.style_ops.is_empty() {
            let styles = std::mem::take(&mut self.style_ops);
            ops.splice(0..0, styles);
        }
        Ok(())
    }

    fn local_sheet(&mut self, wb: &Workbook, ss: &mut SheetSync, sheet: &Arc<Sheet>, ops: &mut Vec<Value>, kinds: &mut Kinds) -> Result<(), ShimError> {
        if ss.arc.as_ref().is_some_and(|a| Arc::ptr_eq(a, sheet)) {
            return Ok(());
        }
        let sid = ss.id.clone();
        let base = |extra: &str| -> Vec<String> { vec!["s".to_owned(), sid.clone(), extra.to_owned()] };
        // The sheet's own properties.
        if ss.name != sheet.name {
            ops.push(json!({"op": "set", "path": ["s", &sid], "key": "n", "value": sheet.name}));
            ss.name = sheet.name.clone();
            kinds.sheets = true;
        }
        if ss.vis != sheet.visibility {
            ops.push(json!({"op": "set", "path": ["s", &sid], "key": "v", "value": json_of(&sheet.visibility)?}));
            ss.vis = sheet.visibility;
            kinds.sheets = true;
        }
        if ss.tab != sheet.tab_color {
            match sheet.tab_color {
                Some(c) => ops.push(json!({"op": "set", "path": ["s", &sid], "key": "t", "value": json_of(&c)?})),
                None => ops.push(json!({"op": "remove", "path": ["s", &sid], "key": "t"})),
            }
            ss.tab = sheet.tab_color;
            kinds.sheets = true;
        }
        if ss.freeze != sheet.freeze {
            match sheet.freeze {
                Some((r, c)) => ops.push(json!({"op": "set", "path": ["s", &sid], "key": "z", "value": {"r": r, "c": c}})),
                None => ops.push(json!({"op": "remove", "path": ["s", &sid], "key": "z"})),
            }
            ss.freeze = sheet.freeze;
            kinds.sheets = true;
        }
        // What changed in the cells and the line properties since the last sync.
        let old_store = ss.store.take().unwrap_or_default();
        let changed = sheet.cells.diff(&old_store);
        let new_rp = line_props(&sheet.rows);
        let new_cp = line_props(&sheet.cols);
        let old_rp = props_by_index(&ss.rowp, &ss.rows);
        let old_cp = props_by_index(&ss.colp, &ss.cols);
        let diff_keys = |a: &BTreeMap<u32, Prop>, b: &BTreeMap<u32, Prop>| -> Vec<u32> {
            a.keys().chain(b.keys()).filter(|k| a.get(k) != b.get(k)).copied().collect()
        };
        let rp_diff = diff_keys(&new_rp, &old_rp);
        let cp_diff = diff_keys(&new_cp, &old_cp);
        let changed_rows: BTreeSet<u32> = changed.iter().map(|c| c.row).chain(rp_diff.iter().copied()).collect();
        let changed_cols: BTreeSet<u32> = changed.iter().map(|c| c.col).chain(cp_diff.iter().copied()).collect();
        // How many rows and columns the sheet now has: its content, and never fewer than the room holds.
        let mut extent_r = sheet.cells.used_range().map_or(0, |r| r.end.row as usize + 1);
        let mut extent_c = sheet.cells.used_range().map_or(0, |r| r.end.col as usize + 1);
        if let Some(k) = new_rp.keys().next_back() {
            extent_r = extent_r.max(*k as usize + 1);
        }
        if let Some(k) = new_cp.keys().next_back() {
            extent_c = extent_c.max(*k as usize + 1);
        }
        for m in &sheet.merges {
            extent_r = extent_r.max(m.end.row as usize + 1);
            extent_c = extent_c.max(m.end.col as usize + 1);
        }
        for at in sheet.comments.keys() {
            extent_r = extent_r.max(at.row as usize + 1);
            extent_c = extent_c.max(at.col as usize + 1);
        }
        // A sheet the room first hears of gets a grid of ids to type in (the room's lists are what two people typing into the same empty
        // cell at once share; a cell beyond them gets ids of its own - two people extending the sheet at one instant each add a line).
        if ss.rows.is_empty() && ss.cols.is_empty() {
            extent_r = extent_r.max(MIN_ROWS);
            extent_c = extent_c.max(MIN_COLS);
        }
        if extent_r > MAX_SHARED_ROWS || extent_c > MAX_SHARED_COLS {
            return Err(ShimError::TooLarge(format!(
                "the sheet '{}' reaches row {extent_r} and column {extent_c} - live editing shares up to {MAX_SHARED_ROWS} rows and {MAX_SHARED_COLS} columns",
                sheet.name
            )));
        }
        let (old_r, old_c) = (ss.rows.len(), ss.cols.len());
        let (new_r, new_c) = (extent_r.max(old_r), extent_c.max(old_c));
        // ★ Rows and columns: a person who inserted or deleted lines changed many rows; a few cells are an edit in place.
        // A line with nothing in it is padding: losing one is not a "delete" of anything the person had.
        let blank = Hasher::default().finish();
        let mut rows_deleted_content = false;
        let rows_plan = if changed_rows.len() > 2 && !changed_rows.is_empty() {
            let r0 = (*changed_rows.iter().next().unwrap_or(&0) as usize).min(old_r);
            let old_h = row_hashes(&old_store, &old_rp, r0 as u32, old_r as u32);
            let new_h = row_hashes(&sheet.cells, &new_rp, r0 as u32, new_r as u32);
            let p = align(&old_h, &new_h);
            rows_deleted_content = p.deleted.iter().any(|j| old_h[*j] != blank);
            // Back to whole-list indices.
            Plan {
                from_old: (0..new_r).map(|i| if i < r0 { Some(i) } else { p.from_old[i - r0].map(|j| j + r0) }).collect(),
                deleted: p.deleted.iter().map(|j| j + r0).collect(),
            }
        } else {
            Plan { from_old: (0..new_r).map(|i| (i < old_r).then_some(i)).collect(), deleted: Vec::new() }
        };
        let row_ids = self.line_ids(&sid, "rows", 'r', &ss.rows, &rows_plan, ops);
        if rows_deleted_content {
            kinds.rows_out = true;
        }
        if rows_plan.inserted_inside() {
            kinds.rows_in = true;
        }
        let mut cols_deleted_content = false;
        let cols_plan = if changed_cols.len() > 2 {
            let c0 = (*changed_cols.iter().next().unwrap_or(&0) as usize).min(old_c);
            let rows_span = (*changed_rows.iter().next().unwrap_or(&0), *changed_rows.iter().next_back().unwrap_or(&0));
            let old_ids = ss.rows.clone();
            let old_h = col_hashes(&old_store, &old_cp, c0 as u32, old_c as u32, rows_span, &old_ids);
            let new_h = col_hashes(&sheet.cells, &new_cp, c0 as u32, new_c as u32, rows_span, &row_ids);
            let p = align(&old_h, &new_h);
            cols_deleted_content = p.deleted.iter().any(|j| old_h[*j] != blank);
            Plan {
                from_old: (0..new_c).map(|i| if i < c0 { Some(i) } else { p.from_old[i - c0].map(|j| j + c0) }).collect(),
                deleted: p.deleted.iter().map(|j| j + c0).collect(),
            }
        } else {
            Plan { from_old: (0..new_c).map(|i| (i < old_c).then_some(i)).collect(), deleted: Vec::new() }
        };
        let col_ids = self.line_ids(&sid, "cols", 'c', &ss.cols, &cols_plan, ops);
        if cols_deleted_content {
            kinds.cols_out = true;
        }
        let structural = !rows_plan.is_stable(old_r) || !cols_plan.is_stable(old_c);
        if cols_plan.inserted_inside() {
            kinds.cols_in = true;
        }
        // ★ Cells.
        let cells_path = base("cells");
        if structural {
            let mut new_cells: HashMap<String, CellRec> = HashMap::new();
            for (at, cell) in sheet.cells.iter() {
                if let Some(rec) = self.rec_of(wb, cell)?
                    && let (Some(r), Some(c)) = (row_ids.get(at.row as usize), col_ids.get(at.col as usize))
                {
                    new_cells.insert(cell_key(r, c), rec);
                }
            }
            for (key, rec) in &new_cells {
                cell_ops(&cells_path, key, Some(rec), ss.cells.get(key), ops, kinds);
            }
            for key in ss.cells.keys() {
                if !new_cells.contains_key(key) {
                    cell_ops(&cells_path, key, None, ss.cells.get(key), ops, kinds);
                }
            }
            ss.cells = new_cells;
        } else {
            for at in &changed {
                let (Some(r), Some(c)) = (row_ids.get(at.row as usize), col_ids.get(at.col as usize)) else { continue };
                let key = cell_key(r, c);
                let new = match sheet.cells.get(*at) {
                    Some(cell) => self.rec_of(wb, cell)?,
                    None => None,
                };
                cell_ops(&cells_path, &key, new.as_ref(), ss.cells.get(&key), ops, kinds);
                match new {
                    Some(rec) => {
                        ss.cells.insert(key, rec);
                    }
                    None => {
                        ss.cells.remove(&key);
                    }
                }
            }
        }
        ss.rows = row_ids;
        ss.cols = col_ids;
        // Line properties, by id.
        for (list, new, old, ids) in [("rp", &new_rp, &mut ss.rowp, &ss.rows), ("cp", &new_cp, &mut ss.colp, &ss.cols)] {
            let want: HashMap<String, Prop> = new.iter().filter_map(|(i, p)| ids.get(*i as usize).map(|id| (id.clone(), *p))).collect();
            let path = base(list);
            for (id, p) in &want {
                if old.get(id) != Some(p) {
                    ops.push(json!({"op": "set", "path": path, "key": id, "value": p.json()}));
                    kinds.sizes = true;
                }
            }
            for id in old.keys() {
                if !want.contains_key(id) {
                    ops.push(json!({"op": "remove", "path": path, "key": id}));
                    kinds.sizes = true;
                }
            }
            *old = want;
        }
        // Merged cells, by the ids of their corners.
        let merges: BTreeSet<String> = sheet
            .merges
            .iter()
            .filter_map(|m| {
                Some(format!("{}:{}", cell_key(ss.rows.get(m.start.row as usize)?, ss.cols.get(m.start.col as usize)?), cell_key(ss.rows.get(m.end.row as usize)?, ss.cols.get(m.end.col as usize)?)))
            })
            .collect();
        let path = base("mg");
        for m in merges.difference(&ss.merges) {
            ops.push(json!({"op": "set", "path": path, "key": m, "value": true}));
            kinds.merges = true;
        }
        for m in ss.merges.difference(&merges) {
            ops.push(json!({"op": "remove", "path": path, "key": m}));
            kinds.merges = true;
        }
        ss.merges = merges;
        ss.store = Some(sheet.cells.clone());
        ss.arc = Some(sheet.clone());
        Ok(())
    }

    /// The ids of a sheet's lines after `plan`, and the ops that take the room's list there.
    fn line_ids(&mut self, sid: &str, list: &str, prefix: char, old: &[String], plan: &Plan, ops: &mut Vec<Value>) -> Vec<String> {
        let path = ["s", sid, list];
        // Gone lines, the last run first so the earlier indices stay true.
        let del = &plan.deleted;
        let mut i = del.len();
        while i > 0 {
            let mut j = i - 1;
            while j > 0 && del[j - 1] + 1 == del[j] {
                j -= 1;
            }
            ops.push(json!({"op": "list.delete", "path": path, "at": del[j], "len": i - j}));
            i = j;
        }
        let mut ids: Vec<String> = Vec::with_capacity(plan.from_old.len());
        for (at, from) in plan.from_old.iter().enumerate() {
            match from.and_then(|j| old.get(j)) {
                Some(id) => ids.push(id.clone()),
                None => {
                    let id = self.minter.mint(prefix, &|c: &str| old.iter().any(|x| x == c) || ids.iter().any(|x| x == c));
                    ops.push(json!({"op": "list.insert", "path": path, "at": at, "value": id}));
                    ids.push(id);
                }
            }
        }
        ids
    }

    // ── comments ──────────────────────────────────────────────────────────────────────────────────────────────────

    fn comments_local(&mut self, wb: &Workbook) {
        let mut threads: Vec<(String, Value, &Comment)> = Vec::new();
        for (ss, sheet) in self.sheets.iter().zip(&wb.sheets) {
            for (at, c) in &sheet.comments {
                let (Some(r), Some(col)) = (ss.rows.get(at.row as usize), ss.cols.get(at.col as usize)) else { continue };
                let cell = cell_key(r, col);
                threads.push((comments::key_of(&ss.id, &cell), json!({"sh": ss.id, "cell": cell}), c));
            }
        }
        let msgs = self.comments.local(&threads);
        self.comment_msgs.extend(msgs);
    }

    /// The comment messages the last local calls produced. Send each as `craft:crdt:comment`.
    pub fn take_comment_msgs(&mut self) -> Vec<CommentMsg> {
        std::mem::take(&mut self.comment_msgs)
    }

    /// The host's comments arrived (`craft:crdt:state`'s `comments`, or a delta at `["comments"]`).
    pub fn remote_comments(&mut self, host: &Value) {
        self.host_comments = host.clone();
        self.apply_host_comments();
    }

    /// A host delta's changes: those below `["comments"]` change the host's comments (the whole value, or one entry or field of it).
    pub fn remote_comment_changes(&mut self, changes: &[Change]) {
        let mut touched = false;
        for c in changes {
            if c.path.first().map(String::as_str) != Some("comments") {
                continue;
            }
            touched = true;
            if c.path.len() == 1 {
                self.host_comments = c.value.clone().unwrap_or(Value::Null);
            } else if let Some(v) = &c.value {
                set_at(&mut self.host_comments, &c.path[1..], v.clone());
            } else {
                remove_at(&mut self.host_comments, &c.path[1..]);
            }
        }
        if touched {
            self.apply_host_comments();
        }
    }

    fn apply_host_comments(&mut self) {
        let (update, deletes) = self.comments.remote(&self.host_comments);
        self.comment_msgs.extend(deletes);
        self.show_comments(update);
    }

    fn show_comments(&mut self, update: CommentsUpdate) {
        let place = |key: &str| -> Option<(usize, CellRef)> {
            let (sid, cell) = key.split_once('|')?;
            let (r, c) = cell.split_once(':')?;
            let si = self.sheets.iter().position(|s| s.id == sid)?;
            let ss = &self.sheets[si];
            let row = ss.rows.iter().position(|x| x == r)?;
            let col = ss.cols.iter().position(|x| x == c)?;
            Some((si, CellRef::new(row as u32, col as u32)))
        };
        let mut out: Vec<(usize, CellRef, Option<Comment>)> = Vec::new();
        for c in &update.upsert {
            if let Some((si, at)) = place(&c.key) {
                out.push((si, at, Some(c.to_comment())));
            }
        }
        for key in &update.remove {
            if let Some((si, at)) = place(key) {
                out.push((si, at, None));
            }
        }
        self.shown.comments.extend(out);
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────

    /// The body of a `craft:crdt:cursor` message for a selection on sheet `sheet`: the sheet and the cells at its two ends, by id (a
    /// place beyond the rows and columns the room holds is named by its number, `~7`). `None` for a sheet the room does not hold.
    #[must_use]
    pub fn cursor_for(&self, sheet: usize, anchor: CellRef, focus: CellRef) -> Option<Value> {
        let ss = self.sheets.get(sheet)?;
        let line = |ids: &[String], i: u32| ids.get(i as usize).cloned().unwrap_or_else(|| format!("~{i}"));
        let end = |p: CellRef| cell_key(&line(&ss.rows, p.row), &line(&ss.cols, p.col));
        let mut body = json!({"sh": ss.id, "f": end(focus)});
        if anchor != focus {
            body["a"] = Value::from(end(anchor));
        }
        Some(body)
    }

    /// Another person's cursor body, resolved against this window's workbook.
    #[must_use]
    pub fn peer_cursor(&self, cursor: &Value) -> Option<PeerCursor> {
        let sid = cursor.get("sh")?.as_str()?;
        let si = self.sheets.iter().position(|s| s.id == sid)?;
        let ss = &self.sheets[si];
        let line = |ids: &[String], part: &str| -> Option<u32> {
            match part.strip_prefix('~') {
                Some(n) => n.parse().ok(),
                None => ids.iter().position(|x| x == part).map(|i| i as u32),
            }
        };
        let place = |v: &Value| -> Option<CellRef> {
            let (r, c) = v.as_str()?.split_once(':')?;
            Some(CellRef::new(line(&ss.rows, r)?, line(&ss.cols, c)?))
        };
        let focus = place(cursor.get("f")?)?;
        let anchor = cursor.get("a").and_then(place).unwrap_or(focus);
        Some(PeerCursor { sheet: si, anchor, focus })
    }

    // ── remote changes ────────────────────────────────────────────────────────────────────────────────────────────

    /// Read a sheet of the shared tree.
    fn read_sheet(&self, sid: &str) -> Result<Option<RemoteSheet>, ShimError> {
        let Some(node) = self.view.get(&["s", sid]).and_then(Value::as_object) else { return Ok(None) };
        let mut r = RemoteSheet {
            name: node.get("n").map(plain).and_then(Value::as_str).unwrap_or("Sheet").to_owned(),
            ..RemoteSheet::default()
        };
        if let Some(v) = node.get("v").map(plain) {
            r.vis = serde_json::from_value(v.clone()).unwrap_or_default();
        }
        r.tab = node.get("t").map(plain).and_then(|v| serde_json::from_value(v.clone()).ok());
        r.freeze = node.get("z").map(plain).and_then(|v| Some((u32::try_from(v.get("r")?.as_u64()?).ok()?, u32::try_from(v.get("c")?.as_u64()?).ok()?)));
        r.rows = list_of(node.get("rows"));
        r.cols = list_of(node.get("cols"));
        let rows: HashSet<&str> = r.rows.iter().map(String::as_str).collect();
        let cols: HashSet<&str> = r.cols.iter().map(String::as_str).collect();
        if let Some(cells) = map_of(node.get("cells")) {
            for (key, c) in cells {
                let Some((rid, cid)) = key.split_once(':') else { continue };
                if !rows.contains(rid) || !cols.contains(cid) {
                    continue;
                }
                let Some(c) = c.as_object() else { continue };
                let v = match c.get("v").map(plain) {
                    // Parsed and written again, so a number compares in the model's own form (20 and 20.0 are one).
                    Some(raw) => match serde_json::from_value::<gridcraft_core::Value>(raw.clone()) {
                        Ok(v) => Some(json_of(&v)?),
                        Err(e) => return Err(ShimError::Model(format!("cell {key}: {e}"))),
                    },
                    None => None,
                };
                let f = c.get("f").map(plain).and_then(Value::as_str).map(str::to_owned);
                let s = c.get("s").map(plain).and_then(Value::as_str).map(str::to_owned);
                if v.is_some() || f.is_some() || s.is_some() {
                    r.cells.insert(key.clone(), CellRec { v, f, s });
                }
            }
        }
        for (list, into) in [("rp", &mut r.rowp), ("cp", &mut r.colp)] {
            if let Some(m) = map_of(node.get(list)) {
                for (id, p) in m {
                    if let Some(p) = Prop::from_json(plain(p)) {
                        into.insert(id.clone(), p);
                    }
                }
            }
        }
        if let Some(m) = map_of(node.get("mg")) {
            for (key, v) in m {
                if plain(v).as_bool() == Some(true) {
                    r.merges.insert(key.clone());
                }
            }
        }
        Ok(Some(r))
    }

    /// Read the room's styles into the shim's table, and answer those a cell of `used` refers to.
    fn read_styles(&mut self, used: &HashSet<String>) -> Result<HashMap<String, Style>, ShimError> {
        let mut out = HashMap::new();
        if let Some(map) = self.view.get(&["styles"]).and_then(Value::as_object) {
            let mut known = HashMap::new();
            for (key, v) in map {
                let style = serde_json::from_value::<Style>(plain(v).clone()).map_err(|e| ShimError::Model(format!("style {key}: {e}")))?;
                known.insert(key.clone(), json_of(&style)?);
                if used.contains(key) {
                    out.insert(key.clone(), style);
                }
            }
            self.styles = known;
        }
        Ok(out)
    }

    /// Compare the room's sheet with what was synced and answer what the editor must take in; the sync becomes the room's.
    fn remote_diff(ss: &mut SheetSync, rs: RemoteSheet, replace: bool) -> SheetShown {
        let mut sh = SheetShown::default();
        if replace || ss.name != rs.name {
            sh.name = Some(rs.name.clone());
        }
        if replace || ss.vis != rs.vis {
            sh.vis = Some(rs.vis);
        }
        if replace || ss.tab != rs.tab {
            sh.tab = Some(rs.tab);
        }
        if replace || ss.freeze != rs.freeze {
            sh.freeze = Some(rs.freeze);
        }
        let lines_moved = !replace && (ss.rows != rs.rows || ss.cols != rs.cols);
        let new_rows = index_of(&rs.rows);
        let new_cols = index_of(&rs.cols);
        if lines_moved {
            let old_rows: Vec<Option<u32>> = ss.rows.iter().map(|id| new_rows.get(id.as_str()).copied()).collect();
            let old_cols: Vec<Option<u32>> = ss.cols.iter().map(|id| new_cols.get(id.as_str()).copied()).collect();
            sh.remap = Some(Remap { rows: old_rows, cols: old_cols });
        }
        sh.replace_cells = replace;
        let at_of = |key: &str| -> Option<CellRef> {
            let (r, c) = key.split_once(':')?;
            Some(CellRef::new(*new_rows.get(r)?, *new_cols.get(c)?))
        };
        for (key, rec) in &rs.cells {
            if replace || ss.cells.get(key) != Some(rec) {
                if let Some(at) = at_of(key) {
                    // A formula is held as the model writes it, so the next local diff finds nothing to say about it.
                    let mut rec = rec.clone();
                    if let Some(f) = &rec.f {
                        rec.f = Some(Formula::new(f).text);
                    }
                    sh.cells_set.push((at, rec));
                }
            }
        }
        if !replace {
            for key in ss.cells.keys() {
                if !rs.cells.contains_key(key)
                    && let Some(at) = at_of(key)
                {
                    sh.cells_removed.push(at);
                }
            }
        }
        if replace || lines_moved || ss.rowp != rs.rowp {
            sh.row_props = Some(props_by_index(&rs.rowp, &rs.rows));
        }
        if replace || lines_moved || ss.colp != rs.colp {
            sh.col_props = Some(props_by_index(&rs.colp, &rs.cols));
        }
        if replace || lines_moved || ss.merges != rs.merges {
            sh.merges = Some(
                rs.merges
                    .iter()
                    .filter_map(|m| {
                        let p: Vec<&str> = m.split(':').collect();
                        if p.len() != 4 {
                            return None;
                        }
                        Some(RangeRef::new(CellRef::new(*new_rows.get(p[0])?, *new_cols.get(p[1])?), CellRef::new(*new_rows.get(p[2])?, *new_cols.get(p[3])?)))
                    })
                    .collect(),
            );
        }
        // The sync becomes the room's. Formulas are kept as the model writes them.
        let mut cells = rs.cells;
        for rec in cells.values_mut() {
            if let Some(f) = &rec.f {
                rec.f = Some(Formula::new(f).text);
            }
        }
        ss.name = rs.name;
        ss.vis = rs.vis;
        ss.tab = rs.tab;
        ss.freeze = rs.freeze;
        ss.rows = rs.rows;
        ss.cols = rs.cols;
        ss.cells = cells;
        ss.rowp = rs.rowp;
        ss.colp = rs.colp;
        ss.merges = rs.merges;
        ss.store = None;
        ss.arc = None;
        sh
    }

    /// The whole shared state arrived (a join, a resync after a refusal, a snapshot): show it. Batches waiting for an
    /// acknowledgement are dropped - the state is the host's truth. Call [`Shim::take_shown`] after it.
    pub fn remote_state(&mut self, doc: &Value) -> Result<(), ShimError> {
        self.view = Mirror::new(doc.clone());
        self.confirmed = None;
        self.pending.clear();
        self.shown = Shown { replace_all: true, ..Shown::default() };
        let ids = list_of(self.view.get(&["sheets"]));
        let mut sheets = Vec::new();
        let mut used: HashSet<String> = HashSet::new();
        let mut reads = Vec::new();
        for id in &ids {
            if let Some(rs) = self.read_sheet(id)? {
                used.extend(rs.cells.values().filter_map(|c| c.s.clone()));
                reads.push((id.clone(), rs));
            }
        }
        self.shown.styles = self.read_styles(&used)?;
        for (i, (id, rs)) in reads.into_iter().enumerate() {
            let mut ss = SheetSync::blank(id, String::new());
            ss.store = None;
            let shown = Self::remote_diff(&mut ss, rs, true);
            sheets.push(ss);
            self.shown.sheets.push((i, shown));
        }
        self.sheets = sheets;
        Ok(())
    }

    /// ★ A REMOTE CHANGE arrived: apply the host's `changes` (and `ack`: the last local batch it applied), put the batches still in
    /// flight back on top, and note what the editor's workbook must take in ([`Shim::take_shown`]). Answers whether there is any.
    pub fn remote_delta(&mut self, changes: &[Change], ack: u64, now_ms: u64) -> Result<bool, ShimError> {
        match self.confirmed.take() {
            Some(mut truth) => {
                // The host applied our batches up to `ack` before it read what it reports.
                while self.pending.first().is_some_and(|(seq, _, _)| *seq <= ack) {
                    let (_, _, ops) = self.pending.remove(0);
                    for op in &ops {
                        let _ = truth.apply(op);
                    }
                }
                for c in changes {
                    truth.apply_change(&c.path, c.value.as_ref())?;
                }
                let mut view = truth.clone();
                for (_, _, ops) in &self.pending {
                    for op in ops {
                        let _ = view.apply(op);
                    }
                }
                self.view = view;
                self.confirmed = if self.pending.is_empty() { None } else { Some(truth) };
            }
            None => {
                for c in changes {
                    self.view.apply_change(&c.path, c.value.as_ref())?;
                }
            }
        }
        self.prune(now_ms)?;
        // Which sheets a change touched.
        let mut touched: BTreeSet<String> = BTreeSet::new();
        let mut order_changed = false;
        let mut styles_changed = false;
        for c in changes {
            match c.path.first().map(String::as_str) {
                Some("s") => match c.path.get(1) {
                    Some(sid) => {
                        touched.insert(sid.clone());
                    }
                    None => order_changed = true,
                },
                Some("sheets") => order_changed = true,
                Some("styles") => styles_changed = true,
                _ => {}
            }
        }
        let ids: Vec<String> = list_of(self.view.get(&["sheets"])).into_iter().filter(|id| self.view.get(&["s", id]).is_some()).collect();
        let current: Vec<String> = self.sheets.iter().map(|s| s.id.clone()).collect();
        if ids != current {
            order_changed = true;
        }
        if touched.is_empty() && !order_changed && !styles_changed {
            return Ok(false);
        }
        let mut old: HashMap<String, (usize, SheetSync)> = std::mem::take(&mut self.sheets).into_iter().enumerate().map(|(i, s)| (s.id.clone(), (i, s))).collect();
        // The sheets the room changed or made are read.
        let mut reads: HashMap<String, RemoteSheet> = HashMap::new();
        for id in &ids {
            if (!old.contains_key(id) || touched.contains(id))
                && let Some(rs) = self.read_sheet(id)?
            {
                reads.insert(id.clone(), rs);
            }
        }
        let used: HashSet<String> = reads.values().flat_map(|rs| rs.cells.values().filter_map(|c| c.s.clone())).collect();
        let styles = self.read_styles(&used)?;
        self.shown.styles.extend(styles);
        let mut layout: Vec<Option<usize>> = Vec::with_capacity(ids.len());
        let mut next: Vec<SheetSync> = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            let (from, mut ss) = match old.remove(id) {
                Some((j, s)) => (Some(j), s),
                None => {
                    let mut blank = SheetSync::blank(id.clone(), String::new());
                    blank.store = None;
                    (None, blank)
                }
            };
            if let Some(rs) = reads.remove(id) {
                let mut shown = Self::remote_diff(&mut ss, rs, false);
                if from.is_none() {
                    // A sheet the room made: the editor makes it empty, and its lines and cells arrive as the shown change.
                    shown.remap = None;
                }
                if !shown.is_empty() {
                    self.shown.sheets.push((i, shown));
                }
            }
            layout.push(from);
            next.push(ss);
        }
        self.sheets = next;
        if layout.len() != current.len() || layout.iter().enumerate().any(|(i, o)| *o != Some(i)) {
            self.shown.layout = Some(layout);
        }
        // Comments are placed by ids, which may just have arrived.
        self.apply_host_comments();
        Ok(!self.shown.is_empty())
    }

    /// Everything the remote changes since the last call altered in the editor's workbook.
    pub fn take_shown(&mut self) -> Shown {
        std::mem::take(&mut self.shown)
    }
}

fn set_at(node: &mut Value, path: &[String], v: Value) {
    let Some((first, rest)) = path.split_first() else {
        *node = v;
        return;
    };
    if !node.is_object() {
        *node = Value::Object(serde_json::Map::new());
    }
    if let Some(o) = node.as_object_mut() {
        let child = o.entry(first.clone()).or_insert(Value::Null);
        set_at(child, rest, v);
    }
}

fn remove_at(root: &mut Value, path: &[String]) {
    let mut at = root;
    for seg in &path[..path.len().saturating_sub(1)] {
        match at.as_object_mut().and_then(|o| o.get_mut(seg)) {
            Some(next) => at = next,
            None => return,
        }
    }
    if let (Some(leaf), Some(o)) = (path.last(), at.as_object_mut()) {
        o.remove(leaf);
    }
}

/// The ops for one cell: a cell the room lacks is made with `ensure`, a field that differs is set, one that went is removed, and a cell
/// that went is removed whole.
fn cell_ops(path: &[String], key: &str, new: Option<&CellRec>, old: Option<&CellRec>, ops: &mut Vec<Value>, kinds: &mut Kinds) {
    match new {
        None => {
            if old.is_some() {
                ops.push(json!({"op": "remove", "path": path, "key": key}));
                kinds.content += 1;
            }
        }
        Some(rec) => {
            let changes = rec.changes_from(old);
            if changes.is_empty() {
                return;
            }
            if old.is_none() {
                ops.push(json!({"op": "ensure", "path": path, "key": key, "kind": "map"}));
            }
            let mut cell_path: Vec<String> = path.to_vec();
            cell_path.push(key.to_owned());
            let mut content = false;
            for (field, value) in changes {
                content |= field != "s";
                match value {
                    Some(v) => ops.push(json!({"op": "set", "path": cell_path, "key": field, "value": v})),
                    None => ops.push(json!({"op": "remove", "path": cell_path, "key": field})),
                }
            }
            if content {
                kinds.content += 1;
            } else {
                kinds.format += 1;
            }
        }
    }
}
