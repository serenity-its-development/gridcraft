//! Live editing for the host bridge: the frame side of the host's common collaboration adapter (design section 12). The shim itself
//! is `gridcraft-collab` (pure, tested natively and against the host's real engine); this module is its wiring: it turns the editor
//! session's edits into `craft:crdt:ops`, and the host's `state` / `delta` / `seed` / `export` into the workbook the editor shows.
//!
//! What goes both ways: sheets, rows and columns, cells (values, formulas, styles), row heights and column widths, merged cells,
//! comments (through the host's comments tier, as `craft:crdt:comment`) and the selections of the people in the room
//! (`craft:crdt:cursor` out, `craft:crdt:peer` in).
//!
//! Order each frame: the host's messages that wait for the file to finish opening are held; before a remote change is shown, the
//! person's own edits since the last frame are sent (so none is overwritten); then the remote change is shown; then any edit this
//! frame made is sent. Undo and Redo are the room's (`craft:crdt:undo` / `redo`): the session's own snapshots would also take
//! back what others typed.

use std::collections::{BTreeMap, VecDeque};

use gridcraft_collab::{Applied, Batch, Shim, changes_of};
use gridcraft_engine::RemoteChange;
use gridcraft_ui_egui::{RemoteCursor, SheetApp};
use serde_json::{Value, json};

use crate::host::{Bridge, Event};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not a live workbook (the host opened it without `collab`, or live editing ended).
    Off,
    /// Opened with `collab`: waiting for the host to seed the room or send its state.
    Joining,
    /// In the room.
    Live,
}

/// Another person in the room: their name, colour and last cursor body (resolved against the workbook every frame).
struct Peer {
    name: String,
    color: [u8; 3],
    cursor: Value,
}

/// My selection goes out when it moves (at most this often) and again this often while it stays, so the others' copy of it does not
/// expire (the host drops a cursor 6 s after its last frame).
const CURSOR_MIN_MS: u64 = 120;
const CURSOR_REPEAT_MS: u64 = 3_000;

pub struct Collab {
    bridge: Bridge,
    shim: Shim,
    phase: Phase,
    /// The uid of the active workbook when the host's `craft:open` arrived: the file has loaded once the session holds another.
    baseline: u64,
    loaded: bool,
    /// The workbook and revision last handed to (or taken from) the shim.
    doc_id: u64,
    rev: u64,
    held: VecDeque<Event>,
    peers: BTreeMap<String, Peer>,
    last_cursor: Option<Value>,
    last_cursor_ms: u64,
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Random bits for ids, from the browser's crypto.
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    if let Some(crypto) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let _ = crypto.get_random_values_with_u8_array(&mut buf);
    }
    u64::from_le_bytes(buf) ^ (js_sys::Math::random() * 9.007_199_254_740_992e15) as u64
}

/// `#rrggbb` as bytes.
fn rgb_of(hex: &str) -> [u8; 3] {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0x7c);
    [byte(0), byte(2), byte(4)]
}

fn status(app: &mut SheetApp, text: String) {
    app.toast = Some((text, gridcraft_ui_egui::now_ms()));
}

impl Collab {
    pub fn new(bridge: Bridge) -> Self {
        Collab {
            bridge,
            shim: Shim::new(random_seed()),
            phase: Phase::Off,
            baseline: 0,
            loaded: false,
            doc_id: 0,
            rev: 0,
            held: VecDeque::new(),
            peers: BTreeMap::new(),
            last_cursor: None,
            last_cursor_ms: 0,
        }
    }

    fn off(&mut self, app: &mut SheetApp, why: &str) {
        self.phase = Phase::Off;
        app.session.collab_undo = false;
        app.remote_cursors.clear();
        status(app, format!("Live editing is off: {why}"));
    }

    fn go_live(&mut self, app: &mut SheetApp) {
        self.phase = Phase::Live;
        if let Some(d) = app.session.active() {
            self.doc_id = d.uid;
            self.rev = d.revision;
        }
        app.session.collab_undo = true;
    }

