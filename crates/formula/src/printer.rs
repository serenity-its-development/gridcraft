//! [`Expr`] → formula text (without `=`), A1 or R1C1.

use std::fmt::Write;

use gridcraft_core::{CellRef, col_to_letters, number_to_text};

use crate::ast::*;

pub use gridcraft_core::{quote_sheet, sheet_needs_quotes as needs_quotes};

fn sheet_prefix(s: &SheetSel) -> String {
    match s {
        SheetSel::Current => String::new(),
        SheetSel::Named(n) => format!("{}!", quote_sheet(n)),
        SheetSel::Span(a, b) => {
            let joined = format!("{a}:{b}");
            if needs_quotes(a) || needs_quotes(b) { format!("'{}'!", joined.replace('\'', "''")) } else { format!("{joined}!") }
        }
    }
}

fn anchor_a1(a: &Anchor) -> String {
    format!("{}{}{}{}", if a.col_abs { "$" } else { "" }, col_to_letters(a.col), if a.row_abs { "$" } else { "" }, a.row as u64 + 1)
}

pub fn reference_a1(r: &Reference) -> String {
    range_a1(r, ":")
}

/// A1 text with `sep` between the two ends of a range (`:` or a trim operator).
fn range_a1(r: &Reference, sep: &str) -> String {
    let body = match &r.kind {
        RefKind::Cell(a) => anchor_a1(a),
        RefKind::Range(a, b) => format!("{}{sep}{}", anchor_a1(a), anchor_a1(b)),
        RefKind::Rows(r0, a0, r1, a1) => format!("{}{}{sep}{}{}", if *a0 { "$" } else { "" }, r0 + 1, if *a1 { "$" } else { "" }, r1 + 1),
        RefKind::Cols(c0, a0, c1, a1) => {
            format!("{}{}{sep}{}{}", if *a0 { "$" } else { "" }, col_to_letters(*c0), if *a1 { "$" } else { "" }, col_to_letters(*c1))
        }
    };
    format!("{}{}", sheet_prefix(&r.sheet), body)
}

fn r1c1_part(letter: char, v: u32, abs: bool, base: u32) -> String {
    if abs {
        format!("{letter}{}", v + 1)
    } else {
        let d = v as i64 - base as i64;
        if d == 0 { letter.to_string() } else { format!("{letter}[{d}]") }
    }
}

fn anchor_r1c1(a: &Anchor, at: CellRef) -> String {
    format!("{}{}", r1c1_part('R', a.row, a.row_abs, at.row), r1c1_part('C', a.col, a.col_abs, at.col))
}

pub fn reference_r1c1(r: &Reference, at: CellRef) -> String {
    range_r1c1(r, at, ":")
}

/// R1C1 text with `sep` between the two ends of a range; a trim operator never collapses
/// `R1:R1` to `R1`.
fn range_r1c1(r: &Reference, at: CellRef, sep: &str) -> String {
    let body = match &r.kind {
        RefKind::Cell(a) => anchor_r1c1(a, at),
        RefKind::Range(a, b) => format!("{}{sep}{}", anchor_r1c1(a, at), anchor_r1c1(b, at)),
        RefKind::Rows(r0, a0, r1, a1) => {
            let (x, y) = (r1c1_part('R', *r0, *a0, at.row), r1c1_part('R', *r1, *a1, at.row));
            if x == y && sep == ":" { x } else { format!("{x}{sep}{y}") }
        }
        RefKind::Cols(c0, a0, c1, a1) => {
            let (x, y) = (r1c1_part('C', *c0, *a0, at.col), r1c1_part('C', *c1, *a1, at.col));
            if x == y && sep == ":" { x } else { format!("{x}{sep}{y}") }
        }
    };
    format!("{}{}", sheet_prefix(&r.sheet), body)
}

pub fn struct_ref(s: &StructRef) -> String {
    write_struct_ref(s, true)
}

/// Structured reference as files spell it: `[@Col]` is only Excel's display shorthand, files
/// hold `[[#This Row],[Col]]` (and `[#This Row]` for `[@]`).
pub fn struct_ref_file(s: &StructRef) -> String {
    write_struct_ref(s, false)
}

