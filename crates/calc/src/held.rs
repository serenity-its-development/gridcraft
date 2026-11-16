//! Values held by the workspace: `WKS_QUERY(text, [page], [after])` and the other `WKS_`
//! functions the host names when it opens the document. A cell asks a question; the host answers
//! it later (or refuses it). Until then the cell shows `#GETTING_DATA`; a refusal shows `#REFUSED`
//! with its reason; an answer recalculates the cells that asked and everything that reads them.
//!
//! Each question is asked once, keyed by its canonical form: other cells asking the same one
//! join it. A question no cell asks any more is dropped, so its late answer is ignored.

use std::collections::{BTreeSet, HashMap};

use gridcraft_core::{CellError, Value};
use gridcraft_formula::Expr;

use crate::recalc::Key;

/// The question `WKS_QUERY` asks.
pub const QUERY: &str = "WKS_QUERY";
/// Longest question text (the relay's bound).
pub const MAX_TEXT: usize = 4000;
/// Rows a page holds at most (and by default).
pub const MAX_PAGE: u32 = 200;
/// Longest continuation cursor.
pub const MAX_AFTER: usize = 8192;
/// Arguments a `WKS_` function takes at most, and values in them all.
pub const MAX_ARGS: usize = 32;
pub const MAX_ARG_VALUES: usize = 10_000;
/// Questions open or answered at once; past it a cell gives `#VALUE!` instead of asking.
pub const MAX_ENTRIES: usize = 256;
/// Function names the host may list.
pub const MAX_NAMES: usize = 512;
pub const MAX_NAME: usize = 64;

/// What a cell asks the workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Question {
    /// `WKS_QUERY`: rows selected by a question in words.
    Select { text: String, page: u32, after: Option<String> },
    /// Another `WKS_` function, by its name, with its argument values (scalars or arrays).
    Fn { name: String, args: Vec<Value> },
}

impl Question {
    /// The question a call of `name` (upper case) with these argument values asks. An error in
    /// an argument is the call's result.
    pub fn of(name: &str, args: &[Value]) -> Result<Question, CellError> {
        for a in args {
            first_error(a)?;
        }
        if name == QUERY {
            let (text, page, after) = match args {
                [t] => (t, None, None),
                [t, p] => (t, Some(p), None),
                [t, p, a] => (t, Some(p), Some(a)),
                _ => return Err(CellError::Value),
            };
            let text = match text {
                Value::Text(t) if !t.trim().is_empty() && t.chars().count() <= MAX_TEXT => t.trim().to_string(),
                _ => return Err(CellError::Value),
            };
            let page = match page {
                None | Some(Value::Empty) => MAX_PAGE,
                Some(Value::Number(n)) if n.fract() == 0.0 && *n >= 1.0 && *n <= f64::from(MAX_PAGE) => *n as u32,
                Some(_) => return Err(CellError::Value),
            };
            let after = match after {
                None | Some(Value::Empty) => None,
                Some(Value::Text(a)) if a.is_empty() => None,
                Some(Value::Text(a)) if a.len() <= MAX_AFTER => Some(a.to_string()),
                Some(_) => return Err(CellError::Value),
            };
            return Ok(Question::Select { text, page, after });
        }
        let values: usize = args.iter().map(|a| if let Value::Array(x) = a { x.data.len() } else { 1 }).sum();
        if args.len() > MAX_ARGS || values > MAX_ARG_VALUES {
            return Err(CellError::Value);
        }
        Ok(Question::Fn { name: name.to_string(), args: args.to_vec() })
    }

    /// The canonical form: two questions with the same key are the same question.
    pub fn key(&self) -> String {
        let mut k = String::new();
        match self {
            Question::Select { text, page, after } => {
                k.push_str("select|");
                part(&mut k, text);
                part(&mut k, &page.to_string());
                if let Some(a) = after {
                    part(&mut k, a);
                }
            }
            Question::Fn { name, args } => {
                k.push_str("fn|");
                part(&mut k, name);
                for a in args {
                    value_key(&mut k, a);
                }
            }
        }
        k
    }
}

fn first_error(v: &Value) -> Result<(), CellError> {
    match v {
        Value::Error(e) => Err(*e),
        Value::Array(a) => a.data.iter().try_for_each(first_error),
        _ => Ok(()),
    }
}

