//! The shim against a stand-in room: a tree that takes the ops of the contract and reports what each batch changed as the host's
//! `changes`. The real engine's two-window convergence is tested from the OneOS repository (`craft-sheets-convergence.test.ts`).

use std::sync::Arc;

use gridcraft_core::{CellRef, Value as Cv};
use gridcraft_model::{Cell, Comment, Formula, Style, Workbook};
use serde_json::{Value, json};

use crate::mirror::Mirror;
use crate::{Batch, Change, Shim, changes_of, digest};

struct Room {
    root: Mirror,
}

fn diff(old: &Value, new: &Value, path: &mut Vec<String>, out: &mut Vec<Value>) {
    match (old.as_object(), new.as_object()) {
        (Some(o), Some(n)) if !n.contains_key("$list") && !n.contains_key("$text") => {
            for key in o.keys().chain(n.keys()).collect::<std::collections::BTreeSet<_>>() {
                path.push(key.clone());
                match (o.get(key), n.get(key)) {
                    (Some(a), Some(b)) if a != b => diff(a, b, path, out),
                    (Some(_), None) => out.push(json!({"path": path.clone(), "removed": true})),
                    (None, Some(b)) => out.push(json!({"path": path.clone(), "value": b})),
                    _ => {}
                }
                path.pop();
            }
        }
        _ => out.push(json!({"path": path.clone(), "value": new})),
    }
}

impl Room {
    fn new() -> Self {
        Room { root: Mirror::default() }
    }

    /// Apply a window's batches; answer the changes (as the host's `craft:crdt:delta`).
    fn take(&mut self, batches: &[Batch]) -> Vec<Change> {
        let before = self.root.root.clone();
        for b in batches {
            for op in &b.ops {
                self.root.apply(op).unwrap_or_else(|e| panic!("the room refused {op}: {e}"));
            }
        }
        let mut out = Vec::new();
        diff(&before, &self.root.root, &mut Vec::new(), &mut out);
        changes_of(&Value::Array(out))
    }
}

struct Win {
    shim: Shim,
    wb: Workbook,
    clock: u64,
}

impl Win {
    fn new(seed: u64, wb: Workbook) -> Self {
        Win { shim: Shim::new(seed), wb, clock: 1000 }
    }

    fn edit(&mut self, sheet: usize, f: impl FnOnce(&mut gridcraft_model::Sheet)) {
        f(Arc::make_mut(&mut self.wb.sheets[sheet]));
    }

    fn flush(&mut self) -> Vec<Batch> {
        self.clock += 100;
        self.shim.local_all(&self.wb, None, self.clock).unwrap()
    }

    /// The room's changes arrive in this window.
    fn hear(&mut self, changes: &[Change], ack: u64) {
        self.clock += 100;
        self.shim.remote_delta(changes, ack, self.clock).unwrap();
        let shown = self.shim.take_shown();
        shown.apply_to(&mut self.wb);
        self.shim.resync(&self.wb);
    }

    fn join(&mut self, room: &Room) {
        self.shim.remote_state(&room.root.root).unwrap();
        let shown = self.shim.take_shown();
        shown.apply_to(&mut self.wb);
        self.shim.resync(&self.wb);
    }
}

fn num(n: f64) -> Cell {
    Cell::value(Cv::Number(n))
}

fn text(t: &str) -> Cell {
    Cell::value(Cv::Text(Arc::from(t)))
}

fn book(rows: &[&[f64]]) -> Workbook {
    let mut wb = Workbook::new();
    let sheet = Arc::make_mut(&mut wb.sheets[0]);
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            sheet.cells.set(CellRef::new(r as u32, c as u32), num(*v));
        }
    }
    wb
}

fn a1(a: &str) -> CellRef {
    let col = a.chars().take_while(char::is_ascii_alphabetic).fold(0u32, |n, c| n * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1)) - 1;
    let row: u32 = a.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u32>().unwrap() - 1;
    CellRef::new(row, col)
}

