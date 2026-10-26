//! The assistant's command surface for the OneOS host bridge (`craft:command {method: "command.batch"}`).
//!
//! The host relays an allow-listed run: `{label, steps: [{name, args?}]}`. The whole run is judged BEFORE anything
//! runs (an id outside the list refuses the run, `command-not-allowed`), every step runs through the same registry the
//! menus use (so a step the workbook's protection or state forbids is refused exactly as it would be for the person), and
//! the run is ONE undo step named by `label`: a step that fails takes back every step before it, so a run is all or
//! nothing. There is no file-path or dialog command in the list: files move only through the host.
//!
//! The list and the run live here, in the engine, so the browser window (`gridcraft_ui_egui`) and the node build the OneOS
//! tests drive (`gridcraft-collab-node`) answer a `command.batch` with the SAME function; each supplies only how one step
//! runs ([`BatchHost`]).
//!
//! This list is the one declared in the host's `platform-craft` registry as `sheets` commands (pinned equal there).

use serde_json::{Value, json};

use crate::Session;

/// Commands the assistant may run. Each acts at once on the workbook (no dialog, no file picker, no pointer gesture), and
/// none is Undo or Redo: undoing a run is the person's. The target of each is its own range, cell or sheet argument or,
/// when it has none, the selection.
pub const ASSISTANT_COMMANDS: &[&str] = &[
    // Content.
    "cell.set",
    "range.setValues",
    "range.fill",
    "edit.clearContents",
    "edit.clearFormats",
    "edit.fillDown",
    "edit.fillUp",
    "edit.fillLeft",
    "edit.fillRight",
    "data.sortAscending",
    "data.sortDescending",
    // Format.
    "home.bold",
    "home.italic",
    "home.strikethrough",
    "home.alignLeft",
    "home.alignCenter",
    "home.alignRight",
    "home.alignTop",
    "home.alignMiddle",
    "home.alignBottom",
    "home.wrapText",
    "home.fillColor",
    "home.fontName",
    "home.fontSize",
    "home.percent",
    "home.comma",
    "home.accounting",
    "home.increaseDecimal",
    "home.decreaseDecimal",
    "home.mergeCells",
    "home.unmergeCells",
    "home.autofitColumnWidth",
    "home.autofitRowHeight",
    // Structure.
    "home.insertColumns",
    "home.deleteColumns",
    "home.insertRows",
    "home.deleteRows",
    "home.hideColumns",
    "home.hideRows",
    "home.insertSheet",
    "home.deleteSheet",
    "sheet.rename",
    "sheet.tabColor",
    "view.freezeTopRow",
    "view.freezeFirstColumn",
    "view.unfreezePanes",
    // Notes.
    "review.newNote",
    // Named ranges (a name is defined, redefined by oldName, or deleted; a hidden name Sheets keeps its own data in is refused).
    "formulas.defineName",
    "formulas.deleteName",
    // Tables.
    "insert.table",
    // Charts: insert one (with a range, or the selection), then change its type, data, elements and place.
    "insert.chart",
    "chart.set",
    "chart.changeType",
    "chart.switchRowColumn",
    "chart.selectData",
    "chart.addElement",
    "chart.changeColors",
    "chart.quickLayout",
    "chart.move",
    "chart.delete",
    // PivotTables: insert one from a source range or table, shape its fields, then a PivotChart bound to it.
    "insert.pivotTable",
    "insert.pivotChart",
    "pivot.addField",
    "pivot.removeField",
    "pivot.moveField",
    "pivot.valueSettings",
    "pivot.filter",
    "pivot.sort",
    "pivot.group",
    "pivot.layout",
    "pivot.grandTotals",
    "pivot.subtotals",
    "pivot.reportLayout",
    "pivot.style",
    "pivot.calculatedField",
    "pivot.changeSource",
    "pivot.refresh",
    "pivot.delete",
];

/// Commands that only look or move the window (they change nothing a save would keep, or only what the person sees).
pub const ARGUMENT_COMMANDS: &[&str] = &[
    // Takes a range or a cell: where the commands without a target act.
    "selection.set",
    // Takes a sheet, by name or by index.
    "sheet.activate",
    // Read-only: a cell's value, formula, display text and style.
    "cell.get",
    // Read-only: the values of a range.
    "sheet.read",
    // Read-only: the workbook's sheets, selection, names, tables, charts, PivotTables and history.
    "document.inspect",
    // Read-only: the defined names with their values.
    "formulas.nameManager",
    // Read-only: chart types that suit a range.
    "insert.chartRecommendations",
    // Read-only: every PivotTable, and one PivotTable's source fields and layout.
    "pivot.list",
    "pivot.fields",
];

