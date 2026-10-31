//! ODS export: what `write_ods` writes reads back (through `read_ods`) and is the ODF it should be.

use std::io::Read as _;
use std::sync::Arc;

use gridcraft_core::{CellError, CellRef, RangeRef, Value};
use gridcraft_model::{Cell, DefinedName, Formula, Sheet, Workbook};

use crate::{Format, read_ods, sniff, write_ods};

fn at(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

fn part(bytes: &[u8], name: &str) -> String {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut s = String::new();
    z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
    s
}

fn formula(sh: &mut Sheet, c: &str, text: &str, cached: Value) {
    sh.set_cell(at(c), Cell { value: cached, formula: Some(Arc::new(Formula::new(text))), ..Default::default() });
}

fn book() -> Workbook {
    let mut wb = Workbook::new();
    let bold = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| {
        s.font.bold = true;
        s.font.color = gridcraft_model::style::Color::rgb(0xff, 0, 0);
        s.fill = gridcraft_model::style::Fill::solid(gridcraft_model::style::Color::rgb(0xff, 0xff, 0));
    });
    let money = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.num_fmt = gridcraft_model::NumFmt("#,##0.00".into()));
    let pct = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.num_fmt = gridcraft_model::NumFmt("0%".into()));
    let date = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.num_fmt = gridcraft_model::NumFmt("yyyy-mm-dd".into()));
    let time = wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.num_fmt = gridcraft_model::NumFmt("hh:mm".into()));
    {
        let sh = wb.sheet_mut(0).unwrap();
        sh.name = "Sales".into();
        sh.set_value(at("A1"), Value::text("Region"));
        sh.set_style(at("A1"), bold);
        sh.set_value(at("B1"), Value::text("two  spaces\nand a line"));
        sh.set_value(at("A2"), Value::Number(1234.5));
        sh.set_style(at("A2"), money);
        sh.set_value(at("A3"), Value::Number(0.25));
        sh.set_style(at("A3"), pct);
        sh.set_value(at("A4"), Value::Number(45658.0)); // 2025-01-01
        sh.set_style(at("A4"), date);
        sh.set_value(at("A5"), Value::Number(0.5));
        sh.set_style(at("A5"), time);
        sh.set_value(at("A6"), Value::Bool(true));
        sh.set_value(at("A7"), Value::Error(CellError::NA));
        formula(sh, "C2", "SUM(A2:A3)", Value::Number(1234.75));
        formula(sh, "C3", "IF(A6,'Other data'!B2&\"x\",0)", Value::text("yx"));
        sh.set_value(at("E40"), Value::text("far"));
        sh.merges.push(RangeRef::new(at("D1"), at("E2")));
        sh.set_value(at("D1"), Value::text("merged"));
        sh.cols.insert(0, gridcraft_model::LineInfo { size: Some(120.0), ..Default::default() });
        sh.cols.insert(3, gridcraft_model::LineInfo { hidden: true, ..Default::default() });
    }
    let mut other = Sheet::new("Other data");
    other.set_value(at("B2"), Value::text("y"));
    other.visibility = gridcraft_model::Visibility::Hidden;
    wb.sheets.push(Arc::new(other));
    wb.sheets.push(Arc::new(Sheet::new("Empty")));
    wb.names.push(DefinedName { name: "Total".into(), scope: None, formula: "Sales!$C$2".into(), comment: String::new(), hidden: false });
    wb.names.push(DefinedName { name: "Rate".into(), scope: None, formula: "0.07".into(), comment: String::new(), hidden: false });
    wb
}

#[test]
fn the_package_is_an_ods_with_its_mimetype_first_and_stored() {
    let bytes = write_ods(&book()).unwrap();
    assert_eq!(sniff(&bytes), Format::Ods);
    assert_eq!(&bytes[30..38], b"mimetype", "the first entry is the mimetype");
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(z.by_index(0).unwrap().compression(), zip::CompressionMethod::Stored);
    for p in ["content.xml", "styles.xml", "meta.xml", "META-INF/manifest.xml"] {
        assert!(z.by_name(p).is_ok(), "{p}");
    }
}