/// Two windows on one workbook: the first seeds the room, the second joins it.
fn pair(wb: Workbook) -> (Win, Win, Room) {
    let mut room = Room::new();
    let mut a = Win::new(11, wb.clone());
    let batches = a.shim.seed_all(&a.wb, 1000).unwrap();
    assert!(batches[0].seed && batches.iter().skip(1).all(|b| !b.seed));
    room.take(&batches);
    a.shim.resync(&a.wb);
    let mut b = Win::new(22, wb);
    b.join(&room);
    assert_eq!(digest(&a.wb), digest(&b.wb), "the joined window shows what the seeding one has");
    (a, b, room)
}

fn send(from: &mut Win, to: &mut Win, room: &mut Room) {
    let batches = from.flush();
    let ack = batches.last().map_or(0, |b| b.seq);
    let changes = room.take(&batches);
    from.hear(&changes, ack);
    to.hear(&changes, 0);
}

#[test]
fn a_seeded_room_shows_the_same_workbook_to_the_second_window() {
    let mut wb = book(&[&[1.0, 2.0], &[3.0, 4.0]]);
    let sheet = Arc::make_mut(&mut wb.sheets[0]);
    sheet.cells.set(a1("C1"), Cell::formula(Formula::new("A1+B1")));
    sheet.cells.set(a1("A3"), text("héllo"));
    let styled = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.font.bold = true);
    Arc::make_mut(&mut wb.sheets[0]).cells.set(a1("B3"), Cell { value: Cv::Number(9.0), formula: None, style: styled });
    let (a, b, _room) = pair(wb);
    let d = digest(&b.wb);
    assert_eq!(d["sheets"][0]["cells"]["C1"]["f"], "A1+B1");
    assert_eq!(d["sheets"][0]["cells"]["A3"]["v"]["v"], "héllo");
    assert_eq!(d["sheets"][0]["cells"]["B3"]["s"]["font"]["bold"], true);
    assert_eq!(digest(&a.wb), d);
}

#[test]
fn an_edit_in_one_window_reaches_the_other_and_two_edits_in_different_cells_merge() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0, 2.0], &[3.0, 4.0], &[5.0, 6.0]]));
    a.edit(0, |s| s.cells.set(a1("A1"), num(10.0)));
    b.edit(0, |s| s.cells.set(a1("B3"), num(60.0)));
    // Both edit before either hears the other.
    let (ba, bb) = (a.flush(), b.flush());
    let (ca, cb) = (room.take(&ba), room.take(&bb));
    a.hear(&ca, ba.last().unwrap().seq);
    a.hear(&cb, 0);
    b.hear(&cb, bb.last().unwrap().seq);
    b.hear(&ca, 0);
    assert_eq!(digest(&a.wb), digest(&b.wb));
    let d = digest(&a.wb);
    assert_eq!(d["sheets"][0]["cells"]["A1"]["v"]["v"], 10.0);
    assert_eq!(d["sheets"][0]["cells"]["B3"]["v"]["v"], 60.0);
}

#[test]
fn rows_inserted_by_one_window_move_the_cells_in_the_other_and_a_concurrent_edit_follows_its_row() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0], &[2.0], &[3.0], &[4.0], &[5.0]]));
    // A inserts a row at the top (everything below moves down); B edits the cell holding 4.
    a.edit(0, |s| {
        let cells: Vec<(CellRef, Cell)> = s.cells.iter().map(|(p, c)| (p, c.clone())).collect();
        s.cells = gridcraft_model::CellStore::new();
        for (p, c) in cells {
            s.cells.set(CellRef::new(p.row + 1, p.col), c);
        }
    });
    b.edit(0, |s| s.cells.set(a1("A4"), num(40.0)));
    let (ba, bb) = (a.flush(), b.flush());
    assert!(ba.iter().flat_map(|x| &x.ops).any(|op| op["op"] == "list.insert" && op["path"][2] == "rows"), "a row id was inserted");
    assert_eq!(ba[0].label, "Insert rows");
    let (ca, cb) = (room.take(&ba), room.take(&bb));
    a.hear(&ca, ba.last().unwrap().seq);
    a.hear(&cb, 0);
    b.hear(&cb, bb.last().unwrap().seq);
    b.hear(&ca, 0);
    assert_eq!(digest(&a.wb), digest(&b.wb));
    let cells = &digest(&a.wb)["sheets"][0]["cells"];
    // The new blank row is row 1; the old cells moved down one; B's edit stayed with its row.
    assert!(cells.get("A1").is_none());
    assert_eq!(cells["A2"]["v"]["v"], 1.0);
    assert_eq!(cells["A5"]["v"]["v"], 40.0);
    assert_eq!(cells["A6"]["v"]["v"], 5.0);
}

