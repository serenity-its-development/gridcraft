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

use serde_json::Value;

use crate::SheetApp;

pub use gridcraft_engine::host_commands::{ARGUMENT_COMMANDS, ASSISTANT_COMMANDS, allowed};

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

    /// One `command.batch`: all of it, as ONE undo step, or none of it (the engine's `run_batch`, with this window's steps).
    pub fn run_command_batch(&mut self, params: &Value) -> Result<Value, String> {
        let (result, label) = gridcraft_engine::host_commands::run_batch(self, params)?;
        if label.is_some() {
            self.host_label = label;
        }
        Ok(result)
    }
}

impl gridcraft_engine::host_commands::BatchHost for SheetApp {
    fn session(&mut self) -> &mut gridcraft_engine::Session {
        &mut self.session
    }

    fn step(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let before = self.dialog.is_some();
        let outcome = self.run(name, args);
        if !before && self.dialog.is_some() {
            // A command in the list never opens one; if it does, the dialog must not sit over the person's work.
            self.dialog = None;
            return Err("it needed a dialog".into());
        }
        outcome
    }

    fn settled(&mut self) {
        self.after_engine();
    }
}
