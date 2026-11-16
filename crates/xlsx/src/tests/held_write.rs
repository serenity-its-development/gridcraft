//! What the workspace holds (a `WKS_` function's answer and every cell that read it, `Sheet::held`) is saved as
//! `#GETTING_DATA`: the file never keeps the workspace's data, and the formulas stay to ask again.

use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_model::{Cell, Formula, Workbook};

use crate::{read_xlsx, write_xlsx};

fn at(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

fn formula(text: &str, value: Value) -> Cell {
    let mut cell = Cell::formula(Formula::new(text));
    cell.value = value;
    cell
}

#[test]
fn held_cells_are_saved_as_getting_data() {
    let mut wb = Workbook::new();
    let sh = wb.sheet_mut(0).unwrap();
    // The answer, a cell that read it, one that read that, and a refusal: all held.
    sh.cells.set(at("A1"), formula("WKS_RATE(\"AUD\")", Value::Number(0.66)));
    sh.cells.set(at("B1"), formula("A1*100", Value::Number(66.0)));
    sh.cells.set(at("C1"), formula("TEXT(B1,\"0\")", Value::text("66")));
    sh.cells.set(at("D1"), formula("WKS_QUERY(\"hours\")", Value::Error(CellError::Refused)));
    // Not held: kept as calculated.
    sh.cells.set(at("E1"), formula("1+1", Value::Number(2.0)));
    sh.held.extend(["A1", "B1", "C1", "D1"].map(at));

    let (back, _) = read_xlsx(&write_xlsx(&wb).unwrap()).unwrap();
    let sh = back.sheet(0).unwrap();
    for a in ["A1", "B1", "C1", "D1"] {
        let cell = sh.cells.get(at(a)).unwrap();
        assert_eq!(cell.value.scalar(), Value::Error(CellError::GettingData), "{a}");
        assert!(cell.formula.is_some(), "{a} keeps its formula");
    }
    assert_eq!(sh.cells.get(at("E1")).unwrap().value.scalar(), Value::Number(2.0));
}

#[test]
fn a_refusal_is_never_saved() {
    let mut wb = Workbook::new();
    let sh = wb.sheet_mut(0).unwrap();
    sh.cells.set(at("A1"), formula("WKS_QUERY(\"hours\")", Value::Error(CellError::Refused)));
    let (back, _) = read_xlsx(&write_xlsx(&wb).unwrap()).unwrap();
    assert_eq!(back.sheet(0).unwrap().cells.get(at("A1")).unwrap().value.scalar(), Value::Error(CellError::GettingData));
}
