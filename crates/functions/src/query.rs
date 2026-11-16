//! Google Sheets' `QUERY(data, query, [headers])`: the Google Visualization API query language
//! over an in-sheet range or array, returning a spilled array that composes with every other
//! function (FILTER, SUM, SORT, INDEX, a second QUERY…).
//!
//! Columns are named by letter (`A`, `B`…, relative to the range's first column) when `data`
//! came from a reference, and `Col1`, `Col2`… when it is an array literal or a function's result,
//! as in Sheets. Each column takes its majority type (number, string or boolean); cells of
//! another type are null. Clauses: SELECT, WHERE, GROUP BY, PIVOT, ORDER BY, LIMIT, OFFSET,
//! LABEL, FORMAT, OPTIONS, in that order. A query Sheets refuses is `#VALUE!`; an empty result
//! with no header row is `#N/A`.

use std::cmp::Ordering;
use std::collections::HashMap;

use gridcraft_core::addr::col_to_letters;
use gridcraft_core::date::{datetime_from_serial, serial_from_ymd, time_fraction};
use gridcraft_core::{Array, CellError, DateSystem, Value};
use regex::Regex;

use crate::util::{A, R, arg, array_val, as_array, has, num};
use crate::{Arg, Ctx, FnSpec};

/// Sheets shows every refused query (parse error, unknown column, type mismatch) as `#VALUE!`.
const BAD: CellError = CellError::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Num,
    Str,
    Bool,
}

/// A query value. Dates, datetimes and times of day are serial numbers, as in the sheet.
#[derive(Clone, Debug, PartialEq)]
enum Q {
    Null,
    Num(f64),
    Str(String),
    Bool(bool),
}

