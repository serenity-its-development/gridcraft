//! The OneOS host bridge: `postMessage` to the ONE parent the frame page declared. The frame page's loader
//! (`bootstrap.js`) sets `globalThis.__craftHost = {app, version, parent}` from the page's `data-parent`; the editor
//! lives on its own origin, so the parent is a DIFFERENT origin. Messages go to that origin only, and only the parent
//! window on that origin is heard. Every message is a plain object with a `type`; `session` is the per-open nonce the
//! host mints in `craft:open` and every later message echoes
//! (`plans/mona/craft-integration-design-2026-10-25.md` sections 2.2 and 12):
//!
//! - `craft:ready {app, version}` (to the parent): listening; send the file now.
//! - `craft:open {session, name, mime, bytes, versionId, readOnly, collab}` (from the parent): open these bytes. With
//!   `collab` the file is also a live document: the host then sends `craft:crdt:seed` or `craft:crdt:state`.
//! - `craft:save {session, name, mime, bytes, baseVersionId, saveAs, checkpoint?}` (to the parent): the person saved (or
//!   the room asked for a checkpoint: `checkpoint` echoes the request). The bytes are a transferred `ArrayBuffer`; the host
//!   makes a NEW version of the file through the compare-and-set and answers `craft:saved {session, versionId}` (the next
//!   save's base) or `craft:refused {session, code, reason}`. Anything that is not the opened file under its own name (an
//!   export, a Save As) goes as `saveAs: true`, a copy, never a new version of the opened file.
//! - `craft:dirty {session, dirty}` (to the parent): unsaved changes appeared or went away.
//! - `craft:failed {error}` (to the parent): the editor could not start.
//! - `craft:crdt:*` both ways: the live editing channel ([`crate::collab`]).
//! - `craft:command {session, id, method: "command.batch", params: {label, steps: [{name, args?}]}}` (from the parent) is
//!   the assistant's allow-listed run (`gridcraft_ui_egui::host_commands`): all of it as ONE undo step, or none of it. It is
//!   answered `craft:result {session, id, ok:true, result}` or `{ok:false, error: "<code>: <reason>"}`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use gridcraft_ui_egui::{HostRun, HostRuns, Inbox, Services, SheetApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Saving one of these hands the whole workbook to the host, which stores it: the workbook counts as saved under that name,
/// as it would after Save on the desktop.
const WORKBOOK_EXTS: &[&str] = &["xlsx"];

/// What the host's last `craft:open` said.
#[derive(Clone, Default)]
pub struct Opened {
    pub session: String,
    pub name: String,
    pub version_id: String,
    pub read_only: bool,
    pub collab: bool,
}

/// What the host said that the shell acts on, in order.
pub enum Event {
    /// A `craft:open` arrived (its bytes went to the inbox).
    Opened(Opened),
    /// `craft:crdt:<kind>` with its body.
    Crdt { kind: String, body: serde_json::Value },
    /// `craft:refused`.
    Refused { code: String, reason: String },
}

#[derive(Default)]
struct Shared {
    opened: Opened,
    events: VecDeque<Event>,
    /// Names of the files handed to the host since the last frame.
    saved_names: Vec<String>,
    /// The (session, dirty) the host was last told: a new open starts again.
    reported: (String, Option<bool>),
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    /// The declared parent origin: the only target and the only sender.
    origin: String,
    app: String,
    version: String,
    shared: Rc<RefCell<Shared>>,
}

/// `scheme://host[:port]` exactly: https, or http only on localhost (the dev estate).
fn plain_origin(v: &str) -> bool {
    let Some((scheme, rest)) = v.split_once("://") else { return false };
    let local = rest == "localhost" || rest.starts_with("localhost:") || rest.starts_with("127.0.0.1");
    (scheme == "https" || (scheme == "http" && local)) && !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
}