    fn post_batches(&self, batches: &[Batch]) {
        for b in batches {
            let mut body = json!({"ops": b.ops, "seq": b.seq, "undo": if b.join { "join" } else { "step" }, "label": b.label});
            if b.seed {
                body["seed"] = json!(true);
            }
            self.bridge.post_crdt("ops", &body);
        }
    }

    /// Send the comment messages the shim holds.
    fn post_comment_msgs(&mut self) {
        for m in self.shim.take_comment_msgs() {
            self.bridge.post_crdt("comment", &m.body());
        }
    }

    /// Send what the person changed since the last frame.
    fn flush_local(&mut self, app: &mut SheetApp) {
        if self.phase != Phase::Live {
            return;
        }
        let Some((uid, revision)) = app.session.active().map(|d| (d.uid, d.revision)) else { return };
        if uid != self.doc_id {
            self.off(app, "another workbook was opened in this window");
            return;
        }
        if revision == self.rev {
            return;
        }
        self.rev = revision;
        if self.bridge.opened().read_only {
            return;
        }
        let label = app.host_label.take();
        let Some(d) = app.session.active() else { return };
        match self.shim.local_all(&d.wb, label.as_deref(), now_ms()) {
            Ok(batches) => {
                self.post_batches(&batches);
                self.post_comment_msgs();
            }
            Err(e) => self.off(app, &e.to_string()),
        }
    }

    /// Take in what remote changes altered in the workbook.
    fn apply_shown(&mut self, app: &mut SheetApp) {
        let shown = self.shim.take_shown();
        if !shown.is_empty() {
            app.session.change_remote(|wb| {
                let Applied { structure, cells } = shown.apply_to(wb);
                RemoteChange { structure, cells }
            });
            if let Some(d) = app.session.active() {
                self.shim.resync(&d.wb);
                self.rev = d.revision;
            }
        }
        self.post_comment_msgs();
    }