/// Every command id a run may name, in the order the host's registry declares them.
pub fn allowed() -> Vec<&'static str> {
    ASSISTANT_COMMANDS.iter().chain(ARGUMENT_COMMANDS).copied().collect()
}

const MAX_STEPS: usize = 50;
const MAX_LABEL: usize = 120;

/// What a window supplies to answer a `command.batch`: its session and how one step runs.
pub trait BatchHost {
    fn session(&mut self) -> &mut Session;
    /// Run one step as the person would. A step that needs a dialog (it must not sit over the person's work) closes it and
    /// answers an error.
    fn step(&mut self, name: &str, args: Value) -> Result<Value, String>;
    /// After the run (or its rollback): let the window take what the engine queued.
    fn settled(&mut self);
}

/// One `command.batch`: all of it, as ONE undo step, or none of it. Answers the result and, when the run changed the workbook,
/// the label it was made under.
pub fn run_batch(host: &mut impl BatchHost, params: &Value) -> Result<(Value, Option<String>), String> {
    let label = params.get("label").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if label.chars().count() > MAX_LABEL {
        return Err(format!("label-too-long: a run's label is at most {MAX_LABEL} characters"));
    }
    let steps = params.get("steps").and_then(Value::as_array).ok_or("run-empty: a run states its steps")?;
    if steps.is_empty() {
        return Err("run-empty: a run states at least one command".into());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!("run-too-long: a run is at most {MAX_STEPS} steps"));
    }
    // Judge the whole run first; nothing has run when one id is outside the list.
    let mut named = Vec::with_capacity(steps.len());
    for step in steps {
        let name = step.get("name").and_then(Value::as_str).unwrap_or("");
        if !ASSISTANT_COMMANDS.contains(&name) && !ARGUMENT_COMMANDS.contains(&name) {
            return Err(format!("command-not-allowed: Sheets does not allow the command '{name}' -- nothing was run"));
        }
        named.push((name, step.get("args").cloned().unwrap_or_else(|| json!({}))));
    }
    if host.session().active().is_none() {
        return Err("no-document: no workbook is open in this window".into());
    }
    let label = if label.is_empty() { "Assistant" } else { label };
    let token = host.session().group_begin(label).map_err(|e| format!("no-document: {e}"))?;
    let mut results = Vec::with_capacity(named.len());
    for (i, (name, args)) in named.iter().enumerate() {
        match host.step(name, args.clone()) {
            Ok(result) => results.push(json!({ "name": name, "result": result })),
            Err(e) => {
                let _ = host.session().group_rollback(token);
                host.settled();
                return Err(format!("step-failed: step {} ({name}): {e} -- nothing was changed", i + 1));
            }
        }
    }
    let changed = host.session().group_end(token).unwrap_or(false);
    host.settled();
    Ok((json!({ "steps": results, "edits": usize::from(changed) }), changed.then(|| label.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UiRequest;

    #[test]
    fn every_listed_command_is_a_real_one_and_none_reaches_a_file() {
        for id in allowed() {
            assert!(crate::find_command(id).is_some(), "{id} is not an engine command");
            assert!(!id.starts_with("file.") && !id.contains("open") && !id.contains("save") && !id.contains("export"), "{id} reaches a file");
        }
        assert!(!allowed().contains(&"edit.undo") && !allowed().contains(&"edit.redo"));
        let mut seen = std::collections::HashSet::new();
        assert!(allowed().iter().all(|id| seen.insert(*id)), "a command is listed twice");
    }

    struct Headless(Session);
    impl BatchHost for Headless {
        fn session(&mut self) -> &mut Session {
            &mut self.0
        }
        fn step(&mut self, name: &str, args: Value) -> Result<Value, String> {
            let r = self.0.run(name, args);
            if self.0.take_ui_requests().iter().any(|q| matches!(q, UiRequest::Dialog(..))) {
                return Err("it needed a dialog".into());
            }
            r
        }
        fn settled(&mut self) {}
    }

    fn run(h: &mut Headless, label: &str, steps: Value) -> Result<(Value, Option<String>), String> {
        run_batch(h, &json!({"label": label, "steps": steps}))
    }

    fn book() -> Headless {
        let mut s = Session::new();
        s.new_workbook();
        Headless(s)
    }

    /// A table, a named range, a PivotTable and a chart from one run; read back; ONE undo step takes all of it.
    #[test]
    fn a_run_builds_a_table_a_name_a_pivot_and_a_chart_as_one_undo_step() {
        let mut h = book();
        let (res, label) = run(
            &mut h,
            "Sales summary",
            json!([
                {"name": "range.setValues", "args": {"range": "A1", "values": [["Region", "Sales"], ["East", 10], ["West", 20], ["East", 5], ["West", 7]]}},
                {"name": "formulas.defineName", "args": {"name": "SalesData", "refersTo": "=Sheet1!$A$1:$B$5"}},
                {"name": "insert.pivotTable", "args": {"source": "Sheet1!A1:B5", "destination": "Sheet1!D1", "name": "ByRegion", "rows": ["Region"], "values": ["Sales"]}},
                {"name": "insert.pivotChart", "args": {"pivot": "ByRegion", "type": "column", "at": "H2"}},
                {"name": "insert.chart", "args": {"range": "A1:B5", "type": "bar", "at": "H20", "title": "Sales"}},
            ]),
        )
        .expect("the run");
        assert_eq!(label.as_deref(), Some("Sales summary"));
        assert_eq!(res["edits"], 1);
        let inspect = h.0.run("document.inspect", json!({})).unwrap();
        assert_eq!(inspect["names"][0]["name"], "SalesData");
        assert_eq!(inspect["sheets"][0]["charts"].as_array().unwrap().len(), 2);
        assert_eq!(inspect["sheets"][0]["pivots"][0]["name"], "ByRegion");
        let grid = h.0.run("sheet.read", json!({"range": "D1:E5"})).unwrap();
        assert_eq!(grid["values"][1], json!(["East", 15.0]));
        assert_eq!(grid["values"][2], json!(["West", 27.0]));
        assert_eq!(grid["values"][3], json!(["Grand Total", 42.0]));
        // One undo step takes the whole run back.
        h.0.run("edit.undo", json!({})).unwrap();
        let after = h.0.run("document.inspect", json!({})).unwrap();
        assert_eq!(after["names"].as_array().unwrap().len(), 0);
        assert_eq!(after["sheets"][0]["charts"].as_array().unwrap().len(), 0);
        assert_eq!(after["sheets"][0]["pivots"].as_array().unwrap().len(), 0);
        assert_eq!(after["sheets"][0]["cells"], 0);
    }

    #[test]
    fn a_failing_step_takes_back_the_steps_before_it() {
        let mut h = book();
        let e = run(
            &mut h,
            "x",
            json!([
                {"name": "range.setValues", "args": {"range": "A1", "values": [["a", "b"], [1, 2]]}},
                {"name": "formulas.defineName", "args": {"name": "Ok", "refersTo": "=Sheet1!$A$1"}},
                {"name": "insert.pivotTable", "args": {"source": "Nowhere!A1:B2", "rows": ["a"]}},
            ]),
        )
        .unwrap_err();
        assert!(e.starts_with("step-failed: step 3 (insert.pivotTable)"), "{e}");
        let after = h.0.run("document.inspect", json!({})).unwrap();
        assert_eq!(after["names"].as_array().unwrap().len(), 0);
        assert_eq!(after["sheets"][0]["cells"], 0);
    }

    #[test]
    fn a_step_that_needs_a_dialog_is_refused_and_the_run_is_taken_back() {
        let mut h = book();
        let e = run(&mut h, "x", json!([{"name": "cell.set", "args": {"cell": "A1", "input": "1"}}, {"name": "pivot.calculatedField", "args": {}}])).unwrap_err();
        assert!(e.starts_with("step-failed: step 2 (pivot.calculatedField)"), "{e}");
        assert_eq!(h.0.run("sheet.read", json!({})).unwrap()["values"], json!([]));
    }

    #[test]
    fn an_id_outside_the_list_refuses_the_whole_run_before_anything_runs() {
        let mut h = book();
        for bad in ["file.save", "edit.undo", "insert.slicer"] {
            let e = run(&mut h, "x", json!([{"name": "cell.set", "args": {"cell": "A1", "input": "1"}}, {"name": bad}])).unwrap_err();
            assert!(e.starts_with("command-not-allowed"), "{e}");
        }
        assert_eq!(h.0.run("sheet.read", json!({})).unwrap()["values"], json!([]));
    }
}