/// A length-prefixed part, so no text can run into the next one.
fn part(k: &mut String, s: &str) {
    k.push_str(&s.len().to_string());
    k.push(':');
    k.push_str(s);
}

fn value_key(k: &mut String, v: &Value) {
    match v {
        Value::Empty => k.push('e'),
        Value::Number(n) => {
            k.push('n');
            part(k, &format!("{:x}", n.to_bits()));
        }
        Value::Text(t) => {
            k.push('t');
            part(k, t);
        }
        Value::Bool(b) => k.push(if *b { 'T' } else { 'F' }),
        Value::Error(e) => {
            k.push('x');
            part(k, e.as_str());
        }
        Value::Array(a) => {
            k.push('a');
            part(k, &format!("{}x{}", a.rows, a.cols));
            for x in &a.data {
                value_key(k, x);
            }
        }
    }
}

/// Whether a formula calls a `WKS_` function (so what it gives is held by the workspace).
pub fn asks_held(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Call(n, _) = x
            && is_wks(n)
        {
            found = true;
        }
    });
    found
}

pub fn is_wks(name: &str) -> bool {
    name.len() > 4 && name.as_bytes()[..4].eq_ignore_ascii_case(b"WKS_")
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Waiting,
    Answered(Value),
    Refused(String),
}

#[derive(Clone, Debug)]
struct Entry {
    id: String,
    question: Question,
    state: State,
}

/// An answer from the host: the value (a spilled array, or one value) or the reason it refused.
pub type Answer = Result<Value, String>;

/// The questions cells have asked, keyed by their canonical form, and their answers.
#[derive(Clone, Debug, Default)]
pub struct HeldStore {
    /// Whether the host answers questions at all (it named its functions when it opened the
    /// document). Outside a workspace every `WKS_` name is unknown (`#NAME?`).
    open: bool,
    /// The `WKS_` functions (other than `WKS_QUERY`) the host answers, upper case.
    names: BTreeSet<String>,
    entries: HashMap<String, Entry>,
    /// Question id → its key.
    by_id: HashMap<String, String>,
    /// The questions each cell asked when it was last calculated.
    cells: HashMap<Key, BTreeSet<String>>,
    /// Asked during the run in progress: cell → question keys.
    pass: HashMap<Key, BTreeSet<String>>,
    /// Questions not yet sent: their keys, in the order asked.
    outbox: Vec<String>,
    /// Ids are never reused, so an answer to a dropped question can't land on a new one.
    next: u64,
}

