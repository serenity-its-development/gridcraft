//! QUERY through the calc engine: its result is an array other functions compose with and that
//! spills into the grid.
// rules: builders/120

use gridcraft_calc::Calc;
use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Formula, Workbook};

fn c(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

struct T {
    wb: Workbook,
    calc: Calc,
}

impl T {
    fn new() -> T {
        T { wb: Workbook::new(), calc: Calc::new() }
    }
    fn set(&mut self, at: &str, input: &str) {
        let cell = if input.starts_with('=') {
            Cell::formula(Formula::new(input))
        } else {
            Cell::value(gridcraft_core::parse::parse_input(input, self.wb.date_system).value)
        };
        self.wb.sheet_mut(0).unwrap().cells.set(c(at), cell);
        self.calc.cells_changed(&mut self.wb, &[(0, c(at))]);
    }
    fn get(&self, at: &str) -> Value {
        self.wb.sheet(0).unwrap().value(c(at))
    }
    fn num(&self, at: &str) -> f64 {
        self.get(at).as_f64().unwrap_or_else(|| panic!("{at} = {:?}", self.get(at)))
    }
}

fn data() -> T {
    let mut t = T::new();
    for (row, [a, b, v]) in [["Name", "N", "V"], ["a", "1", "10"], ["b", "2", "20"], ["c", "3", "30"], ["d", "0", "40"]].iter().enumerate() {
        t.set(&format!("A{}", row + 1), a);
        t.set(&format!("B{}", row + 1), b);
        t.set(&format!("C{}", row + 1), v);
    }
    t
}

#[test]
fn query_composes_with_other_functions() {
    let mut t = data();
    t.set("H1", r#"=SUM(QUERY(A1:C5,"select C where B > 1"))"#);
    assert_eq!(t.num("H1"), 50.0);
    // The header row counts as a row of the result.
    t.set("H2", r#"=ROWS(QUERY(A1:C5,"select C where B > 1"))"#);
    assert_eq!(t.num("H2"), 3.0);
    // A query over a query's result: its columns are Col1, Col2, ...
    t.set("H3", r#"=SUM(QUERY(QUERY(A1:C5,"select A, C where B > 0"),"select Col2 where Col1 != 'b'"))"#);
    assert_eq!(t.num("H3"), 40.0);
    // Recalculates when its data changes.
    t.set("C3", "25");
    assert_eq!(t.num("H1"), 55.0);
}

#[test]
fn a_range_off_column_a_answers_to_its_own_letters() {
    let mut t = T::new();
    for (row, [d, e]) in [["k", "v"], ["1", "10"], ["2", "20"], ["3", "30"]].iter().enumerate() {
        t.set(&format!("D{}", row + 1), d);
        t.set(&format!("E{}", row + 1), e);
        t.set(&format!("F{}", row + 1), "x");
    }
    t.set("H1", r#"=SUM(QUERY(D1:F4,"select E where D > 1"))"#);
    assert_eq!(t.num("H1"), 50.0);
    // Its first column is D, not A.
    t.set("H2", r#"=QUERY(D1:F4,"select A")"#);
    assert_eq!(t.get("H2"), Value::Error(gridcraft_core::CellError::Value));
}

#[test]
fn query_spills_into_the_grid() {
    let mut t = data();
    t.set("E1", r#"=QUERY(A1:C5,"select A, C order by C desc limit 2")"#);
    assert_eq!(t.get("E1"), Value::text("Name"));
    assert_eq!(t.get("F1"), Value::text("V"));
    assert_eq!(t.get("E2"), Value::text("d"));
    assert_eq!(t.num("F2"), 40.0);
    assert_eq!(t.get("E3"), Value::text("c"));
    assert_eq!(t.num("F3"), 30.0);
    assert_eq!(t.get("E4"), Value::Empty);
}