#[test]
fn a_deleted_row_goes_from_the_other_window_too() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0], &[2.0], &[3.0], &[4.0]]));
    a.edit(0, |s| {
        let cells: Vec<(CellRef, Cell)> = s.cells.iter().map(|(p, c)| (p, c.clone())).collect();
        s.cells = gridcraft_model::CellStore::new();
        for (p, c) in cells {
            if p.row == 1 {
                continue;
            }
            s.cells.set(CellRef::new(if p.row > 1 { p.row - 1 } else { p.row }, p.col), c);
        }
    });
    send(&mut a, &mut b, &mut room);
    assert_eq!(digest(&a.wb), digest(&b.wb));
    let cells = &digest(&b.wb)["sheets"][0]["cells"];
    assert_eq!(cells["A1"]["v"]["v"], 1.0);
    assert_eq!(cells["A2"]["v"]["v"], 3.0);
    assert_eq!(cells["A3"]["v"]["v"], 4.0);
    assert!(cells.get("A4").is_none());
}

#[test]
fn sheets_are_added_renamed_and_removed_across_windows() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0]]));
    a.wb.sheets.push(Arc::new(gridcraft_model::Sheet::new("Costs")));
    a.edit(1, |s| s.cells.set(a1("A1"), num(7.0)));
    send(&mut a, &mut b, &mut room);
    assert_eq!(digest(&b.wb)["sheets"].as_array().unwrap().len(), 2);
    assert_eq!(digest(&b.wb)["sheets"][1]["name"], "Costs");
    assert_eq!(digest(&b.wb)["sheets"][1]["cells"]["A1"]["v"]["v"], 7.0);
    b.edit(1, |s| s.name = "Costs 2026".into());
    send(&mut b, &mut a, &mut room);
    assert_eq!(digest(&a.wb)["sheets"][1]["name"], "Costs 2026");
    // A moves the second sheet first.
    a.wb.sheets.swap(0, 1);
    send(&mut a, &mut b, &mut room);
    assert_eq!(digest(&a.wb), digest(&b.wb));
    assert_eq!(digest(&b.wb)["sheets"][0]["name"], "Costs 2026");
    // B removes it.
    b.wb.sheets.remove(0);
    send(&mut b, &mut a, &mut room);
    assert_eq!(digest(&a.wb)["sheets"].as_array().unwrap().len(), 1);
    assert_eq!(digest(&a.wb), digest(&b.wb));
}

#[test]
fn formats_sizes_and_merges_travel() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0, 2.0], &[3.0, 4.0]]));
    let bold = a.wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s: &mut Style| s.font.bold = true);
    a.edit(0, |s| {
        s.cells.set(a1("A1"), Cell { value: Cv::Number(1.0), formula: None, style: bold });
        s.rows.insert(1, gridcraft_model::LineInfo { size: Some(40.0), custom: true, ..Default::default() });
        s.cols.insert(0, gridcraft_model::LineInfo { size: Some(120.0), custom: true, ..Default::default() });
        s.merges.push(gridcraft_core::RangeRef::new(a1("A1"), a1("B1")));
        s.freeze = Some((1, 0));
    });
    let batches = a.flush();
    assert!(!batches.is_empty());
    let changes = room.take(&batches);
    a.hear(&changes, batches.last().unwrap().seq);
    b.hear(&changes, 0);
    assert_eq!(digest(&a.wb), digest(&b.wb));
    let d = digest(&b.wb);
    assert_eq!(d["sheets"][0]["cells"]["A1"]["s"]["font"]["bold"], true);
    assert_eq!(d["sheets"][0]["rows"]["1"]["h"], 40.0);
    assert_eq!(d["sheets"][0]["cols"]["0"]["h"], 120.0);
    assert_eq!(d["sheets"][0]["merges"][0], "A1:B1");
    assert_eq!(d["sheets"][0]["freeze"], json!([1, 0]));
}