    fn handle(&mut self, app: &mut SheetApp, kind: &str, body: &Value) {
        match kind {
            "state" => {
                let doc = body.get("state").and_then(|s| s.get("doc")).cloned().unwrap_or(Value::Null);
                // An empty room has nothing to show: the host asks for the seed.
                if doc.as_object().is_none_or(|o| o.get("sheets").is_none()) {
                    return;
                }
                if body.get("readOnly").and_then(Value::as_bool) == Some(true) {
                    app.session.view_only = true;
                }
                self.flush_local(app);
                match self.shim.remote_state(&doc) {
                    Ok(()) => {
                        if let Some(comments) = body.get("state").and_then(|s| s.get("comments")) {
                            self.shim.remote_comments(comments);
                        }
                        self.apply_shown(app);
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "seed" => {
                let Some(d) = app.session.active() else { return };
                match self.shim.seed_all(&d.wb, now_ms()) {
                    Ok(batches) => {
                        self.post_batches(&batches);
                        self.post_comment_msgs();
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "delta" => {
                if self.phase != Phase::Live {
                    return;
                }
                self.flush_local(app);
                if self.phase != Phase::Live {
                    return;
                }
                let ack = body.get("ack").and_then(Value::as_u64).unwrap_or(0);
                // The shim reads what is below `doc`, and the host's comments as one value at `comments`.
                let all = changes_of(body.get("changes").unwrap_or(&Value::Null));
                self.shim.remote_comment_changes(&all);
                let doc_changes: Vec<_> = all
                    .into_iter()
                    .filter_map(|mut c| {
                        (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                            c.path.remove(0);
                            c
                        })
                    })
                    .collect();
                if let Err(e) = self.shim.remote_delta(&doc_changes, ack, now_ms()) {
                    self.off(app, &e.to_string());
                    return;
                }
                self.apply_shown(app);
            }
            "export" => {
                self.flush_local(app);
                let request = body.get("request").and_then(Value::as_u64);
                let opened = self.bridge.opened();
                let Some(d) = app.session.active() else { return };
                match gridcraft_engine::io::save_bytes(&d.wb, &opened.name) {
                    Ok(bytes) => {
                        if let Err(e) = self.bridge.post_save(&opened.name, &bytes, request) {
                            status(app, format!("The checkpoint was not sent: {e}"));
                        }
                    }
                    Err(e) => status(app, format!("The checkpoint could not be made: {e}")),
                }
            }
            "peer" => {
                let Some(party) = body.get("party").and_then(Value::as_str) else { return };
                match body.get("cursor") {
                    Some(c) if !c.is_null() => {
                        let name = body.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Someone").to_owned();
                        let color = rgb_of(body.get("color").and_then(Value::as_str).unwrap_or("#7c3aed"));
                        self.peers.insert(party.to_owned(), Peer { name, color, cursor: c.clone() });
                    }
                    _ => {
                        self.peers.remove(party);
                    }
                }
            }
            "unplaced" => status(app, "Some of your offline edits could not be placed in the live workbook.".into()),
            _ => {}
        }
    }

    /// Once per frame BEFORE the app's own pass: a `craft:open` is noted against the workbook the session holds right now (the app
    /// opens the file from the inbox during its pass, so the open has happened once the session holds another).
    pub fn pre(&mut self, app: &SheetApp) {
        for ev in self.bridge.take_events() {
            match ev {
                Event::Opened(o) => {
                    self.phase = if o.collab { Phase::Joining } else { Phase::Off };
                    self.baseline = app.session.active().map_or(0, |d| d.uid);
                    self.loaded = false;
                    // Messages for the file that was open before are not for this one.
                    self.held.clear();
                    self.peers.clear();
                    self.last_cursor = None;
                }
                other => self.held.push_back(other),
            }
        }
    }

    /// Once per frame AFTER the app's own pass.
    pub fn post(&mut self, app: &mut SheetApp) {
        let uid = app.session.active().map_or(0, |d| d.uid);
        if !self.loaded && self.phase != Phase::Off && uid != self.baseline && uid != 0 {
            self.loaded = true;
            if let Some(d) = app.session.active() {
                self.doc_id = d.uid;
                self.rev = d.revision;
            }
            app.session.collab_undo = false;
            app.session.view_only = self.bridge.opened().read_only;
        }
        while self.loaded || matches!(self.held.front(), Some(Event::Refused { .. })) {
            match self.held.pop_front() {
                Some(Event::Refused { code, reason }) => status(app, if code.is_empty() { reason } else { format!("{reason} ({code})") }),
                Some(Event::Crdt { kind, body }) => self.handle(app, &kind, &body),
                Some(Event::Opened(_)) | None => break,
            }
        }
        if self.loaded {
            self.flush_local(app);
        }
        for undo in std::mem::take(&mut app.session.collab_undo_requests) {
            self.bridge.post_crdt(if undo { "undo" } else { "redo" }, &json!({}));
        }
        if self.phase == Phase::Live {
            self.share_cursor(app);
            self.show_peers(app);
        }
    }

    /// Say where my selection is: when it moves, and now and then while it stays.
    fn share_cursor(&mut self, app: &SheetApp) {
        let now = now_ms();
        let Some(d) = app.session.active() else { return };
        let range = d.selection.current();
        let Some(body) = self.shim.cursor_for(d.wb.active_sheet, range.start, range.end) else { return };
        let moved = self.last_cursor.as_ref() != Some(&body);
        let due = if moved { now.saturating_sub(self.last_cursor_ms) >= CURSOR_MIN_MS } else { now.saturating_sub(self.last_cursor_ms) >= CURSOR_REPEAT_MS };
        if !due {
            return;
        }
        self.bridge.post_crdt("cursor", &json!({"cursor": body}));
        self.last_cursor = Some(body);
        self.last_cursor_ms = now;
    }

    /// The other people's selections, resolved against the workbook as it is now.
    fn show_peers(&mut self, app: &mut SheetApp) {
        let cursors: Vec<RemoteCursor> = self
            .peers
            .values()
            .filter_map(|p| self.shim.peer_cursor(&p.cursor).map(|c| RemoteCursor { name: p.name.clone(), color: p.color, sheet: c.sheet, anchor: c.anchor, focus: c.focus }))
            .collect();
        if cursors != app.remote_cursors {
            app.remote_cursors = cursors;
        }
    }
}
