//! `WKS_` functions through the calc engine: a held question shows `#GETTING_DATA` until the
//! workspace answers, `#REFUSED` when it refuses, and its answer recalculates what reads it.

use gridcraft_calc::{Calc, Question};
use gridcraft_core::{Array, CellError, CellRef, Value};
use gridcraft_model::{Cell, Formula, Workbook};

fn c(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

struct T {
    wb: Workbook,
    calc: Calc,
}

impl T {
    fn new(names: &[&str]) -> T {
        let mut calc = Calc::new();
        calc.held.set_names(names);
        T { wb: Workbook::new(), calc }
    }
    fn set(&mut self, at: &str, input: &str) {
        let cell = if input.starts_with('=') {
            Cell::formula(Formula::new(input))
        } else if input.is_empty() {
            self.wb.sheet_mut(0).unwrap().cells.remove(c(at));
            self.calc.cells_changed(&mut self.wb, &[(0, c(at))]);
            return;
        } else {
            Cell::value(gridcraft_core::parse::parse_input(input, self.wb.date_system).value)
        };
        self.wb.sheet_mut(0).unwrap().cells.set(c(at), cell);
        self.calc.cells_changed(&mut self.wb, &[(0, c(at))]);
    }
    fn get(&self, at: &str) -> Value {
        self.wb.sheet(0).unwrap().value(c(at))
    }
    fn held(&self, at: &str) -> bool {
        self.wb.sheet(0).unwrap().held.contains(&c(at))
    }
    fn answer(&mut self, id: &str, a: Result<Value, String>) -> bool {
        self.calc.held_answered(&mut self.wb, id, a)
    }
}

fn table() -> Value {
    let v = |s: &str| Value::text(s);
    Array::new(3, 2, vec![v("name"), v("hours"), v("Ana"), Value::Number(3.0), v("Bo"), Value::Number(5.0)]).unwrap().into()
}

#[test]
fn an_unanswered_question_is_getting_data_and_asked_once() {
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\"open tickets\")");
    t.set("B1", "=A1");
    assert_eq!(t.get("A1"), Value::Error(CellError::GettingData));
    assert_eq!(t.get("B1"), Value::Error(CellError::GettingData));
    let asks = t.calc.held.take_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].1, Question::Select { text: "open tickets".into(), page: 200, after: None });
    // Recalculating asks nothing new.
    t.set("Z9", "1");
    assert!(t.calc.held.take_asks().is_empty());
}

#[test]
fn the_answer_spills_and_recalculates_what_reads_it() {
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\"hours\", 50)");
    t.set("D1", "=SUM(B2:B3)*2");
    t.set("E1", "=D1+1");
    t.set("F1", "7");
    t.set("G1", "=F1*2");
    let asks = t.calc.held.take_asks();
    assert_eq!(asks[0].1, Question::Select { text: "hours".into(), page: 50, after: None });
    assert!(t.answer(&asks[0].0, Ok(table())));
    assert_eq!(t.get("A1"), Value::text("name"));
    assert_eq!(t.get("B3"), Value::Number(5.0));
    assert_eq!(t.get("D1"), Value::Number(16.0));
    assert_eq!(t.get("E1"), Value::Number(17.0));
    // What read the held value, directly or through other formulas, is marked; nothing else.
    for at in ["A1", "B1", "A3", "B3", "D1", "E1"] {
        assert!(t.held(at), "{at} is held");
    }
    assert!(!t.held("F1") && !t.held("G1") && !t.held("C1"));
    // The same answer again changes nothing; a fresh one replaces it.
    assert!(!t.answer(&asks[0].0, Ok(table())));
    let fresh = Array::new(2, 2, vec![Value::text("name"), Value::text("hours"), Value::text("Cy"), Value::Number(1.0)]).unwrap();
    assert!(t.answer(&asks[0].0, Ok(fresh.into())));
    assert_eq!(t.get("D1"), Value::Number(2.0));
}

#[test]
fn a_refusal_is_refused_with_its_reason() {
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\"salaries\")");
    t.set("B1", "=IFERROR(A1,\"-\")");
    let id = t.calc.held.take_asks().remove(0).0;
    assert!(t.answer(&id, Err("you cannot read payroll".into())));
    assert_eq!(t.get("A1"), Value::Error(CellError::Refused));
    assert_eq!(CellError::Refused.as_str(), "#REFUSED");
    assert_eq!(CellError::parse("#refused"), Some(CellError::Refused));
    assert_eq!(t.calc.held_reason(0, c("A1")), Some("you cannot read payroll"));
    assert_eq!(t.calc.held_reason(0, c("B1")), None);
    assert_eq!(t.get("B1"), Value::text("-"));
    assert!(t.held("B1"));
}