/// The query language's order: null first, then by value within a type.
fn qcmp(a: &Q, b: &Q) -> Ordering {
    fn rank(q: &Q) -> u8 {
        match q {
            Q::Null => 0,
            Q::Num(_) => 1,
            Q::Str(_) => 2,
            Q::Bool(_) => 3,
        }
    }
    match (a, b) {
        (Q::Num(x), Q::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Q::Str(x), Q::Str(y)) => x.cmp(y),
        (Q::Bool(x), Q::Bool(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

fn keys_cmp(a: &[Q], b: &[Q]) -> Ordering {
    a.iter().zip(b).map(|(x, y)| qcmp(x, y)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
}

/// Label text for a value (pivot headers).
fn show(q: &Q) -> String {
    match q {
        Q::Null => String::new(),
        Q::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
        Q::Num(n) => n.to_string(),
        Q::Str(s) => s.clone(),
        Q::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
    }
}

/// A result cell: null and the empty string are blank.
fn out(q: &Q) -> Value {
    match q {
        Q::Null => Value::Empty,
        Q::Num(n) => Value::number(*n),
        Q::Str(s) if s.is_empty() => Value::Empty,
        Q::Str(s) => Value::text(s.as_str()),
        Q::Bool(b) => Value::Bool(*b),
    }
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Id(String),
    /// A back-quoted column id.
    Quoted(String),
    Num(f64),
    Str(String),
    Sym(&'static str),
}

fn lex(s: &str) -> R<Vec<Tok>> {
    let cs: Vec<char> = s.chars().collect();
    let at = |i: usize| cs.get(i).copied();
    let text = |a: usize, b: usize| cs.get(a..b).map(|x| x.iter().collect::<String>()).ok_or(BAD);
    let mut toks = Vec::new();
    let mut i = 0;
    while let Some(c) = at(i) {
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphabetic() || c == '_' {
            let st = i;
            while at(i).is_some_and(|c| c.is_alphanumeric() || c == '_') {
                i += 1;
            }
            toks.push(Tok::Id(text(st, i)?));
        } else if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let st = i;
            while at(i).is_some_and(|c| c.is_ascii_digit() || c == '.') {
                i += 1;
            }
            if at(i).is_some_and(|c| c == 'e' || c == 'E') {
                let mut j = i + 1;
                if at(j).is_some_and(|c| c == '+' || c == '-') {
                    j += 1;
                }
                if at(j).is_some_and(|c| c.is_ascii_digit()) {
                    i = j;
                    while at(i).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1;
                    }
                }
            }
            toks.push(Tok::Num(text(st, i)?.parse().map_err(|_| BAD)?));
        } else if c == '\'' || c == '"' || c == '`' {
            let st = i + 1;
            i = st;
            while at(i).is_some_and(|d| d != c) {
                i += 1;
            }
            if at(i).is_none() {
                return Err(BAD);
            }
            let t = text(st, i)?;
            i += 1;
            toks.push(if c == '`' { Tok::Quoted(t) } else { Tok::Str(t) });
        } else {
            let two = text(i, (i + 2).min(cs.len()))?;
            let sym = ["<=", ">=", "!=", "<>"]
                .into_iter()
                .find(|s| *s == two)
                .or_else(|| [",", "(", ")", "*", "+", "-", "/", "=", "<", ">"].into_iter().find(|s| s.starts_with(c)))
                .ok_or(BAD)?;
            i += sym.len();
            toks.push(Tok::Sym(sym));
        }
    }
    Ok(toks)
}

// ---------------------------------------------------------------------------------------------
// Syntax

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrOp {
    Contains,
    Starts,
    Ends,
    Matches,
    Like,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agg {
    Avg,
    Count,
    Max,
    Min,
    Sum,
}

impl Agg {
    const ALL: [Agg; 5] = [Agg::Avg, Agg::Count, Agg::Max, Agg::Min, Agg::Sum];
    fn name(self) -> &'static str {
        match self {
            Agg::Avg => "avg",
            Agg::Count => "count",
            Agg::Max => "max",
            Agg::Min => "min",
            Agg::Sum => "sum",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Func {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Quarter,
    DayOfWeek,
    Now,
    DateDiff,
    ToDate,
    Upper,
    Lower,
}

impl Func {
    const ALL: [Func; 14] = [
        Func::Year,
        Func::Month,
        Func::Day,
        Func::Hour,
        Func::Minute,
        Func::Second,
        Func::Millisecond,
        Func::Quarter,
        Func::DayOfWeek,
        Func::Now,
        Func::DateDiff,
        Func::ToDate,
        Func::Upper,
        Func::Lower,
    ];
    fn name(self) -> &'static str {
        match self {
            Func::Year => "year",
            Func::Month => "month",
            Func::Day => "day",
            Func::Hour => "hour",
            Func::Minute => "minute",
            Func::Second => "second",
            Func::Millisecond => "millisecond",
            Func::Quarter => "quarter",
            Func::DayOfWeek => "dayOfWeek",
            Func::Now => "now",
            Func::DateDiff => "dateDiff",
            Func::ToDate => "toDate",
            Func::Upper => "upper",
            Func::Lower => "lower",
        }
    }
    fn arity(self) -> usize {
        match self {
            Func::Now => 0,
            Func::DateDiff => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Col(usize),
    Lit(Q),
    Neg(Box<Expr>),
    Arith(char, Box<Expr>, Box<Expr>),
    Cmp(Cmp, Box<Expr>, Box<Expr>),
    Str(StrOp, Box<Expr>, Box<Expr>),
    /// `is null` (`false`) or `is not null` (`true`).
    IsNull(Box<Expr>, bool),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Func(Func, Vec<Expr>),
    Agg(Agg, usize),
}

impl Expr {
    fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Col(_) | Expr::Lit(_) | Expr::Agg(..) => Vec::new(),
            Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) => vec![a],
            Expr::Arith(_, a, b) | Expr::Cmp(_, a, b) | Expr::Str(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => vec![a, b],
            Expr::Func(_, xs) => xs.iter().collect(),
        }
    }
    fn has_agg(&self) -> bool {
        matches!(self, Expr::Agg(..)) || self.children().iter().any(|c| c.has_agg())
    }
    /// Valid in an aggregating query: every column outside an aggregate is inside a GROUP BY
    /// expression.
    fn grouped_ok(&self, group: &[Expr]) -> bool {
        group.contains(self)
            || match self {
                Expr::Col(_) => false,
                Expr::Agg(..) | Expr::Lit(_) => true,
                _ => self.children().iter().all(|c| c.grouped_ok(group)),
            }
    }
}

#[derive(Default)]
struct Query {
    /// `None` for `select *` or no SELECT clause.
    select: Option<Vec<Expr>>,
    filter: Option<Expr>,
    group: Vec<Expr>,
    pivot: Vec<Expr>,
    /// Expression and whether it sorts descending.
    order: Vec<(Expr, bool)>,
    limit: Option<usize>,
    offset: usize,
    labels: Vec<(Expr, String)>,
}

/// The data's columns: query ids, header labels and majority types.
struct Cols {
    ids: Vec<String>,
    labels: Vec<String>,
    types: Vec<Ty>,
}

const RESERVED: &[&str] = &[
    "select",
    "where",
    "group",
    "by",
    "pivot",
    "order",
    "limit",
    "offset",
    "label",
    "format",
    "options",
    "and",
    "or",
    "not",
    "asc",
    "desc",
    "is",
    "null",
    "contains",
    "starts",
    "ends",
    "with",
    "matches",
    "like",
    "true",
    "false",
    "date",
    "datetime",
    "timeofday",
];

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    cols: &'a Cols,
    sys: DateSystem,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    /// Consumes keyword `w` (case-insensitive) when it is next.
    fn kw(&mut self, w: &str) -> bool {
        let yes = matches!(self.peek(), Some(Tok::Id(s)) if s.eq_ignore_ascii_case(w));
        if yes {
            self.pos += 1;
        }
        yes
    }
    fn need_kw(&mut self, w: &str) -> R<()> {
        if self.kw(w) { Ok(()) } else { Err(BAD) }
    }
    fn sym(&mut self, s: &str) -> bool {
        let yes = matches!(self.peek(), Some(Tok::Sym(t)) if *t == s);
        if yes {
            self.pos += 1;
        }
        yes
    }
    fn string(&mut self) -> R<String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            _ => Err(BAD),
        }
    }
    fn count(&mut self) -> R<usize> {
        match self.next() {
            Some(Tok::Num(n)) if n >= 0.0 && n.fract() == 0.0 && n < 1e15 => Ok(n as usize),
            _ => Err(BAD),
        }
    }
    fn list(&mut self) -> R<Vec<Expr>> {
        let mut v = vec![self.expr()?];
        while self.sym(",") {
            v.push(self.expr()?);
        }
        Ok(v)
    }
    fn pairs(&mut self) -> R<Vec<(Expr, String)>> {
        let mut v = Vec::new();
        loop {
            let e = self.expr()?;
            v.push((e, self.string()?));
            if !self.sym(",") {
                return Ok(v);
            }
        }
    }

    /// The clauses, each optional, in the order the language requires.
    fn query(&mut self) -> R<Query> {
        let mut q = Query::default();
        if self.kw("select") && !self.sym("*") {
            q.select = Some(self.list()?);
        }
        if self.kw("where") {
            q.filter = Some(self.expr()?);
        }
        if self.kw("group") {
            self.need_kw("by")?;
            q.group = self.list()?;
        }
        if self.kw("pivot") {
            q.pivot = self.list()?;
        }
        if self.kw("order") {
            self.need_kw("by")?;
            loop {
                let e = self.expr()?;
                let desc = self.kw("desc");
                if !desc {
                    self.kw("asc");
                }
                q.order.push((e, desc));
                if !self.sym(",") {
                    break;
                }
            }
        }
        if self.kw("limit") {
            q.limit = Some(self.count()?);
        }
        if self.kw("offset") {
            q.offset = self.count()?;
        }
        if self.kw("label") {
            q.labels = self.pairs()?;
        }
        // FORMAT sets display patterns and OPTIONS (no_format, no_values) display choices: a
        // function result carries no cell formats, so both are parsed and have no effect.
        if self.kw("format") {
            self.pairs()?;
        }
        if self.kw("options") {
            while matches!(self.peek(), Some(Tok::Id(_))) {
                self.pos += 1;
                self.sym(",");
            }
        }
        if self.pos == self.toks.len() { Ok(q) } else { Err(BAD) }
    }

    fn expr(&mut self) -> R<Expr> {
        let mut a = self.and()?;
        while self.kw("or") {
            a = Expr::Or(Box::new(a), Box::new(self.and()?));
        }
        Ok(a)
    }
    fn and(&mut self) -> R<Expr> {
        let mut a = self.not()?;
        while self.kw("and") {
            a = Expr::And(Box::new(a), Box::new(self.not()?));
        }
        Ok(a)
    }
    fn not(&mut self) -> R<Expr> {
        if self.kw("not") { Ok(Expr::Not(Box::new(self.not()?))) } else { self.compare() }
    }
    fn compare(&mut self) -> R<Expr> {
        let a = self.add()?;
        for (s, op) in [("=", Cmp::Eq), ("!=", Cmp::Ne), ("<>", Cmp::Ne), ("<", Cmp::Lt), ("<=", Cmp::Le), (">", Cmp::Gt), (">=", Cmp::Ge)] {
            if self.sym(s) {
                return Ok(Expr::Cmp(op, Box::new(a), Box::new(self.add()?)));
            }
        }
        if self.kw("is") {
            let not = self.kw("not");
            self.need_kw("null")?;
            return Ok(Expr::IsNull(Box::new(a), not));
        }
        let op = if self.kw("contains") {
            StrOp::Contains
        } else if self.kw("starts") {
            self.need_kw("with")?;
            StrOp::Starts
        } else if self.kw("ends") {
            self.need_kw("with")?;
            StrOp::Ends
        } else if self.kw("matches") {
            StrOp::Matches
        } else if self.kw("like") {
            StrOp::Like
        } else {
            return Ok(a);
        };
        Ok(Expr::Str(op, Box::new(a), Box::new(self.add()?)))
    }
    fn add(&mut self) -> R<Expr> {
        let mut a = self.mul()?;
        loop {
            let op = if self.sym("+") {
                '+'
            } else if self.sym("-") {
                '-'
            } else {
                return Ok(a);
            };
            a = Expr::Arith(op, Box::new(a), Box::new(self.mul()?));
        }
    }
    fn mul(&mut self) -> R<Expr> {
        let mut a = self.unary()?;
        loop {
            let op = if self.sym("*") {
                '*'
            } else if self.sym("/") {
                '/'
            } else {
                return Ok(a);
            };
            a = Expr::Arith(op, Box::new(a), Box::new(self.unary()?));
        }
    }
    fn unary(&mut self) -> R<Expr> {
        if self.sym("-") { Ok(Expr::Neg(Box::new(self.unary()?))) } else { self.primary() }
    }
    fn primary(&mut self) -> R<Expr> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Lit(Q::Num(n))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Q::Str(s))),
            Some(Tok::Sym("(")) => {
                let e = self.expr()?;
                if self.sym(")") { Ok(e) } else { Err(BAD) }
            }
            Some(Tok::Quoted(id)) => self.column(&id),
            Some(Tok::Id(id)) => {
                let lower = id.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => return Ok(Expr::Lit(Q::Bool(true))),
                    "false" => return Ok(Expr::Lit(Q::Bool(false))),
                    "date" | "datetime" | "timeofday" if matches!(self.peek(), Some(Tok::Str(_))) => {
                        let s = self.string()?;
                        return Ok(Expr::Lit(Q::Num(self.temporal(&lower, &s)?)));
                    }
                    _ => {}
                }
                if self.sym("(") {
                    let mut args = Vec::new();
                    if !self.sym(")") {
                        args = self.list()?;
                        if !self.sym(")") {
                            return Err(BAD);
                        }
                    }
                    if let Some(k) = Agg::ALL.into_iter().find(|k| k.name() == lower) {
                        return match args.as_slice() {
                            [Expr::Col(i)] => Ok(Expr::Agg(k, *i)),
                            _ => Err(BAD),
                        };
                    }
                    let f = Func::ALL.into_iter().find(|f| f.name().eq_ignore_ascii_case(&lower)).ok_or(BAD)?;
                    return if args.len() == f.arity() { Ok(Expr::Func(f, args)) } else { Err(BAD) };
                }
                if RESERVED.contains(&lower.as_str()) {
                    return Err(BAD);
                }
                self.column(&id)
            }
            _ => Err(BAD),
        }
    }
    fn column(&self, id: &str) -> R<Expr> {
        self.cols.ids.iter().position(|c| c == id).map(Expr::Col).ok_or(BAD)
    }
    /// `date 'yyyy-MM-dd'`, `datetime 'yyyy-MM-dd HH:mm:ss[.SSS]'`, `timeofday 'HH:mm:ss[.SSS]'`
    /// as a serial number.
    fn temporal(&self, kind: &str, s: &str) -> R<f64> {
        let s = s.trim();
        let (date, time) = match kind {
            "date" => (Some(s), None),
            "timeofday" => (None, Some(s)),
            _ => {
                let (d, t) = s.split_once(' ').ok_or(BAD)?;
                (Some(d), Some(t.trim()))
            }
        };
        let mut serial = 0.0;
        if let Some(d) = date {
            let parts: Vec<i64> = d.split('-').map(|x| x.trim().parse::<i64>().map_err(|_| BAD)).collect::<R<_>>()?;
            let [y, m, d] = parts.as_slice() else { return Err(BAD) };
            if !(1..=12).contains(m) || !(1..=31).contains(d) {
                return Err(BAD);
            }
            serial += serial_from_ymd(self.sys, *y, *m, *d).ok_or(BAD)?;
        }
        if let Some(t) = time {
            let parts: Vec<f64> = t.split(':').map(|x| x.trim().parse::<f64>().map_err(|_| BAD)).collect::<R<_>>()?;
            let [h, m, sec] = parts.as_slice() else { return Err(BAD) };
            if !(0.0..24.0).contains(h) || !(0.0..60.0).contains(m) || !(0.0..60.0).contains(sec) {
                return Err(BAD);
            }
            serial += time_fraction(*h, *m, *sec);
        }
        Ok(serial)
    }
}