impl Bridge {
    /// `None` when the page is not framed, or declares no parent origin.
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let declared = js_sys::Reflect::get(&window, &JsValue::from_str("__craftHost")).ok().filter(|d| d.is_object())?;
        let origin = string(&declared, "parent").filter(|o| plain_origin(o))?;
        Some(Self {
            window,
            parent,
            origin,
            app: string(&declared, "app").unwrap_or_else(|| "sheets".into()),
            version: string(&declared, "version").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
            shared: Rc::default(),
        })
    }

    /// Send saved and exported files to the host instead of downloading them, and hear the files, the assistant's runs and the
    /// live channel it sends.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, runs: HostRuns, ctx: egui::Context) {
        self.listen(inbox, runs, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes, None) {
                log::error!("couldn't hand {name} to the host: {e}");
            }
        }));
    }

    pub fn opened(&self) -> Opened {
        self.shared.borrow().opened.clone()
    }

    pub fn take_events(&self) -> Vec<Event> {
        self.shared.borrow_mut().events.drain(..).collect()
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "craft:ready".into()), ("app", self.app.as_str().into()), ("version", self.version.as_str().into())]);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "craft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: a workbook handed to the host this frame counts as saved under the name it was saved as, and the host
    /// hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SheetApp) {
        let (saved, session) = {
            let mut s = self.shared.borrow_mut();
            (std::mem::take(&mut s.saved_names), s.opened.session.clone())
        };
        if let Some(name) = saved.iter().rev().find(|name| is_workbook(name))
            && let Some(doc) = app.session.active_mut()
        {
            doc.saved = doc.wb.clone();
            if doc.path.is_none() {
                doc.title = name.clone();
            }
        }
        if session.is_empty() {
            return;
        }
        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        let told = {
            let mut s = self.shared.borrow_mut();
            if s.reported.0 != session {
                s.reported = (session.clone(), None);
            }
            s.reported.1
        };
        if told != Some(dirty) && self.send(&[("type", "craft:dirty".into()), ("session", session.as_str().into()), ("dirty", dirty.into())]) {
            self.shared.borrow_mut().reported.1 = Some(dirty);
        }
    }

    /// Send a live-editing frame: `craft:crdt:<kind>` with `body`'s members beside the session.
    pub fn post_crdt(&self, kind: &str, body: &serde_json::Value) {
        let session = self.shared.borrow().opened.session.clone();
        let Ok(json) = serde_json::to_string(body) else { return };
        let Ok(parsed) = js_sys::JSON::parse(&json) else { return };
        let _ = js_sys::Reflect::set(&parsed, &JsValue::from_str("type"), &JsValue::from_str(&format!("craft:crdt:{kind}")));
        let _ = js_sys::Reflect::set(&parsed, &JsValue::from_str("session"), &JsValue::from_str(&session));
        let _ = self.parent.post_message(&parsed, &self.origin);
    }

    /// `craft:result {session, id, ok, result|error}` to the host, on the session it was asked in.
    pub fn post_result(&self, id: f64, outcome: Result<serde_json::Value, String>) {
        let session = self.shared.borrow().opened.session.clone();
        self.post_result_for(&session, id, outcome);
    }

    fn post_result_for(&self, session: &str, id: f64, outcome: Result<serde_json::Value, String>) {
        let mut fields: Vec<(&str, JsValue)> = vec![("type", "craft:result".into()), ("session", session.into()), ("id", id.into()), ("ok", outcome.is_ok().into())];
        match &outcome {
            Ok(value) => fields.push(("result", js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL))),
            Err(error) => fields.push(("error", error.as_str().into())),
        }
        self.send(&fields);
    }

    /// Save (or a checkpoint, when `checkpoint` names the room's request) as a message the host turns into a new version.
    pub fn post_save(&self, path: &str, bytes: &[u8], checkpoint: Option<u64>) -> Result<(), String> {
        let (session, base, read_only, opened_name) = {
            let s = self.shared.borrow();
            (s.opened.session.clone(), s.opened.version_id.clone(), s.opened.read_only, s.opened.name.clone())
        };
        if session.is_empty() {
            return Err("the host has not opened a workbook yet".into());
        }
        if read_only {
            return Err("this file is open to view; saving is off".into());
        }
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let mut fields: Vec<(&str, JsValue)> = vec![
            ("type", "craft:save".into()),
            ("session", session.as_str().into()),
            ("name", name.as_str().into()),
            ("mime", mime_of(&name).into()),
            ("baseVersionId", base.as_str().into()),
            // Anything that is not the opened file under its own name (an export, a Save As) is a copy.
            ("saveAs", (checkpoint.is_none() && name != opened_name).into()),
            ("bytes", buffer.clone().into()),
        ];
        if let Some(c) = checkpoint {
            fields.push(("checkpoint", JsValue::from_f64(c as f64)));
        }
        let message = message(&fields).ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        if checkpoint.is_none() {
            self.shared.borrow_mut().saved_names.push(name);
        }
        Ok(())
    }

    /// Hear the host.
    fn listen(&self, inbox: Inbox, runs: HostRuns, ctx: egui::Context) {
        let bridge = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != bridge.origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &bridge.parent)) {
                return;
            }
            let data = event.data();
            let Some(kind) = string(&data, "type") else { return };
            let known = bridge.shared.borrow().opened.session.clone();
            match kind.as_str() {
                "craft:open" => {
                    let Some(session) = string(&data, "session").filter(|s| !s.is_empty()) else { return };
                    let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Book.xlsx".to_string());
                    let Some(bytes) = bytes(&data) else { return };
                    let opened = Opened {
                        session,
                        name: name.clone(),
                        version_id: string(&data, "versionId").unwrap_or_default(),
                        read_only: boolean(&data, "readOnly"),
                        collab: boolean(&data, "collab"),
                    };
                    {
                        let mut s = bridge.shared.borrow_mut();
                        s.opened = opened.clone();
                        s.events.push_back(Event::Opened(opened));
                    }
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                }
                "craft:saved" if string(&data, "session").as_deref() == Some(known.as_str()) => {
                    if let Some(version) = string(&data, "versionId") {
                        bridge.shared.borrow_mut().opened.version_id = version;
                    }
                }
                "craft:refused" if string(&data, "session").as_deref() == Some(known.as_str()) => {
                    let code = string(&data, "code").unwrap_or_default();
                    let reason = string(&data, "reason").unwrap_or_else(|| "refused".into());
                    bridge.shared.borrow_mut().events.push_back(Event::Refused { code, reason });
                }
                "craft:command" => {
                    let session = string(&data, "session").unwrap_or_default();
                    let id = js_sys::Reflect::get(&data, &JsValue::from_str("id")).ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let (current, read_only) = {
                        let s = bridge.shared.borrow();
                        (s.opened.session.clone(), s.opened.read_only)
                    };
                    if session.is_empty() || session != current {
                        bridge.post_result_for(&session, id, Err("session-mismatch: this command is for another open of the file".into()));
                    } else if read_only {
                        bridge.post_result_for(&session, id, Err("read-only: this file is open to view; the assistant cannot change it".into()));
                    } else {
                        let params = js_sys::Reflect::get(&data, &JsValue::from_str("params"))
                            .ok()
                            .and_then(|p| js_sys::JSON::stringify(&p).ok())
                            .and_then(|s| s.as_string())
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or(serde_json::Value::Null);
                        let run = HostRun { id: serde_json::json!(id), method: string(&data, "method").unwrap_or_default(), params };
                        if let Ok(mut q) = runs.lock() {
                            q.push(run);
                        }
                    }
                }
                other => {
                    let Some(rest) = other.strip_prefix("craft:crdt:") else { return };
                    if string(&data, "session").as_deref() != Some(known.as_str()) {
                        return;
                    }
                    let body = js_sys::JSON::stringify(&data).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null);
                    bridge.shared.borrow_mut().events.push_back(Event::Crdt { kind: rest.to_owned(), body });
                }
            }
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// The content type of a file name the editor saves.
fn mime_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("xlsx" | "xlsm") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("xltx" | "xltm") => "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
        Some("xlsb") => "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        Some("csv") => "text/csv",
        Some("tsv" | "txt") => "text/plain",
        Some("pdf") => "application/pdf",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

fn boolean(data: &JsValue, key: &str) -> bool {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
