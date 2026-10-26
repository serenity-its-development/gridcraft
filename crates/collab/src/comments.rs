//! Cell comments, through the comments tier the host already holds (design 12.5, 12.9 Q2).
//!
//! The host owns comments: each is a map in the file's sibling comments room, `{by, name, at, text, resolved, parent?,
//! anchor?}`, stamped with the signed-in party (a frame cannot claim an author). This shim does not put comments in the
//! workbook's tree. It only translates:
//!
//! * the editor's cell comments (`Sheet::comments`: a note or a thread of replies per cell) become `craft:crdt:comment`
//!   messages ([`CommentMsg`]): add, reply, edit, resolve, reopen, delete;
//! * the host's `comments` map (in `craft:crdt:state`, and in a delta whose path is `["comments"]`) becomes cell comments in the
//!   editor ([`RemoteComment`]).
//!
//! A comment's anchor is `{sh: <sheet id>, cell: "<row id>:<column id>"}`, so it follows its cell when rows or columns are
//! inserted or deleted; a thread is the top comment (no `parent`) and its replies (`parent` = the top comment's host id).

use std::collections::{BTreeMap, HashMap, HashSet};

use gridcraft_model::Comment;
use serde_json::{Value, json};

/// One `craft:crdt:comment` message (without the session).
#[derive(Debug, Clone, PartialEq)]
pub enum CommentMsg {
    Add { text: String, anchor: Value },
    Reply { parent: String, text: String },
    Edit { id: String, text: String },
    Resolve { id: String },
    Reopen { id: String },
    Delete { id: String },
}

impl CommentMsg {
    /// The frame message body: `{action, id?, text?, anchor?}`.
    #[must_use]
    pub fn body(&self) -> Value {
        match self {
            CommentMsg::Add { text, anchor } => json!({"action": "add", "text": text, "anchor": anchor}),
            CommentMsg::Reply { parent, text } => json!({"action": "reply", "id": parent, "text": text}),
            CommentMsg::Edit { id, text } => json!({"action": "edit", "id": id, "text": text}),
            CommentMsg::Resolve { id } => json!({"action": "resolve", "id": id}),
            CommentMsg::Reopen { id } => json!({"action": "reopen", "id": id}),
            CommentMsg::Delete { id } => json!({"action": "delete", "id": id}),
        }
    }
}

/// A thread the host holds that the editor should show on a cell.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteComment {
    /// `<sheet id>|<row id>:<column id>`.
    pub key: String,
    pub author: String,
    pub text: String,
    pub resolved: bool,
    /// `(author, text)` of each reply, oldest first.
    pub replies: Vec<(String, String)>,
    /// ISO 8601, UTC, when the thread started.
    pub date: String,
}

impl RemoteComment {
    /// The editor's comment for it.
    #[must_use]
    pub fn to_comment(&self) -> Comment {
        Comment {
            author: self.author.clone(),
            text: self.text.clone(),
            replies: self.replies.clone(),
            threaded: true,
            resolved: self.resolved,
            visible: false,
            dates: vec![self.date.clone()],
            note_box: None,
        }
    }
}

/// What the host's comments changed in the editor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommentsUpdate {
    pub upsert: Vec<RemoteComment>,
    /// Keys of threads that are gone.
    pub remove: Vec<String>,
}

impl CommentsUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.remove.is_empty()
    }
}

/// What both sides agree a thread is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Known {
    text: String,
    resolved: bool,
    replies: Vec<String>,
}

/// The key of a thread: its sheet and cell, by ids.
#[must_use]
pub fn key_of(sheet_id: &str, cell: &str) -> String {
    format!("{sheet_id}|{cell}")
}