// ---------------------------------------------------------------------------------------------
// Types and labels

fn want(e: &Expr, cols: &Cols, t: Ty) -> R<()> {
    if ty(e, cols)? == t { Ok(()) } else { Err(BAD) }
}

/// The static type of an expression; a mismatch is the type error Sheets reports.
fn ty(e: &Expr, cols: &Cols) -> R<Ty> {
    let col = |i: &usize| cols.types.get(*i).copied().ok_or(BAD);
    Ok(match e {
        Expr::Col(i) | Expr::Agg(Agg::Max | Agg::Min, i) => col(i)?,
        Expr::Agg(Agg::Count, i) => {
            col(i)?;
            Ty::Num
        }
        Expr::Agg(_, i) => {
            if col(i)? != Ty::Num {
                return Err(BAD);
            }
            Ty::Num
        }
        Expr::Lit(Q::Num(_)) => Ty::Num,
        Expr::Lit(Q::Str(_) | Q::Null) => Ty::Str,
        Expr::Lit(Q::Bool(_)) => Ty::Bool,
        Expr::Neg(a) => {
            want(a, cols, Ty::Num)?;
            Ty::Num
        }
        Expr::Arith(_, a, b) => {
            want(a, cols, Ty::Num)?;
            want(b, cols, Ty::Num)?;
            Ty::Num
        }
        Expr::Cmp(_, a, b) => {
            want(b, cols, ty(a, cols)?)?;
            Ty::Bool
        }
        Expr::Str(_, a, b) => {
            want(a, cols, Ty::Str)?;
            want(b, cols, Ty::Str)?;
            Ty::Bool
        }
        Expr::IsNull(a, _) => {
            ty(a, cols)?;
            Ty::Bool
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            want(a, cols, Ty::Bool)?;
            want(b, cols, Ty::Bool)?;
            Ty::Bool
        }
        Expr::Not(a) => {
            want(a, cols, Ty::Bool)?;
            Ty::Bool
        }
        Expr::Func(f, xs) => {
            let t = if matches!(f, Func::Upper | Func::Lower) { Ty::Str } else { Ty::Num };
            for x in xs {
                want(x, cols, t)?;
            }
            t
        }
    })
}

