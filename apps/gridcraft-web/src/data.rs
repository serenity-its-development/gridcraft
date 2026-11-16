//! The workspace's data, asked for by `WKS_` functions (`gridcraft_calc::held`): the messages the frame sends and hears.
//!
//! - `craft:open {…, wks: [names]}` (from the parent): the `WKS_` functions this workspace answers besides `WKS_QUERY`.
//!   None listed, or no `wks`: only `WKS_QUERY`. Nothing is listed here: the host names them.
//! - `craft:data:ask {session, id, question}` (to the parent): `WKS_QUERY(text, [page], [after])` asks
//!   `{tool: "data.select", text, page, after?}`; another `WKS_` function asks `{tool: "fn", name, args}`.
//! - `craft:data:answer {session, id, ok: true, columns, rows, complete, cursor?, fresh?}` or `{…, ok: false, code, reason}`
//!   (from the parent): the answer (`fresh`: a newer one for a question already answered) or the refusal.

use gridcraft_engine::calc::{Answer, Question};
use gridcraft_engine::core::{Array, Value};
use serde_json::{Value as Json, json};

/// An id the host echoes (the relay's bound).
const MAX_ID: usize = 64;
/// Most names `craft:open` lists, and longest name.
const MAX_NAMES: usize = 512;
const MAX_NAME: usize = 64;
/// Most columns, rows and values an answer is taken with (past them it is refused here).
const MAX_COLS: usize = 256;
const MAX_ROWS: usize = 10_000;
const MAX_VALUES: usize = 200_000;
/// Longest text of one value.
const MAX_TEXT: usize = 32_767;

/// The id a question of document `doc` goes out under: each document numbers its own questions, the frame tells them apart.
pub fn wire_id(doc: u64, id: &str) -> String {
    format!("d{doc}.{id}")
}

/// The document and its question id from an id [`wire_id`] made.
pub fn split_id(wire: &str) -> Option<(u64, &str)> {
    let (doc, id) = wire.strip_prefix('d')?.split_once('.')?;
    if id.is_empty() {
        return None;
    }
    Some((doc.parse().ok()?, id))
}

/// The `WKS_` function names `craft:open` lists (`wks`), upper case; anything else is ignored.
pub fn names_of(open: &Json) -> Vec<String> {
    let Some(list) = open.get("wks").and_then(Json::as_array) else { return vec![] };
    list.iter()
        .filter_map(Json::as_str)
        .filter(|n| n.len() <= MAX_NAME && n.len() > 4 && n[..4].eq_ignore_ascii_case("WKS_"))
        .map(str::to_ascii_uppercase)
        .take(MAX_NAMES)
        .collect()
}

/// `craft:data:ask` for question `id` on `session`.
pub fn ask_message(session: &str, id: &str, q: &Question) -> Json {
    let question = match q {
        Question::Select { text, page, after } => {
            let mut q = json!({ "tool": "data.select", "text": text, "page": page });
            if let Some(a) = after {
                q["after"] = json!(a);
            }
            q
        }
        Question::Fn { name, args } => json!({ "tool": "fn", "name": name, "args": args.iter().map(to_json).collect::<Vec<_>>() }),
    };
    json!({ "type": "craft:data:ask", "session": session, "id": id, "question": question })
}

fn to_json(v: &Value) -> Json {
    match v {
        Value::Empty => Json::Null,
        Value::Number(n) => json!(n),
        Value::Text(t) => json!(t.as_ref()),
        Value::Bool(b) => json!(b),
        Value::Error(e) => json!({ "error": e.as_str() }),
        Value::Array(a) => {
            Json::Array((0..a.rows).map(|r| Json::Array((0..a.cols).map(|c| a.get(r, c).map_or(Json::Null, to_json)).collect())).collect())
        }
    }
}

fn from_json(v: &Json) -> Value {
    match v {
        Json::Null => Value::Empty,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n.as_f64().map_or(Value::Empty, Value::number),
        Json::String(s) => Value::text(clip(s)),
        other => Value::text(clip(&other.to_string())),
    }
}