/// A millisecond Unix time as ISO 8601 UTC (`2026-10-26T01:02:03Z`).
#[must_use]
pub fn iso_of(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The comment sync state of one window.
#[derive(Debug, Default)]
pub struct CommentSync {
    /// Threads both sides hold, as last agreed.
    known: BTreeMap<String, Known>,
    /// The host's id of each thread.
    host_of: HashMap<String, String>,
    /// Threads deleted here before the host told us their id: deleted once it does.
    deleted_unknown: HashSet<String>,
}

impl CommentSync {
    /// The messages that take the host's threads to the editor's. `threads`: every cell comment of the editor with its key and anchor.
    pub fn local(&mut self, threads: &[(String, Value, &Comment)]) -> Vec<CommentMsg> {
        let mut out = Vec::new();
        let present: HashSet<&str> = threads.iter().map(|(k, _, _)| k.as_str()).collect();
        let gone: Vec<String> = self.known.keys().filter(|k| !present.contains(k.as_str())).cloned().collect();
        for key in gone {
            self.known.remove(&key);
            match self.host_of.get(&key) {
                Some(id) => out.push(CommentMsg::Delete { id: id.clone() }),
                None => {
                    self.deleted_unknown.insert(key);
                }
            }
        }
        for (key, anchor, c) in threads {
            let want = Known { text: c.text.clone(), resolved: c.resolved, replies: c.replies.iter().map(|(_, t)| t.clone()).collect() };
            match self.known.get(key).cloned() {
                None => {
                    if want.text.trim().is_empty() {
                        // The host takes no empty comment: it is sent when it has words.
                        continue;
                    }
                    out.push(CommentMsg::Add { text: want.text.clone(), anchor: anchor.clone() });
                    // The replies follow once the host names the thread.
                    self.known.insert(key.clone(), Known { replies: Vec::new(), ..want });
                }
                Some(have) if have != want => {
                    let Some(id) = self.host_of.get(key).cloned() else { continue };
                    let mut now = have.clone();
                    if have.text != want.text && !want.text.trim().is_empty() {
                        out.push(CommentMsg::Edit { id: id.clone(), text: want.text.clone() });
                        now.text = want.text.clone();
                    }
                    if have.resolved != want.resolved {
                        out.push(if want.resolved { CommentMsg::Resolve { id: id.clone() } } else { CommentMsg::Reopen { id: id.clone() } });
                        now.resolved = want.resolved;
                    }
                    for reply in want.replies.iter().skip(have.replies.len()) {
                        if !reply.trim().is_empty() {
                            out.push(CommentMsg::Reply { parent: id.clone(), text: reply.clone() });
                        }
                    }
                    if want.replies.len() > have.replies.len() {
                        now.replies = want.replies.clone();
                    }
                    self.known.insert(key.clone(), now);
                }
                Some(_) => {}
            }
        }
        out
    }

    /// The host's `comments` map arrived: what the editor should show, and any delete that was waiting on an id.
    pub fn remote(&mut self, host: &Value) -> (CommentsUpdate, Vec<CommentMsg>) {
        let mut update = CommentsUpdate::default();
        let mut deletes = Vec::new();
        let Some(map) = host.as_object() else { return (update, deletes) };
        // Threads: top comments with a cell anchor, and their replies in time order.
        struct Top<'a> {
            id: &'a str,
            key: String,
            c: &'a Value,
        }
        let mut tops: Vec<Top<'_>> = Vec::new();
        let mut replies: HashMap<&str, Vec<(u64, &str, &Value)>> = HashMap::new();
        for (id, c) in map {
            match c.get("parent").and_then(Value::as_str) {
                Some(parent) => replies.entry(parent).or_default().push((c.get("at").and_then(Value::as_u64).unwrap_or(0), id.as_str(), c)),
                None => {
                    let anchor = c.get("anchor");
                    let sh = anchor.and_then(|a| a.get("sh")).and_then(Value::as_str);
                    let cell = anchor.and_then(|a| a.get("cell")).and_then(Value::as_str);
                    if let (Some(sh), Some(cell)) = (sh, cell) {
                        tops.push(Top { id: id.as_str(), key: key_of(sh, cell), c });
                    }
                }
            }
        }
        for t in &tops {
            self.host_of.insert(t.key.clone(), t.id.to_owned());
        }
        let live: HashSet<&str> = tops.iter().map(|t| t.key.as_str()).collect();
        for t in &tops {
            if self.deleted_unknown.remove(&t.key) {
                deletes.push(CommentMsg::Delete { id: t.id.to_owned() });
                continue;
            }
            let text_of = |c: &Value| c.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let name_of = |c: &Value| c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Someone").to_owned();
            let mut kids = replies.get(t.id).cloned().unwrap_or_default();
            kids.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
            let reply_pairs: Vec<(String, String)> = kids.iter().map(|(_, _, c)| (name_of(c), text_of(c))).collect();
            let want = Known {
                text: text_of(t.c),
                resolved: t.c.get("resolved").and_then(Value::as_bool) == Some(true),
                replies: reply_pairs.iter().map(|(_, text)| text.clone()).collect(),
            };
            if self.known.get(&t.key) == Some(&want) {
                continue;
            }
            self.known.insert(t.key.clone(), want.clone());
            update.upsert.push(RemoteComment {
                key: t.key.clone(),
                author: name_of(t.c),
                text: want.text,
                resolved: want.resolved,
                replies: reply_pairs,
                date: iso_of(t.c.get("at").and_then(Value::as_u64).unwrap_or(0)),
            });
        }
        // Gone from the host.
        let removed: Vec<String> = self.known.keys().filter(|k| !live.contains(k.as_str()) && self.host_of.contains_key(*k)).cloned().collect();
        for key in removed {
            self.known.remove(&key);
            self.host_of.remove(&key);
            update.remove.push(key);
        }
        (update, deletes)
    }
}