/// The expression as text, a column by its header label (or its id when it has none).
fn render(e: &Expr, cols: &Cols) -> String {
    let r = |x: &Expr| render(x, cols);
    match e {
        Expr::Col(i) => match (cols.labels.get(*i), cols.ids.get(*i)) {
            (Some(l), _) if !l.is_empty() => l.clone(),
            (_, Some(id)) => id.clone(),
            _ => String::new(),
        },
        Expr::Lit(Q::Str(s)) => format!("'{s}'"),
        Expr::Lit(q) => show(q),
        Expr::Neg(a) => format!("-{}", r(a)),
        Expr::Arith(op, a, b) => format!("{}{op}{}", r(a), r(b)),
        Expr::Cmp(op, a, b) => {
            let s = match op {
                Cmp::Eq => "=",
                Cmp::Ne => "!=",
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Gt => ">",
                Cmp::Ge => ">=",
            };
            format!("{}{s}{}", r(a), r(b))
        }
        Expr::Str(op, a, b) => {
            let s = match op {
                StrOp::Contains => "contains",
                StrOp::Starts => "starts with",
                StrOp::Ends => "ends with",
                StrOp::Matches => "matches",
                StrOp::Like => "like",
            };
            format!("{} {s} {}", r(a), r(b))
        }
        Expr::IsNull(a, not) => format!("{} is {}null", r(a), if *not { "not " } else { "" }),
        Expr::And(a, b) => format!("{} and {}", r(a), r(b)),
        Expr::Or(a, b) => format!("{} or {}", r(a), r(b)),
        Expr::Not(a) => format!("not {}", r(a)),
        Expr::Func(f, xs) => format!("{}({})", f.name(), xs.iter().map(r).collect::<Vec<_>>().join(", ")),
        Expr::Agg(k, i) => format!("{}({})", k.name(), r(&Expr::Col(*i))),
    }
}

/// Sheets' default header: the column's label, `sum Salary` for an aggregate, else the text.
fn default_label(e: &Expr, cols: &Cols) -> String {
    match e {
        Expr::Col(i) => cols.labels.get(*i).cloned().unwrap_or_default(),
        Expr::Agg(k, i) => format!("{} {}", k.name(), cols.labels.get(*i).map_or("", String::as_str)).trim_end().to_string(),
        _ => render(e, cols),
    }
}

// ---------------------------------------------------------------------------------------------
// Data

fn cell_ty(v: &Value) -> Option<Ty> {
    match v {
        Value::Number(_) => Some(Ty::Num),
        Value::Text(s) if !s.is_empty() => Some(Ty::Str),
        Value::Bool(_) => Some(Ty::Bool),
        _ => None,
    }
}

/// The majority type of column `col` from row `from` down (ties: number, string, boolean).
fn majority(a: &Array, from: usize, col: usize) -> Ty {
    let mut n = [0usize; 3];
    for r in from..a.rows {
        match a.get(r, col).and_then(cell_ty) {
            Some(Ty::Num) => n[0] += 1,
            Some(Ty::Str) => n[1] += 1,
            Some(Ty::Bool) => n[2] += 1,
            None => {}
        }
    }
    if n == [0, 0, 0] {
        Ty::Str
    } else if n[0] >= n[1] && n[0] >= n[2] {
        Ty::Num
    } else if n[1] >= n[2] {
        Ty::Str
    } else {
        Ty::Bool
    }
}

/// `headers` omitted or -1: one header row when the first row is text over a column whose
/// values below are not text.
fn guess_headers(a: &Array) -> usize {
    if a.rows < 2 {
        return 0;
    }
    let text = |c: usize| matches!(a.get(0, c), Some(Value::Text(s)) if !s.is_empty());
    let first_is_text = (0..a.cols).all(|c| text(c) || matches!(a.get(0, c), Some(Value::Empty))) && (0..a.cols).any(text);
    usize::from(first_is_text && (0..a.cols).any(|c| text(c) && majority(a, 1, c) != Ty::Str))
}