fn clip(s: &str) -> &str {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// A `craft:data:answer` on `session`: the question's id and its answer. `None` when it is not one, or is for another
/// session. The value of an answer depends on what was asked (`question`, from the id): `WKS_QUERY` spills its columns'
/// names over its rows; another function's single value is that value, and its rows otherwise.
pub fn answer_of(msg: &Json, session: &str, question: impl FnOnce(&str) -> Option<Question>) -> Option<(String, Answer)> {
    if msg.get("type")?.as_str()? != "craft:data:answer" || session.is_empty() || msg.get("session")?.as_str()? != session {
        return None;
    }
    let id = msg.get("id")?.as_str().filter(|id| !id.is_empty() && id.len() <= MAX_ID)?.to_owned();
    if msg.get("ok").and_then(Json::as_bool) != Some(true) {
        let reason = msg.get("reason").and_then(Json::as_str).map(clip).filter(|r| !r.trim().is_empty()).unwrap_or("refused");
        let answer = match msg.get("code").and_then(Json::as_str).filter(|c| !c.is_empty()) {
            Some(code) => format!("{reason} ({})", clip(code)),
            None => reason.to_owned(),
        };
        return Some((id, Err(answer)));
    }
    let header = matches!(question(&id)?, Question::Select { .. });
    let columns: Vec<Value> = msg.get("columns").and_then(Json::as_array).map(|c| c.iter().map(from_json).collect()).unwrap_or_default();
    let rows = msg.get("rows").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
    let width = columns.len().max(rows.iter().map(|r| r.as_array().map_or(1, Vec::len)).max().unwrap_or(0));
    let height = rows.len() + usize::from(header);
    if width > MAX_COLS || rows.len() > MAX_ROWS || width.saturating_mul(height) > MAX_VALUES {
        return Some((id, Err("the answer is larger than a sheet takes".into())));
    }
    if width == 0 {
        return Some((id, Ok(Value::Empty)));
    }
    let mut data = Vec::with_capacity(width * height);
    if header {
        data.extend(columns.iter().cloned().chain(std::iter::repeat(Value::Empty)).take(width));
    }
    for row in rows {
        let cells: Vec<Value> = match row.as_array() {
            Some(r) => r.iter().map(from_json).collect(),
            None => vec![from_json(row)],
        };
        data.extend(cells.into_iter().chain(std::iter::repeat(Value::Empty)).take(width));
    }
    let value = match (height, width) {
        (0, _) => Value::Empty,
        (1, 1) => data.pop().unwrap_or_default(),
        (h, w) => Array::new(h, w, data).map_or(Value::Empty, Value::from),
    };
    Some((id, Ok(value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_engine::core::CellError;

    fn select() -> Option<Question> {
        Some(Question::Select { text: "hours".into(), page: 200, after: None })
    }

    #[test]
    fn asks_say_the_question() {
        let q = Question::Select { text: "open tickets".into(), page: 50, after: Some("c1".into()) };
        assert_eq!(
            ask_message("s1", "wks-1", &q),
            json!({"type": "craft:data:ask", "session": "s1", "id": "wks-1", "question": {"tool": "data.select", "text": "open tickets", "page": 50, "after": "c1"}})
        );
        let q = Question::Fn { name: "WKS_RATE".into(), args: vec![Value::text("AUD"), Value::Number(2.0), Value::Empty] };
        assert_eq!(ask_message("s1", "wks-2", &q)["question"], json!({"tool": "fn", "name": "WKS_RATE", "args": ["AUD", 2.0, null]}));
    }

    #[test]
    fn a_query_answer_spills_its_columns_over_its_rows() {
        let msg = json!({"type": "craft:data:answer", "session": "s1", "id": "wks-1", "ok": true, "columns": ["name", "hours"], "rows": [["Ana", 3], ["Bo"]], "complete": true});
        let (id, a) = answer_of(&msg, "s1", |_| select()).unwrap();
        assert_eq!(id, "wks-1");
        let Ok(Value::Array(arr)) = a else { panic!("{a:?}") };
        assert_eq!((arr.rows, arr.cols), (3, 2));
        assert_eq!(arr.get(0, 1), Some(&Value::text("hours")));
        assert_eq!(arr.get(1, 1), Some(&Value::Number(3.0)));
        assert_eq!(arr.get(2, 1), Some(&Value::Empty));
        // Another session's answer, an unknown id, a long id: not taken.
        assert!(answer_of(&msg, "s2", |_| select()).is_none());
        assert!(answer_of(&msg, "s1", |_| None).is_none());
        let mut long = msg.clone();
        long["id"] = json!("x".repeat(65));
        assert!(answer_of(&long, "s1", |_| select()).is_none());
    }

    #[test]
    fn a_function_answer_of_one_value_is_that_value() {
        let msg = json!({"type": "craft:data:answer", "session": "s1", "id": "wks-2", "ok": true, "columns": ["rate"], "rows": [[0.66]], "complete": true, "fresh": true});
        let fun = || Some(Question::Fn { name: "WKS_RATE".into(), args: vec![] });
        assert_eq!(answer_of(&msg, "s1", |_| fun()).unwrap().1, Ok(Value::Number(0.66)));
    }

    #[test]
    fn a_refusal_carries_its_reason_and_code() {
        let msg = json!({"type": "craft:data:answer", "session": "s1", "id": "wks-3", "ok": false, "code": "craft-data-tool-not-allowed", "reason": "only data.select is asked"});
        assert_eq!(answer_of(&msg, "s1", |_| None).unwrap().1, Err("only data.select is asked (craft-data-tool-not-allowed)".into()));
        let _ = CellError::Refused;
    }

    #[test]
    fn ids_name_their_document() {
        assert_eq!(split_id(&wire_id(7, "wks-3")), Some((7, "wks-3")));
        assert_eq!(split_id("wks-3"), None);
        assert_eq!(split_id("d7."), None);
        assert_eq!(split_id("dx.wks-1"), None);
    }

    #[test]
    fn names_come_from_the_open() {
        let open = json!({"wks": ["wks_rate", "SUM", 3, "WKS_", "WKS_TOTAL"]});
        assert_eq!(names_of(&open), vec!["WKS_RATE".to_string(), "WKS_TOTAL".to_string()]);
        assert!(names_of(&json!({})).is_empty());
    }
}