#[test]
fn values_types_merges_and_sheets_read_back() {
    let (wb, _) = read_ods(&write_ods(&book()).unwrap()).unwrap();
    assert_eq!(wb.sheets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Sales", "Other data", "Empty"]);
    let sh = wb.sheet(0).unwrap();
    assert_eq!(sh.value(at("A1")), Value::text("Region"));
    assert_eq!(sh.value(at("B1")), Value::text("two  spaces\nand a line"), "spaces and line breaks survive");
    assert_eq!(sh.value(at("A2")), Value::Number(1234.5));
    assert_eq!(sh.value(at("A3")), Value::Number(0.25));
    assert_eq!(sh.value(at("A4")), Value::Number(45658.0), "a date cell keeps its day");
    assert_eq!(sh.value(at("A5")), Value::Number(0.5), "a time cell keeps its time");
    assert_eq!(sh.value(at("A6")), Value::Bool(true));
    assert_eq!(sh.value(at("A7")), Value::Error(CellError::NA));
    assert_eq!(sh.value(at("C2")), Value::Number(1234.75), "a formula's saved result");
    assert_eq!(sh.value(at("C3")), Value::text("yx"));
    assert_eq!(sh.value(at("E40")), Value::text("far"), "rows after a run of empty ones");
    assert_eq!(sh.value(at("D1")), Value::text("merged"));
    assert!(sh.merges.contains(&RangeRef::new(at("D1"), at("E2"))), "{:?}", sh.merges);
    assert_eq!(wb.sheet(1).unwrap().value(at("B2")), Value::text("y"));
}

#[test]
fn formulas_are_openformula() {
    let content = part(&write_ods(&book()).unwrap(), "content.xml");
    assert!(content.contains(r#"table:formula="of:=SUM([.A2:.A3])""#), "{content}");
    assert!(
        content.contains(r#"of:=IF([.A6];[$&apos;Other data&apos;.B2]&amp;&quot;x&quot;;0)"#)
            || content.contains(r#"of:=IF([.A6];[$'Other data'.B2]&amp;&quot;x&quot;;0)"#),
        "{content}"
    );
    assert!(
        content.contains(r#"<table:named-range table:name="Total""#) && content.contains(r#"table:cell-range-address="$Sales.$C$2""#),
        "{content}"
    );
    assert!(content.contains(r#"<table:named-expression table:name="Rate""#) && content.contains(r#"table:expression="of:0.07""#), "{content}");
}

#[test]
fn styles_number_formats_and_columns_are_written() {
    let content = part(&write_ods(&book()).unwrap(), "content.xml");
    assert!(content.contains(r#"fo:font-weight="bold""#) && content.contains(r##"fo:color="#ff0000""##), "{content}");
    assert!(content.contains(r##"fo:background-color="#ffff00""##), "{content}");
    assert!(
        content.contains(
            r#"<number:number number:decimal-places="2" number:min-decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>"#
        ),
        "{content}"
    );
    assert!(content.contains("<number:percentage-style") && content.contains("<number:text>%</number:text>"), "{content}");
    assert!(content.contains(r#"<number:year number:style="long"/><number:text>-</number:text><number:month number:style="long"/>"#), "{content}");
    assert!(content.contains(r#"<number:hours number:style="long"/><number:text>:</number:text><number:minutes number:style="long"/>"#), "{content}");
    assert!(content.contains(r#"office:value-type="date" office:date-value="2025-01-01""#), "{content}");
    assert!(content.contains(r#"office:value-type="time" office:time-value="PT12H00M00S""#), "{content}");
    assert!(content.contains(r#"style:column-width="90pt""#), "120 px is 90 pt: {content}");
    assert!(content.contains(r#"table:visibility="collapse""#), "a hidden column");
    assert!(content.contains(r#"table:display="false""#), "a hidden sheet");
    assert!(content.contains("table:number-rows-repeated"), "empty rows are one repeated row");
}

#[test]
fn an_empty_workbook_still_has_a_row_and_a_column_per_table() {
    let content = part(&write_ods(&Workbook::new()).unwrap(), "content.xml");
    assert!(content.contains("<table:table-column") && content.contains("<table:table-row"), "{content}");
    let (wb, _) = read_ods(&write_ods(&Workbook::new()).unwrap()).unwrap();
    assert_eq!(wb.sheets.len(), 1);
}

#[test]
fn every_part_is_well_formed_xml() {
    let bytes = write_ods(&book()).unwrap();
    for p in ["content.xml", "styles.xml", "meta.xml", "META-INF/manifest.xml"] {
        let xml = part(&bytes, p);
        let mut r = quick_xml::Reader::from_str(&xml);
        let mut depth = 0i64;
        loop {
            match r.read_event().unwrap_or_else(|e| panic!("{p}: {e}")) {
                quick_xml::events::Event::Start(_) => depth += 1,
                quick_xml::events::Event::End(_) => depth -= 1,
                quick_xml::events::Event::Eof => break,
                _ => {}
            }
        }
        assert_eq!(depth, 0, "{p}");
    }
}