/// A cell under its column's type: another type is null; a blank in a string column is ''.
fn cell_q(v: &Value, t: Ty) -> Q {
    match (v, t) {
        (Value::Number(n), Ty::Num) => Q::Num(*n),
        (Value::Text(s), Ty::Str) => Q::Str(s.to_string()),
        (Value::Empty, Ty::Str) => Q::Str(String::new()),
        (Value::Bool(b), Ty::Bool) => Q::Bool(*b),
        _ => Q::Null,
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation

struct Env {
    sys: DateSystem,
    now: f64,
    regex: HashMap<(bool, String), Regex>,
}

fn aggregate(k: Agg, vals: Vec<&Q>) -> Q {
    match k {
        Agg::Count => Q::Num(vals.len() as f64),
        Agg::Sum | Agg::Avg => {
            let xs: Vec<f64> = vals.iter().filter_map(|q| if let Q::Num(x) = q { Some(*x) } else { None }).collect();
            if xs.is_empty() {
                return Q::Null;
            }
            let s: f64 = xs.iter().sum();
            Q::Num(if k == Agg::Sum { s } else { s / xs.len() as f64 })
        }
        Agg::Max => vals.into_iter().max_by(|a, b| qcmp(a, b)).cloned().unwrap_or(Q::Null),
        Agg::Min => vals.into_iter().min_by(|a, b| qcmp(a, b)).cloned().unwrap_or(Q::Null),
    }
}

/// Three-valued truth: `None` is null.
fn truth(q: &Q) -> Option<bool> {
    if let Q::Bool(b) = q { Some(*b) } else { None }
}

impl Env {
    /// `matches` is a whole-string regular expression; `like` has `%` and `_` wildcards.
    fn regex(&mut self, pat: &str, like: bool) -> R<Regex> {
        let key = (like, pat.to_string());
        if let Some(r) = self.regex.get(&key) {
            return Ok(r.clone());
        }
        let body = if like {
            pat.chars()
                .map(|c| match c {
                    '%' => ".*".to_string(),
                    '_' => ".".to_string(),
                    c => regex::escape(&c.to_string()),
                })
                .collect::<String>()
        } else {
            pat.to_string()
        };
        let r = Regex::new(&format!("^(?s:{body})$")).map_err(|_| BAD)?;
        self.regex.insert(key, r.clone());
        Ok(r)
    }

    /// Evaluates `e` on `row`; aggregates run over `group`.
    fn eval(&mut self, e: &Expr, row: &[Q], group: &[&[Q]]) -> R<Q> {
        Ok(match e {
            Expr::Col(i) => row.get(*i).cloned().unwrap_or(Q::Null),
            Expr::Lit(q) => q.clone(),
            Expr::Agg(k, i) => aggregate(*k, group.iter().filter_map(|r| r.get(*i)).filter(|q| **q != Q::Null).collect()),
            Expr::Neg(a) => match self.eval(a, row, group)? {
                Q::Num(x) => Q::Num(-x),
                _ => Q::Null,
            },
            Expr::Arith(op, a, b) => match (self.eval(a, row, group)?, self.eval(b, row, group)?) {
                (Q::Num(x), Q::Num(y)) => {
                    let r = match op {
                        '+' => x + y,
                        '-' => x - y,
                        '*' => x * y,
                        _ => x / y,
                    };
                    if r.is_finite() { Q::Num(r) } else { Q::Null }
                }
                _ => Q::Null,
            },
            Expr::Cmp(op, a, b) => {
                let (x, y) = (self.eval(a, row, group)?, self.eval(b, row, group)?);
                if x == Q::Null || y == Q::Null {
                    return Ok(Q::Null);
                }
                let o = qcmp(&x, &y);
                Q::Bool(match op {
                    Cmp::Eq => o.is_eq(),
                    Cmp::Ne => o.is_ne(),
                    Cmp::Lt => o.is_lt(),
                    Cmp::Le => o.is_le(),
                    Cmp::Gt => o.is_gt(),
                    Cmp::Ge => o.is_ge(),
                })
            }
            Expr::Str(op, a, b) => match (self.eval(a, row, group)?, self.eval(b, row, group)?) {
                (Q::Str(x), Q::Str(y)) => Q::Bool(match op {
                    StrOp::Contains => x.contains(&y),
                    StrOp::Starts => x.starts_with(&y),
                    StrOp::Ends => x.ends_with(&y),
                    StrOp::Matches => self.regex(&y, false)?.is_match(&x),
                    StrOp::Like => self.regex(&y, true)?.is_match(&x),
                }),
                _ => Q::Null,
            },
            Expr::IsNull(a, not) => Q::Bool((self.eval(a, row, group)? == Q::Null) != *not),
            Expr::And(a, b) => match (truth(&self.eval(a, row, group)?), truth(&self.eval(b, row, group)?)) {
                (Some(false), _) | (_, Some(false)) => Q::Bool(false),
                (Some(true), Some(true)) => Q::Bool(true),
                _ => Q::Null,
            },
            Expr::Or(a, b) => match (truth(&self.eval(a, row, group)?), truth(&self.eval(b, row, group)?)) {
                (Some(true), _) | (_, Some(true)) => Q::Bool(true),
                (Some(false), Some(false)) => Q::Bool(false),
                _ => Q::Null,
            },
            Expr::Not(a) => match self.eval(a, row, group)? {
                Q::Bool(b) => Q::Bool(!b),
                _ => Q::Null,
            },
            Expr::Func(f, xs) => {
                let args = xs.iter().map(|x| self.eval(x, row, group)).collect::<R<Vec<_>>>()?;
                self.func(*f, &args)
            }
        })
    }

    fn func(&self, f: Func, a: &[Q]) -> Q {
        let n = |i: usize| match a.get(i) {
            Some(Q::Num(x)) => Some(*x),
            _ => None,
        };
        match f {
            Func::Now => Q::Num(self.now),
            Func::Upper | Func::Lower => match a.first() {
                Some(Q::Str(s)) => Q::Str(if f == Func::Upper { s.to_uppercase() } else { s.to_lowercase() }),
                _ => Q::Null,
            },
            Func::DateDiff => match (n(0), n(1)) {
                (Some(x), Some(y)) => Q::Num(x.floor() - y.floor()),
                _ => Q::Null,
            },
            Func::ToDate => n(0).map_or(Q::Null, |x| Q::Num(x.floor())),
            _ => {
                let Some(dt) = n(0).and_then(|x| datetime_from_serial(self.sys, x)) else { return Q::Null };
                Q::Num(match f {
                    Func::Year => f64::from(dt.year),
                    Func::Month => f64::from(dt.month),
                    Func::Day => f64::from(dt.day),
                    Func::Hour => f64::from(dt.hour),
                    Func::Minute => f64::from(dt.minute),
                    Func::Second => f64::from(dt.second),
                    Func::Millisecond => f64::from(dt.milli),
                    Func::Quarter => f64::from((dt.month - 1) / 3 + 1),
                    _ => f64::from(dt.weekday + 1),
                })
            }
        }
    }
}

/// Runs a parsed query over the typed rows: the header labels and the (order keys, values) of
/// each result row.
fn run(q: &Query, cols: &Cols, rows: Vec<Vec<Q>>, env: &mut Env) -> R<(Vec<String>, Vec<(Vec<Q>, Vec<Q>)>)> {
    let select: Vec<Expr> = q.select.clone().unwrap_or_else(|| (0..cols.ids.len()).map(Expr::Col).collect());
    let order: Vec<&Expr> = q.order.iter().map(|(e, _)| e).collect();
    for e in select.iter().chain(&q.group).chain(&q.pivot).chain(order.iter().copied()).chain(q.filter.iter()) {
        ty(e, cols)?;
    }
    if let Some(w) = &q.filter
        && (w.has_agg() || ty(w, cols)? != Ty::Bool)
    {
        return Err(BAD);
    }
    if q.group.iter().chain(&q.pivot).any(Expr::has_agg) || q.labels.iter().any(|(e, _)| !select.contains(e)) {
        return Err(BAD);
    }
    let grouped = !q.group.is_empty() || !q.pivot.is_empty() || select.iter().any(Expr::has_agg);
    if grouped {
        if q.select.is_none() || !select.iter().chain(order.iter().copied()).all(|e| e.grouped_ok(&q.group)) {
            return Err(BAD);
        }
    } else if order.iter().any(|e| e.has_agg()) {
        return Err(BAD);
    }
    if !q.pivot.is_empty() && (!select.iter().any(Expr::has_agg) || order.iter().any(|e| e.has_agg())) {
        return Err(BAD);
    }
    let label_of = |e: &Expr| q.labels.iter().find(|(l, _)| l == e).map_or_else(|| default_label(e, cols), |(_, s)| s.clone());

    let mut kept = Vec::new();
    for r in rows {
        if let Some(w) = &q.filter
            && env.eval(w, &r, &[])? != Q::Bool(true)
        {
            continue;
        }
        kept.push(r);
    }

    if !grouped {
        let header = select.iter().map(label_of).collect();
        let mut body = Vec::with_capacity(kept.len());
        for r in &kept {
            let keys = order.iter().map(|e| env.eval(e, r, &[])).collect::<R<Vec<_>>>()?;
            let vals = select.iter().map(|e| env.eval(e, r, &[])).collect::<R<Vec<_>>>()?;
            body.push((keys, vals));
        }
        return Ok((header, body));
    }

    // Each row's group key and pivot key; groups come out in ascending key order.
    let mut keyed: Vec<(Vec<Q>, Vec<Q>, &[Q])> = Vec::with_capacity(kept.len());
    for r in &kept {
        let g = q.group.iter().map(|e| env.eval(e, r, &[])).collect::<R<Vec<_>>>()?;
        let p = q.pivot.iter().map(|e| env.eval(e, r, &[])).collect::<R<Vec<_>>>()?;
        keyed.push((g, p, r.as_slice()));
    }
    keyed.sort_by(|a, b| keys_cmp(&a.0, &b.0));
    let mut groups: Vec<Vec<(&[Q], &[Q])>> = Vec::new();
    let mut last: Option<&[Q]> = None;
    for (g, p, r) in &keyed {
        match groups.last_mut() {
            Some(cur) if last.is_some_and(|l| keys_cmp(l, g).is_eq()) => cur.push((p, r)),
            _ => groups.push(vec![(p, r)]),
        }
        last = Some(g);
    }
    if q.group.is_empty() && groups.is_empty() {
        groups.push(Vec::new());
    }
    let blank = vec![Q::Null; cols.ids.len()];

    let mut pivots: Vec<&[Q]> = keyed.iter().map(|(_, p, _)| p.as_slice()).collect();
    pivots.sort_by(|a, b| keys_cmp(a, b));
    pivots.dedup_by(|a, b| keys_cmp(a, b).is_eq());
    let plain: Vec<&Expr> = if q.pivot.is_empty() { select.iter().collect() } else { select.iter().filter(|e| !e.has_agg()).collect() };
    let aggs: Vec<&Expr> = if q.pivot.is_empty() { Vec::new() } else { select.iter().filter(|e| e.has_agg()).collect() };

    let mut header: Vec<String> = plain.iter().map(|e| label_of(e)).collect();
    for p in &pivots {
        let name = p.iter().map(show).collect::<Vec<_>>().join(", ");
        for a in &aggs {
            header.push(if aggs.len() == 1 { name.clone() } else { format!("{name} {}", label_of(a)) });
        }
    }
    let mut body = Vec::with_capacity(groups.len());
    for g in &groups {
        let rows: Vec<&[Q]> = g.iter().map(|(_, r)| *r).collect();
        let rep = rows.first().copied().unwrap_or(&blank);
        let keys = order.iter().map(|e| env.eval(e, rep, &rows)).collect::<R<Vec<_>>>()?;
        let mut vals = plain.iter().map(|e| env.eval(e, rep, &rows)).collect::<R<Vec<_>>>()?;
        for p in &pivots {
            let sub: Vec<&[Q]> = g.iter().filter(|(k, _)| keys_cmp(k, p).is_eq()).map(|(_, r)| *r).collect();
            for a in &aggs {
                vals.push(match sub.first() {
                    Some(first) => env.eval(a, first, &sub)?,
                    None => Q::Null,
                });
            }
        }
        body.push((keys, vals));
    }
    Ok((header, body))
}

fn query(args: &[Arg], c: &mut dyn Ctx) -> R<Value> {
    let data = arg(args, 0)?;
    if let Value::Error(e) = data.value {
        return Err(e);
    }
    let text = match arg(args, 1)?.value.scalar() {
        Value::Error(e) => return Err(e),
        v => v.to_text()?,
    };
    let headers = if has(args, 2) {
        let h = num(args, 2)?.trunc();
        if h == -1.0 {
            None
        } else if (0.0..1e9).contains(&h) {
            Some(h as usize)
        } else {
            return Err(BAD);
        }
    } else {
        None
    };
    let a = as_array(&data.value);
    let h = headers.unwrap_or_else(|| guess_headers(&a)).min(a.rows);
    let types: Vec<Ty> = (0..a.cols).map(|col| majority(&a, h, col)).collect();
    let labels = (0..a.cols)
        .map(|col| (0..h).filter_map(|r| a.get(r, col)).map(Value::display).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "))
        .collect();
    // A range's columns are the sheet's own letters, from where the range starts.
    let first = data.origin.map_or(0, |o| o.col);
    let ids = (0..a.cols).map(|i| if data.from_ref { col_to_letters(first + i as u32) } else { format!("Col{}", i + 1) }).collect();
    let rows: Vec<Vec<Q>> =
        (h..a.rows).map(|r| types.iter().enumerate().map(|(col, t)| a.get(r, col).map_or(Q::Null, |v| cell_q(v, *t))).collect()).collect();
    let cols = Cols { ids, labels, types };
    let sys = c.date_system();
    let parsed = Parser { toks: lex(&text)?, pos: 0, cols: &cols, sys }.query()?;
    let mut env = Env { sys, now: c.now_serial(), regex: HashMap::new() };
    let (header, mut body) = run(&parsed, &cols, rows, &mut env)?;

    body.sort_by(|a, b| {
        for (i, (_, desc)) in parsed.order.iter().enumerate() {
            let o = match (a.0.get(i), b.0.get(i)) {
                (Some(x), Some(y)) => qcmp(x, y),
                _ => Ordering::Equal,
            };
            let o = if *desc { o.reverse() } else { o };
            if o.is_ne() {
                return o;
            }
        }
        Ordering::Equal
    });
    let width = header.len();
    let mut cells = Vec::new();
    if header.iter().any(|s| !s.is_empty()) {
        cells.extend(header.iter().map(|s| if s.is_empty() { Value::Empty } else { Value::text(s.as_str()) }));
    }
    for (_, vals) in body.iter().skip(parsed.offset).take(parsed.limit.unwrap_or(usize::MAX)) {
        cells.extend(vals.iter().map(out));
    }
    if width == 0 || cells.is_empty() {
        return Err(CellError::NA);
    }
    array_val(cells.len() / width, width, cells)
}

pub(crate) fn specs() -> Vec<FnSpec> {
    vec![f!(
        "QUERY",
        2,
        3,
        Lookup,
        A,
        "QUERY(data, query, [headers])",
        "Runs a Google Visualization API query (select, where, group by, pivot, order by, limit, offset, label) over a range or array.",
        query
    )]
}

#[cfg(test)]
mod tests {
    use gridcraft_core::{CellError, Value};

    use crate::util::testutil::*;

    fn staff() -> Value {
        let b = Value::Bool;
        arr(vec![
            vec![tv("Name"), tv("Dept"), tv("Salary"), tv("Age"), tv("Senior")],
            vec![tv("John"), tv("Eng"), nv(1000.0), nv(35.0), b(true)],
            vec![tv("Dave"), tv("Eng"), nv(500.0), nv(27.0), b(false)],
            vec![tv("Sally"), tv("Eng"), nv(600.0), nv(30.0), b(false)],
            vec![tv("Ben"), tv("Sales"), nv(400.0), nv(32.0), b(true)],
            vec![tv("Dana"), tv("Sales"), nv(350.0), nv(25.0), b(false)],
            vec![tv("Mike"), tv("Marketing"), nv(800.0), nv(40.0), b(true)],
        ])
    }

    fn q(data: Value, text: &str) -> Value {
        ev("QUERY", vec![rf(data), t(text)])
    }

    fn q_h(data: Value, text: &str, headers: f64) -> Value {
        ev("QUERY", vec![rf(data), t(text), n(headers)])
    }

    fn texts(v: &Value) -> Vec<Vec<String>> {
        rows_of(v).iter().map(|r| r.iter().map(Value::display).collect()).collect()
    }

    fn names(v: &Value) -> Vec<String> {
        texts(v).into_iter().map(|r| r.join("|")).collect()
    }

    #[test]
    fn select_star_and_empty_query_guess_one_header_row() {
        let all = q(staff(), "select *");
        assert_eq!(rows_of(&all).len(), 7);
        assert_eq!(rows_of(&all)[0][0], tv("Name"));
        assert_eq!(rows_of(&all)[1][2], nv(1000.0));
        assert_eq!(rows_of(&all)[1][4], Value::Bool(true));
        assert_eq!(q(staff(), ""), all);
    }

    #[test]
    fn select_columns_where_comparison() {
        assert_eq!(names(&q(staff(), "select A, C where C > 500")), ["Name|Salary", "John|1000", "Sally|600", "Mike|800"]);
        assert_eq!(names(&q(staff(), "SELECT A WHERE C >= 600 AND C <= 800")), ["Name", "Sally", "Mike"]);
        assert_eq!(names(&q(staff(), "select A where B != 'Eng'")), ["Name", "Ben", "Dana", "Mike"]);
        assert_eq!(names(&q(staff(), "select A where B <> 'Eng' and E = true")), ["Name", "Ben", "Mike"]);
    }

    #[test]
    fn and_or_not_and_parentheses() {
        assert_eq!(names(&q(staff(), "select A where (B = 'Eng' and C < 700) or not E")), ["Name", "Dave", "Sally", "Dana"]);
    }

    #[test]
    fn string_operators_are_case_sensitive() {
        assert_eq!(names(&q(staff(), "select A where A contains 'a'")), ["Name", "Dave", "Sally", "Dana"]);
        assert_eq!(names(&q(staff(), "select A where A starts with 'D'")), ["Name", "Dave", "Dana"]);
        assert_eq!(names(&q(staff(), "select A where A ends with 'e'")), ["Name", "Dave", "Mike"]);
        assert_eq!(names(&q(staff(), "select A where A like 'S%'")), ["Name", "Sally"]);
        assert_eq!(names(&q(staff(), "select A where A like '_e%'")), ["Name", "Ben"]);
        assert_eq!(names(&q(staff(), "select A where A matches '[BM].*'")), ["Name", "Ben", "Mike"]);
        // `=` is case-sensitive: no match leaves the header row alone.
        assert_eq!(names(&q(staff(), "select A where A = 'john'")), ["Name"]);
    }

    #[test]
    fn group_by_with_default_aggregate_labels() {
        assert_eq!(names(&q(staff(), "select B, sum(C) group by B")), ["Dept|sum Salary", "Eng|2100", "Marketing|800", "Sales|750"]);
        let v = q(staff(), "select B, avg(D), count(A), max(C), min(A) group by B");
        assert_eq!(texts(&v)[0], ["Dept", "avg Age", "count Name", "max Salary", "min Name"]);
        assert_eq!(texts(&v)[3], ["Sales", "28.5", "2", "400", "Ben"]);
        close(rows_of(&v)[1][1].clone(), 92.0 / 3.0);
    }

    #[test]
    fn aggregate_without_group_by_is_one_row() {
        assert_eq!(names(&q(staff(), "select count(A) where C > 5000")), ["count Name", "0"]);
        assert_eq!(names(&q(staff(), "select sum(C), max(D)")), ["sum Salary|max Age", "3650|40"]);
    }

    #[test]
    fn pivot() {
        assert_eq!(names(&q(staff(), "select sum(C) pivot B")), ["Eng|Marketing|Sales", "2100|800|750"]);
        let v = q(staff(), "select E, count(A) group by E pivot B");
        assert_eq!(names(&v), ["Senior|Eng|Marketing|Sales", "FALSE|2||1", "TRUE|1|1|1"]);
        assert_eq!(rows_of(&v)[1][2], Value::Empty);
    }

    #[test]
    fn order_by_limit_offset() {
        assert_eq!(names(&q(staff(), "select A order by C desc limit 2 offset 1")), ["Name", "Mike", "Sally"]);
        assert_eq!(names(&q(staff(), "select A, B order by B, D desc limit 3")), ["Name|Dept", "John|Eng", "Sally|Eng", "Dave|Eng"]);
        assert_eq!(
            names(&q(staff(), "select B, sum(C) group by B order by sum(C) desc")),
            ["Dept|sum Salary", "Eng|2100", "Marketing|800", "Sales|750"]
        );
        assert_eq!(names(&q(staff(), "select A order by A limit 0")), ["Name"]);
    }

    #[test]
    fn label_renames_and_an_empty_label_drops_the_header_row() {
        assert_eq!(texts(&q(staff(), "select B, avg(D) group by B label avg(D) 'Mean age'"))[0], ["Dept", "Mean age"]);
        assert_eq!(names(&q(staff(), "select A, C where D > 35 label A 'Who', C 'Pay'")), ["Who|Pay", "Mike|800"]);
        assert_eq!(names(&q(staff(), "select A where D > 35 label A ''")), ["Mike"]);
    }

    #[test]
    fn format_and_options_parse() {
        assert_eq!(names(&q(staff(), "select A, C where C > 900 format C '#,##0' options no_format")), ["Name|Salary", "John|1000"]);
    }

    #[test]
    fn arithmetic_and_scalar_functions() {
        let v = q(staff(), "select A, C * 2 + 1 where D < 30");
        assert_eq!(texts(&v)[1..], [vec!["Dave", "1001"], vec!["Dana", "701"]]);
        assert_eq!(texts(&q(staff(), "select upper(A), lower(B) where C = 1000"))[1], ["JOHN", "eng"]);
        assert_eq!(texts(&q(staff(), "select C / 0 where C = 1000"))[1], [""]);
    }

    #[test]
    fn explicit_headers() {
        let none = q_h(staff(), "select A limit 2", 0.0);
        assert_eq!(names(&none), ["Name", "John"]);
        let two = arr(vec![vec![tv("First"), tv("Pay")], vec![tv("name"), tv("amt")], vec![tv("a"), nv(1.0)], vec![tv("b"), nv(2.0)]]);
        assert_eq!(names(&q_h(two, "select A, B", 2.0)), ["First name|Pay amt", "a|1", "b|2"]);
        let text_only = arr(vec![vec![tv("x"), tv("y")], vec![tv("z"), tv("w")]]);
        assert_eq!(names(&q(text_only, "select *")), ["x|y", "z|w"]);
        is_err(q_h(staff(), "select A", -2.0), CellError::Value);
    }

    #[test]
    fn minority_types_are_null() {
        let mixed = arr(vec![vec![nv(1.0)], vec![nv(2.0)], vec![tv("x")], vec![nv(4.0)]]);
        assert_eq!(rows_of(&q_h(mixed.clone(), "select A where A is null", 0.0)), [[Value::Empty]]);
        assert_eq!(names(&q_h(mixed.clone(), "select A where A is not null", 0.0)), ["1", "2", "4"]);
        assert_eq!(names(&q_h(mixed, "select sum(A)", 0.0)), ["sum", "7"]);
    }

    #[test]
    fn array_data_uses_col_ids() {
        let data = arr(vec![vec![nv(1.0), tv("a")], vec![nv(2.0), tv("b")], vec![nv(3.0), tv("c")]]);
        let v = ev("QUERY", vec![av(data.clone()), t("select Col2 where Col1 > 1")]);
        assert_eq!(names(&v), ["b", "c"]);
        is_err(ev("QUERY", vec![av(data), t("select A")]), CellError::Value);
        is_err(q(staff(), "select Col1"), CellError::Value);
    }

    #[test]
    fn dates() {
        // 2024-01-15 and 2024-05-03 (a Friday).
        let d = arr(vec![vec![nv(45306.0)], vec![nv(45415.0)]]);
        let v = q_h(d.clone(), "select year(A), month(A), day(A), quarter(A), dayOfWeek(A) where A > date '2024-02-01'", 0.0);
        assert_eq!(texts(&v)[1], ["2024", "5", "3", "2", "6"]);
        let v = q_h(d.clone(), "select dateDiff(A, date '2024-01-01') where A < datetime '2024-01-15 12:00:00'", 0.0);
        assert_eq!(texts(&v)[1], ["14"]);
        let t = arr(vec![vec![nv(45306.75)]]);
        assert_eq!(texts(&q_h(t, "select hour(A), minute(A), toDate(A)", 0.0))[1], ["18", "0", "45306"]);
        assert_eq!(names(&q_h(d, "select A where A < timeofday '12:00:00' + 45307", 0.0)), ["45306"]);
    }

    #[test]
    fn errors() {
        is_err(q(staff(), "selec A"), CellError::Value);
        is_err(q(staff(), "select Z"), CellError::Value);
        is_err(q(staff(), "select A where C > 'x'"), CellError::Value);
        is_err(q(staff(), "select A, sum(C)"), CellError::Value);
        is_err(q(staff(), "select sum(A)"), CellError::Value);
        is_err(q(staff(), "select A where sum(C) > 1"), CellError::Value);
        is_err(q(staff(), "select A order by C limit 1 where C > 1"), CellError::Value);
        is_err(q(staff(), "select A where A matches '('"), CellError::Value);
        is_err(q(staff(), "select A where A = 'unterminated"), CellError::Value);
        // No rows and no header row.
        is_err(q_h(staff(), "select A where C > 5000", 0.0), CellError::NA);
        is_err(ev("QUERY", vec![av(Value::Error(CellError::Div0)), t("select *")]), CellError::Div0);
    }
}
