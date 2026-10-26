//! The assistant's `command.batch` in the window: a chart, a PivotTable and a defined name made by one run are the window's own
//! (the chart is selected, the report is drawn), the run is ONE undo step, and a step that needs a dialog takes the run back
//! without leaving the dialog over the person's work.

use egui::vec2;
use gridcraft_engine::Session;
use gridcraft_ui_egui::SheetApp;
use serde_json::json;

fn harness() -> egui_kittest::Harness<'static, SheetApp> {
    let mut session = Session::new();
    session.new_workbook();
    let mut h = egui_kittest::Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SheetApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SheetApp::new(session, Default::default()),
    );
    h.run_steps(4);
    h
}

#[test]
fn a_run_makes_a_name_a_pivot_and_a_chart_as_one_undo_step() {
    let mut h = harness();
    let run = json!({"label": "Sales summary", "steps": [
        {"name": "range.setValues", "args": {"range": "A1", "values": [["Region", "Sales"], ["East", 100], ["West", 80], ["East", 30]]}},
        {"name": "formulas.defineName", "args": {"name": "SalesData", "refersTo": "=Sheet1!$A$1:$B$4"}},
        {"name": "insert.pivotTable", "args": {"source": "SalesData", "destination": "Sheet1!D1", "name": "ByRegion", "rows": ["Region"], "values": ["Sales"]}},
        {"name": "insert.chart", "args": {"range": "A1:B4", "type": "column", "at": "H2"}},
        {"name": "insert.pivotChart", "args": {"pivot": "ByRegion", "at": "H20"}},
    ]});
    let out = h.state_mut().run_command_batch(&run).unwrap();
    assert_eq!(out["edits"], 1);
    h.run_steps(3);
    let seen = h.state_mut().session.execute("document.inspect", json!({})).unwrap();
    assert_eq!(seen["names"][0]["name"], "SalesData");
    assert_eq!(seen["sheets"][0]["pivots"][0]["name"], "ByRegion");
    assert_eq!(seen["sheets"][0]["charts"].as_array().unwrap().len(), 2);
    assert_eq!(h.state_mut().host_label.as_deref(), Some("Sales summary"));
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    let after = h.state_mut().session.execute("document.inspect", json!({})).unwrap();
    assert_eq!(after["sheets"][0]["charts"].as_array().unwrap().len(), 0);
    assert_eq!(after["sheets"][0]["pivots"].as_array().unwrap().len(), 0);
    assert_eq!(after["names"].as_array().unwrap().len(), 0);
}

#[test]
fn a_step_that_needs_a_dialog_takes_the_run_back_and_leaves_none_open() {
    let mut h = harness();
    let run = json!({"label": "x", "steps": [
        {"name": "cell.set", "args": {"cell": "A1", "input": "1"}},
        {"name": "pivot.calculatedField", "args": {}},
    ]});
    let e = h.state_mut().run_command_batch(&run).unwrap_err();
    assert!(e.starts_with("step-failed: step 2 (pivot.calculatedField)"), "{e}");
    h.run_steps(2);
    assert!(h.state_mut().dialog.is_none(), "a dialog was left over the person's work");
    let read = h.state_mut().session.execute("sheet.read", json!({})).unwrap();
    assert_eq!(read["values"], json!([]));
}