#[test]
fn a_comment_becomes_a_thread_message_and_a_remote_thread_shows_on_its_cell() {
    let (mut a, mut b, mut room) = pair(book(&[&[1.0], &[2.0]]));
    a.edit(0, |s| {
        s.comments.insert(
            a1("A2"),
            Comment { author: "Ann".into(), text: "check this".into(), replies: vec![], threaded: true, resolved: false, visible: false, dates: vec![], note_box: None },
        );
    });
    let _ = a.flush();
    let msgs = a.shim.take_comment_msgs();
    assert_eq!(msgs.len(), 1);
    let body = msgs[0].body();
    assert_eq!(body["action"], "add");
    assert_eq!(body["text"], "check this");
    // The host stores it as {by, name, at, text, resolved, anchor} and tells both windows.
    let host = json!({"c1": {"by": "p1", "name": "Ann", "at": 1_790_000_000_000u64, "text": "check this", "resolved": false, "anchor": body["anchor"]}});
    b.shim.remote_comments(&host);
    let shown = b.shim.take_shown();
    shown.apply_to(&mut b.wb);
    assert_eq!(digest(&b.wb)["sheets"][0]["comments"]["A2"]["text"], "check this");
    // A reply by B is a message; A hears the thread with it.
    b.edit(0, |s| {
        if let Some(c) = s.comments.get_mut(&a1("A2")) {
            c.replies.push(("Ben".into(), "done".into()));
        }
    });
    let _ = b.flush();
    let replies = b.shim.take_comment_msgs();
    assert_eq!(replies, vec![crate::comments::CommentMsg::Reply { parent: "c1".into(), text: "done".into() }]);
    let host = json!({
        "c1": {"by": "p1", "name": "Ann", "at": 1_790_000_000_000u64, "text": "check this", "resolved": false, "anchor": body["anchor"]},
        "c2": {"by": "p2", "name": "Ben", "at": 1_790_000_005_000u64, "text": "done", "resolved": false, "parent": "c1"}
    });
    a.shim.remote_comments(&host);
    a.shim.take_shown().apply_to(&mut a.wb);
    assert_eq!(digest(&a.wb)["sheets"][0]["comments"]["A2"]["replies"], json!(["done"]));
    let _ = &mut room;
}

#[test]
fn a_cursor_names_cells_by_id_and_resolves_in_the_other_window_after_rows_moved() {
    let (mut a, b, _room) = pair(book(&[&[1.0], &[2.0], &[3.0], &[4.0]]));
    let body = a.shim.cursor_for(0, a1("A3"), a1("A4")).unwrap();
    assert!(body.to_string().len() < 2048);
    let peer = b.shim.peer_cursor(&body).unwrap();
    assert_eq!((peer.sheet, peer.anchor, peer.focus), (0, a1("A3"), a1("A4")));
    a.clock += 1;
}

#[test]
fn a_sheet_too_big_to_share_is_refused_by_name() {
    let mut wb = Workbook::new();
    Arc::make_mut(&mut wb.sheets[0]).cells.set(CellRef::new(60_000, 0), num(1.0));
    let mut shim = Shim::new(3);
    let e = shim.seed_all(&wb, 1).unwrap_err();
    assert!(e.to_string().contains("up to 50000 rows") || e.to_string().contains("50000"), "{e}");
}

#[test]
fn a_big_seed_is_split_into_batches_of_at_most_the_hosts_limit() {
    let mut wb = Workbook::new();
    let sheet = Arc::make_mut(&mut wb.sheets[0]);
    for r in 0..400 {
        for c in 0..6 {
            sheet.cells.set(CellRef::new(r, c), num(f64::from(r * 10 + c)));
        }
    }
    let mut shim = Shim::new(5);
    let batches = shim.seed_all(&wb, 1).unwrap();
    assert!(batches.len() > 1);
    assert!(batches.iter().all(|b| b.ops.len() <= crate::BATCH_OPS));
    assert!(batches[0].seed);
}