/// Splits a sheet-qualified name at its last `!` outside brackets. A `!` inside brackets
/// belongs to a structured reference's column (`Table1[Done!]`, `Table1[#REF!]`).
fn sheet_split(n: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut escaped = false;
    let mut at = None;
    for (i, c) in n.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\'' if depth > 0 => escaped = true,
            '[' => depth = depth.saturating_add(1),
            ']' => depth = depth.saturating_sub(1),
            '!' if depth == 0 => at = Some(i),
            _ => {}
        }
    }
    at.and_then(|i| Some((n.get(..i)?, n.get(i + 1..)?)))
}

fn write_struct_ref(s: &StructRef, at_shorthand: bool) -> String {
    let esc = |c: &str| c.replace('\'', "''").replace('[', "'[").replace(']', "']").replace('#', "'#");
    let item = |i: &StructItem| match i {
        StructItem::All => "#All",
        StructItem::Data => "#Data",
        StructItem::Headers => "#Headers",
        StructItem::Totals => "#Totals",
        StructItem::ThisRow => "#This Row",
    };
    let this_row = at_shorthand && s.specifiers == [StructItem::ThisRow];
    let mut out = s.table.clone();
    let simple_col = |c: &str| !c.chars().any(|ch| matches!(ch, ' ' | '[' | ']' | '#' | '\'' | ',' | ':'));
    match (&s.col_start, &s.col_end) {
        (None, None) if s.specifiers.is_empty() => out.push_str("[]"),
        (None, None) if this_row => out.push_str("[@]"),
        (None, None) if s.specifiers.len() == 1 => {
            let _ = write!(out, "[{}]", s.specifiers.first().map(item).unwrap_or(""));
        }
        (Some(c), None) if s.specifiers.is_empty() => {
            // Excel writes a name with spaces at its edges in the double-bracket form.
            if c.trim() == c {
                let _ = write!(out, "[{}]", esc(c));
            } else {
                let _ = write!(out, "[[{}]]", esc(c));
            }
        }
        (Some(c), None) if this_row => {
            if simple_col(c) {
                let _ = write!(out, "[@{}]", esc(c));
            } else {
                let _ = write!(out, "[@[{}]]", esc(c));
            }
        }
        _ => {
            let mut parts: Vec<String> = s.specifiers.iter().map(|i| format!("[{}]", item(i))).collect();
            match (&s.col_start, &s.col_end) {
                (Some(a), Some(b)) => parts.push(format!("[{}]:[{}]", esc(a), esc(b))),
                (Some(a), None) => parts.push(format!("[{}]", esc(a))),
                _ => {}
            }
            let _ = write!(out, "[{}]", parts.join(","));
        }
    }
    out
}

fn number(n: f64) -> String {
    let text = number_to_text(n);
    // 15 significant digits can round past f64::MAX, which no longer parses.
    if text.parse::<f64>().is_ok_and(f64::is_finite) {
        return text;
    }
    let exact = format!("{:E}", n);
    match exact.split_once('E') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}E+{e}"),
        _ => exact,
    }
}

/// Prints an expression in A1 notation.
pub fn print(e: &Expr) -> String {
    let mut s = String::new();
    write_expr(&mut s, e, None);
    s
}

/// Prints an expression in R1C1 notation relative to `at`.
pub fn print_r1c1(e: &Expr, at: CellRef) -> String {
    let mut s = String::new();
    write_expr(&mut s, e, Some(at));
    s
}