#[test]
fn cells_asking_the_same_question_join_it() {
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\"x\")");
    t.set("A5", "=WKS_QUERY(\"x\", 200)");
    t.set("A9", "=WKS_QUERY(\"y\")");
    let asks = t.calc.held.take_asks();
    assert_eq!(asks.len(), 2, "{asks:?}");
    let x = asks.iter().find(|(_, q)| matches!(q, Question::Select { text, .. } if text == "x")).unwrap().0.clone();
    assert!(t.answer(&x, Ok(Value::Number(4.0))));
    assert_eq!(t.get("A1"), Value::Number(4.0));
    assert_eq!(t.get("A5"), Value::Number(4.0));
    assert_eq!(t.get("A9"), Value::Error(CellError::GettingData));
}

#[test]
fn a_question_no_cell_asks_is_dropped_and_its_answer_ignored() {
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\"x\")");
    let id = t.calc.held.take_asks().remove(0).0;
    t.set("A1", "");
    assert!(t.calc.held.is_empty());
    assert!(!t.answer(&id, Ok(Value::Number(1.0))));
    // Asked again, it is a new question with a new id.
    t.set("A1", "=WKS_QUERY(\"x\")");
    let again = t.calc.held.take_asks().remove(0).0;
    assert_ne!(again, id);
    // Closing drops every question and the names.
    t.calc.held.clear();
    assert!(!t.answer(&again, Ok(Value::Number(1.0))));
    t.set("A2", "=WKS_QUERY(\"x\")");
    assert_eq!(t.get("A2"), Value::Error(CellError::Name));
}

#[test]
fn wks_functions_are_the_ones_the_host_names() {
    let mut t = T::new(&["wks_rate", "NOT_WKS", "WKS_BAD NAME"]);
    t.set("A1", "=WKS_RATE(\"AUD\", 2)");
    t.set("A2", "=WKS_OTHER(1)");
    t.set("A3", "=WKS_QUERY(1/0)");
    t.set("A4", "=WKS_QUERY(\"x\", 201)");
    assert_eq!(t.get("A1"), Value::Error(CellError::GettingData));
    assert_eq!(t.get("A2"), Value::Error(CellError::Name));
    assert_eq!(t.get("A3"), Value::Error(CellError::Div0));
    assert_eq!(t.get("A4"), Value::Error(CellError::Value));
    let asks = t.calc.held.take_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].1, Question::Fn { name: "WKS_RATE".into(), args: vec![Value::text("AUD"), Value::Number(2.0)] });
    assert!(t.answer(&asks[0].0, Ok(Value::Number(0.66))));
    assert_eq!(t.get("A1"), Value::Number(0.66));
}

#[test]
fn outside_a_workspace_wks_names_are_unknown() {
    let mut t = T { wb: Workbook::new(), calc: Calc::new() };
    t.set("A1", "=WKS_QUERY(\"x\")");
    assert_eq!(t.get("A1"), Value::Error(CellError::Name));
    assert!(t.calc.held.take_asks().is_empty());
}

#[test]
fn past_the_cap_a_question_is_value_and_the_text_is_trimmed() {
    let mut t = T::new(&[]);
    for i in 1..=gridcraft_calc::held::MAX_ENTRIES + 1 {
        t.set(&format!("A{i}"), &format!("=WKS_QUERY(\"q{i}\")"));
    }
    assert_eq!(t.get("A256"), Value::Error(CellError::GettingData));
    assert_eq!(t.get("A257"), Value::Error(CellError::Value));
    assert_eq!(t.calc.held.take_asks().len(), gridcraft_calc::held::MAX_ENTRIES);
    // ' x' and 'x' are one question.
    let mut t = T::new(&[]);
    t.set("A1", "=WKS_QUERY(\" x\")");
    t.set("A2", "=WKS_QUERY(\"x\")");
    let asks = t.calc.held.take_asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].1, Question::Select { text: "x".into(), page: 200, after: None });
}
