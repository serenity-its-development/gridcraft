//! The assistant's command surface for the OneOS host bridge (`craft:command {method: "command.batch"}`).
//!
//! The host relays an allow-listed run: `{label, steps: [{name, args?}]}`. The whole run is judged BEFORE anything
//! runs (an id outside the list refuses the run, `command-not-allowed`), every step runs through the same registry the
//! menus use (so a step the workbook's protection or state forbids is refused exactly as it would be for the person), and
//! the run is ONE undo step named by `label`: a step that fails takes back every step before it, so a run is all or
//! nothing. There is no file-path or dialog command in the list: files move only through the host. While the workbook is
//! live, the run's edits reach the room as ONE change labelled by `label` (`SheetApp::host_label`).
//!
//! This list is the one declared in the host's `platform-craft` registry as `sheets` commands (pinned equal there).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::SheetApp;

/// Commands the assistant may run. Each acts at once on the workbook (no dialog, no file picker, no pointer gesture), and
/// none is Undo or Redo: undoing a run is the person's. The target of each is its own `range`/`cell`/`sheet` argument or,
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
];

/// Commands that only look or move the window (they change nothing a save would keep, or only what the person sees).
pub const ARGUMENT_COMMANDS: &[&str] = &[
    // {range: "B2:D9"} or {cell}: where the commands without a target act.
    "selection.set",
    // {sheet: "Sales"} (a name or an index).
    "sheet.activate",
    // Read-only: a cell's value, formula, display text and style.
    "cell.get",
    // Read-only: the values of a range.
    "sheet.read",
    // Read-only: the workbook's sheets, selection, names, tables, charts and history.
    "document.inspect",
];

/// Every command id a run may name, in the order the host's registry declares them.
pub fn allowed() -> Vec<&'static str> {
    ASSISTANT_COMMANDS.iter().chain(ARGUMENT_COMMANDS).copied().collect()
}

const MAX_STEPS: usize = 50;
const MAX_LABEL: usize = 120;

/// One relayed run: the host's request id and its `command.batch` params.
pub struct HostRun {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// Runs waiting for the next frame (the bridge's listener pushes, the app drains).
pub type HostRuns = Arc<Mutex<Vec<HostRun>>>;

/// Answers a run: the request id and the outcome (`craft:result {ok, result|error}`).
pub type HostAnswerFn = Box<dyn Fn(&Value, Result<Value, String>)>;

impl SheetApp {
    /// Run the relayed runs that arrived since the last frame, and answer each.
    pub(crate) fn drain_host_runs(&mut self) {
        let runs: Vec<HostRun> = self.host_runs.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for run in runs {
            let outcome = if run.method == "command.batch" {
                self.run_command_batch(&run.params)
            } else {
                Err(format!("unknown-method: this editor does not take `{}`", run.method))
            };
            if let Some(answer) = &self.host_answer {
                answer(&run.id, outcome);
            }
        }
    }

    /// One `command.batch`: all of it, as ONE undo step, or none of it.
    pub fn run_command_batch(&mut self, params: &Value) -> Result<Value, String> {
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
        if self.session.active().is_none() {
            return Err("no-document: no workbook is open in this window".into());
        }
        let label = if label.is_empty() { "Assistant" } else { label };
        let token = self.session.group_begin(label).map_err(|e| format!("no-document: {e}"))?;
        let mut results = Vec::with_capacity(named.len());
        for (i, (name, args)) in named.iter().enumerate() {
            let before = self.dialog.is_some();
            let outcome = self.run(name, args.clone());
            if !before && self.dialog.is_some() {
                // A command in the list never opens one; if it does, the dialog must not sit over the person's work.
                self.dialog = None;
                let _ = self.session.group_rollback(token);
                self.after_engine();
                return Err(format!("step-failed: step {} ({name}): it needed a dialog -- nothing was changed", i + 1));
            }
            match outcome {
                Ok(result) => results.push(json!({ "name": name, "result": result })),
                Err(e) => {
                    let _ = self.session.group_rollback(token);
                    self.after_engine();
                    return Err(format!("step-failed: step {} ({name}): {e} -- nothing was changed", i + 1));
                }
            }
        }
        let changed = self.session.group_end(token).unwrap_or(false);
        self.after_engine();
        if changed {
            self.host_label = Some(label.to_owned());
        }
        Ok(json!({ "steps": results, "edits": usize::from(changed) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_command_is_a_real_one_and_none_reaches_a_file() {
        for id in allowed() {
            assert!(gridcraft_engine::find_command(id).is_some(), "{id} is not an engine command");
            assert!(!id.starts_with("file.") && !id.contains("open") && !id.contains("save") && !id.contains("export"), "{id} reaches a file");
        }
        assert!(!allowed().contains(&"edit.undo") && !allowed().contains(&"edit.redo"));
    }
}