fn write_expr(s: &mut String, e: &Expr, r1c1: Option<CellRef>) {
    match e {
        Expr::Number(n) => s.push_str(&number(*n)),
        Expr::Text(t) => {
            s.push('"');
            s.push_str(&t.replace('"', "\"\""));
            s.push('"');
        }
        Expr::Bool(b) => s.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Error(err) => s.push_str(err.as_str()),
        Expr::Array(rows) => {
            s.push('{');
            for (ri, row) in rows.iter().enumerate() {
                if ri > 0 {
                    s.push(';');
                }
                for (ci, el) in row.iter().enumerate() {
                    if ci > 0 {
                        s.push(',');
                    }
                    write_expr(s, el, r1c1);
                }
            }
            s.push('}');
        }
        Expr::Ref(r) => match r1c1 {
            Some(at) => s.push_str(&reference_r1c1(r, at)),
            None => s.push_str(&reference_a1(r)),
        },
        Expr::Name(n) => match sheet_split(n) {
            // A sheet-qualified name is stored with the bare sheet name (`My Sheet!Total`): quote it so it parses back.
            Some((sheet, name)) => {
                s.push_str(&quote_sheet(sheet));
                s.push('!');
                s.push_str(name);
            }
            _ => s.push_str(n),
        },
        Expr::Struct(st) => s.push_str(&struct_ref(st)),
        Expr::Unary(op, x) => match op {
            UnOp::Neg => {
                s.push('-');
                write_expr(s, x, r1c1);
            }
            UnOp::Plus => {
                s.push('+');
                write_expr(s, x, r1c1);
            }
            UnOp::At => {
                s.push('@');
                write_expr(s, x, r1c1);
            }
            UnOp::Percent => {
                write_expr(s, x, r1c1);
                s.push('%');
            }
            UnOp::Spill => {
                write_expr(s, x, r1c1);
                // A reference that became #REF! (its sheet or cell was deleted) stays `#REF!`.
                if !matches!(**x, Expr::Error(_)) {
                    s.push('#');
                }
            }
            UnOp::Trim(mode) => match &**x {
                Expr::Ref(r) if !matches!(r.kind, RefKind::Cell(_)) => match r1c1 {
                    Some(at) => s.push_str(&range_r1c1(r, at, mode.symbol())),
                    None => s.push_str(&range_a1(r, mode.symbol())),
                },
                Expr::Binary(BinOp::Range, a, b) => {
                    write_expr(s, a, r1c1);
                    s.push_str(mode.symbol());
                    write_expr(s, b, r1c1);
                }
                // A reference that became #REF! stays `#REF!`.
                Expr::Error(_) => write_expr(s, x, r1c1),
                // No operator form (e.g. a range shrunk to one cell): the equivalent call.
                other => {
                    s.push_str("TRIMRANGE(");
                    write_expr(s, other, r1c1);
                    let _ = write!(s, ",{0},{0})", mode.code());
                }
            },
        },
        Expr::Binary(..) => {
            // Walk the left spine iteratively: a flat chain such as `1+1+1+...` parses into a
            // tree one level deeper per operator, and recursing on it could exhaust the stack.
            let mut rest: Vec<(&BinOp, &Expr)> = Vec::new();
            let mut first = e;
            while let Expr::Binary(op, a, b) = first {
                rest.push((op, b));
                first = a;
            }
            write_expr(s, first, r1c1);
            for (op, right) in rest.into_iter().rev() {
                s.push_str(op.symbol());
                write_expr(s, right, r1c1);
            }
        }
        Expr::Call(name, args) => {
            s.push_str(name);
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_expr(s, a, r1c1);
            }
            s.push(')');
        }
        Expr::Invoke(c, args) => {
            write_expr(s, c, r1c1);
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_expr(s, a, r1c1);
            }
            s.push(')');
        }
        Expr::Missing => {}
        Expr::Paren(x) => {
            s.push('(');
            write_expr(s, x, r1c1);
            s.push(')');
        }
    }
}

/// Prints an expression as OpenFormula (ODF 1.2 part 2), the `of:` namespace an `.ods` cell's
/// `table:formula` uses: references in brackets (`[.A1]`, `[$Sheet2.A1:.B2]`), `;` between
/// arguments, `~` for union and `!` for intersection. `None` when the formula uses something
/// OpenFormula has no spelling for (a structured table reference, a spill `#`, a trim range, a
/// LAMBDA invocation): the caller then keeps only the cell's value.
pub fn print_odf(e: &Expr) -> Option<String> {
    let mut s = String::new();
    write_odf(&mut s, e)?;
    Some(s)
}

fn odf_sheet(name: &str) -> String {
    // A sheet name is quoted when it isn't a plain identifier (ODF 1.2 part 2, 5.8).
    let plain = name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit());
    if plain { format!("${name}") } else { format!("$'{}'", name.replace('\'', "''")) }
}