impl HeldStore {
    /// The `WKS_` function names the host answers (from the document's open). Names that are
    /// not `WKS_` names, too long, or past the limit are ignored. Returns whether they changed.
    pub fn set_names<S: AsRef<str>>(&mut self, names: &[S]) -> bool {
        let names: BTreeSet<String> = names
            .iter()
            .map(|n| n.as_ref().to_ascii_uppercase())
            .filter(|n| is_wks(n) && n.len() <= MAX_NAME && n != QUERY && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
            .take(MAX_NAMES)
            .collect();
        let changed = !self.open || names != self.names;
        self.open = true;
        self.names = names;
        changed
    }

    /// Whether `name` (upper case) is a function the host answers.
    pub fn knows(&self, name: &str) -> bool {
        self.open && (name == QUERY || self.names.contains(name))
    }

    /// The value a cell's question gives now, asking it if no cell has.
    pub fn ask(&mut self, cell: Key, q: Question) -> Value {
        let key = q.key();
        if !self.entries.contains_key(&key) {
            if self.entries.len() >= MAX_ENTRIES {
                return Value::Error(CellError::Value);
            }
            self.next += 1;
            let id = format!("wks-{}", self.next);
            self.by_id.insert(id.clone(), key.clone());
            self.outbox.push(key.clone());
            self.entries.insert(key.clone(), Entry { id, question: q, state: State::Waiting });
        }
        self.pass.entry(cell).or_default().insert(key.clone());
        match self.entries.get(&key).map(|e| &e.state) {
            Some(State::Answered(v)) => v.clone(),
            Some(State::Refused(_)) => Value::Error(CellError::Refused),
            _ => Value::Error(CellError::GettingData),
        }
    }

    /// After a run: the cells it calculated now ask only what they asked in it. Questions no
    /// cell asks are dropped.
    pub fn settle(&mut self, calculated: impl IntoIterator<Item = Key>) {
        let mut changed = false;
        // A cell calculated in several rounds of the run is settled once.
        let calculated: BTreeSet<Key> = calculated.into_iter().collect();
        for k in calculated {
            match self.pass.remove(&k) {
                Some(qs) => {
                    changed |= self.cells.get(&k) != Some(&qs);
                    self.cells.insert(k, qs);
                }
                None => changed |= self.cells.remove(&k).is_some(),
            }
        }
        self.pass.clear();
        if changed {
            self.drop_unasked();
        }
    }

    /// A cell no longer has a formula.
    pub fn forget_cell(&mut self, cell: Key) {
        if self.cells.remove(&cell).is_some() {
            self.drop_unasked();
        }
    }

    fn drop_unasked(&mut self) {
        let asked: BTreeSet<&String> = self.cells.values().flatten().collect();
        let gone: Vec<String> = self.entries.keys().filter(|k| !asked.contains(k)).cloned().collect();
        for k in gone {
            if let Some(e) = self.entries.remove(&k) {
                self.by_id.remove(&e.id);
            }
        }
        let entries = &self.entries;
        self.outbox.retain(|k| entries.contains_key(k));
    }

    /// The questions to send to the host, each once: (id, question).
    pub fn take_asks(&mut self) -> Vec<(String, Question)> {
        std::mem::take(&mut self.outbox).into_iter().filter_map(|k| self.entries.get(&k).map(|e| (e.id.clone(), e.question.clone()))).collect()
    }

    /// The host's answer to question `id` (a later answer replaces an earlier one). Returns the
    /// cells that asked it, to recalculate; none when no cell asks it any more.
    pub fn answer(&mut self, id: &str, answer: Answer) -> Vec<Key> {
        let Some(key) = self.by_id.get(id) else { return vec![] };
        let Some(e) = self.entries.get_mut(key) else { return vec![] };
        let state = match answer {
            Ok(v) => State::Answered(v),
            Err(reason) => State::Refused(reason),
        };
        if e.state == state {
            return vec![];
        }
        e.state = state;
        let key = key.clone();
        let mut cells: Vec<Key> = self.cells.iter().filter(|(_, qs)| qs.contains(&key)).map(|(k, _)| *k).collect();
        cells.sort_by_key(|(s, c)| (*s, c.row, c.col));
        cells
    }

    /// Drops every question and the names (the document closed or its session ended): late
    /// answers are ignored, and the cells ask again once a host names its functions again.
    pub fn clear(&mut self) {
        self.open = false;
        self.names.clear();
        self.entries.clear();
        self.by_id.clear();
        self.outbox.clear();
        self.pass.clear();
        self.cells.clear();
    }

    /// Drops one question (cancelled): its late answer is ignored. Returns the cells that asked it.
    pub fn cancel(&mut self, id: &str) -> Vec<Key> {
        let Some(key) = self.by_id.remove(id) else { return vec![] };
        self.entries.remove(&key);
        self.outbox.retain(|k| *k != key);
        let mut cells: Vec<Key> = self.cells.iter().filter(|(_, qs)| qs.contains(&key)).map(|(k, _)| *k).collect();
        for k in &cells {
            if let Some(qs) = self.cells.get_mut(k) {
                qs.remove(&key);
            }
        }
        cells.sort_by_key(|(s, c)| (*s, c.row, c.col));
        cells
    }

    /// Whether question `id` is held (asked by some cell, not dropped).
    pub fn is_known(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    /// What question `id` asks, while it is held.
    pub fn question(&self, id: &str) -> Option<&Question> {
        self.entries.get(self.by_id.get(id)?).map(|e| &e.question)
    }

    /// Why the host refused what the cell asked, if it did.
    pub fn reason_at(&self, cell: Key) -> Option<&str> {
        self.cells.get(&cell)?.iter().find_map(|k| match &self.entries.get(k)?.state {
            State::Refused(r) => Some(r.as_str()),
            _ => None,
        })
    }

    /// Questions waiting for an answer.
    pub fn waiting(&self) -> usize {
        self.entries.values().filter(|e| e.state == State::Waiting).count()
    }

    /// Questions held (asked by some cell).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