fn odf_anchor(a: &Anchor) -> String {
    format!("{}{}{}{}", if a.col_abs { "$" } else { "" }, col_to_letters(a.col), if a.row_abs { "$" } else { "" }, a.row as u64 + 1)
}

fn odf_reference(r: &Reference) -> String {
    let (first, last) = match &r.sheet {
        SheetSel::Current => (String::new(), String::new()),
        SheetSel::Named(n) => (odf_sheet(n), String::new()),
        SheetSel::Span(a, b) => (odf_sheet(a), odf_sheet(b)),
    };
    let ends = match &r.kind {
        RefKind::Cell(a) => (odf_anchor(a), None),
        RefKind::Range(a, b) => (odf_anchor(a), Some(odf_anchor(b))),
        RefKind::Rows(r0, a0, r1, a1) => {
            (format!("{}{}", if *a0 { "$" } else { "" }, r0 + 1), Some(format!("{}{}", if *a1 { "$" } else { "" }, r1 + 1)))
        }
        RefKind::Cols(c0, a0, c1, a1) => {
            (format!("{}{}", if *a0 { "$" } else { "" }, col_to_letters(*c0)), Some(format!("{}{}", if *a1 { "$" } else { "" }, col_to_letters(*c1))))
        }
    };
    match (ends, matches!(r.sheet, SheetSel::Span(..))) {
        ((a, None), false) => format!("[{first}.{a}]"),
        ((a, None), true) => format!("[{first}.{a}:{last}.{a}]"),
        ((a, Some(b)), _) => format!("[{first}.{a}:{last}.{b}]"),
    }
}

fn write_odf(s: &mut String, e: &Expr) -> Option<()> {
    match e {
        Expr::Number(n) => s.push_str(&number(*n)),
        Expr::Text(t) => {
            s.push('"');
            s.push_str(&t.replace('"', "\"\""));
            s.push('"');
        }
        Expr::Bool(b) => s.push_str(if *b { "TRUE()" } else { "FALSE()" }),
        Expr::Error(err) => s.push_str(err.as_str()),
        Expr::Array(rows) => {
            s.push('{');
            for (ri, row) in rows.iter().enumerate() {
                if ri > 0 {
                    s.push('|');
                }
                for (ci, el) in row.iter().enumerate() {
                    if ci > 0 {
                        s.push(';');
                    }
                    write_odf(s, el)?;
                }
            }
            s.push('}');
        }
        Expr::Ref(r) => s.push_str(&odf_reference(r)),
        Expr::Name(n) => {
            if sheet_split(n).is_some() {
                return None;
            }
            s.push_str(n);
        }
        Expr::Struct(_) | Expr::Invoke(..) => return None,
        Expr::Unary(op, x) => match op {
            UnOp::Neg => {
                s.push('-');
                write_odf(s, x)?;
            }
            UnOp::Plus => {
                s.push('+');
                write_odf(s, x)?;
            }
            // Implicit intersection is what OpenFormula does anyway for a range in a scalar slot.
            UnOp::At => write_odf(s, x)?,
            UnOp::Percent => {
                write_odf(s, x)?;
                s.push('%');
            }
            UnOp::Spill | UnOp::Trim(_) => return None,
        },
        Expr::Binary(..) => {
            let mut rest: Vec<(&BinOp, &Expr)> = Vec::new();
            let mut first = e;
            while let Expr::Binary(op, a, b) = first {
                rest.push((op, b));
                first = a;
            }
            write_odf(s, first)?;
            for (op, right) in rest.into_iter().rev() {
                s.push_str(match op {
                    BinOp::Union => "~",
                    BinOp::Intersect => "!",
                    other => other.symbol(),
                });
                write_odf(s, right)?;
            }
        }
        Expr::Call(name, args) => {
            s.push_str(name);
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                write_odf(s, a)?;
            }
            s.push(')');
        }
        Expr::Missing => {}
        Expr::Paren(x) => {
            s.push('(');
            write_odf(s, x)?;
            s.push(')');
        }
    }
    Some(())
}
