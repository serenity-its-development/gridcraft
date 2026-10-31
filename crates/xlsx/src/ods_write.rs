//! OpenDocument Spreadsheet (`.ods`) export, written from ODF 1.3 (part 3 for the package and
//! the table schema, part 4 for OpenFormula).
//!
//! What is written: every sheet (name, visibility, tab order), cell values with their types
//! (number, percentage, date, time, boolean, text, error), formulas in OpenFormula with their saved
//! results, merges, column widths and row heights (and hidden ones), cell styles (font face,
//! size, bold, italic, underline, strike-through, colour, fill, borders, alignment, wrap,
//! rotation) and number formats (numbers, percentages, scientific, currency text, dates, times,
//! text), workbook-scoped defined names, and document properties. What isn't: charts, images,
//! shapes, conditional formats, validations, pivot tables, comments and print setup. A formula
//! OpenFormula can't spell (structured references, spill `#`) is saved as its value.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;

use gridcraft_core::date::datetime_from_serial;
use gridcraft_core::{CellRef, DateSystem, Value, number_to_text};
use gridcraft_model::style::{BorderLine, BorderStyle, Color, HAlign, PatternType, Style, Underline, VAlign, VertAlign};
use gridcraft_model::{Cell, Sheet, StyleId, Workbook};

use crate::IoError;
use crate::xml::{esc, esc_attr};

const MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";

const NS: &str = concat!(
    r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" "#,
    r#"xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" "#,
    r#"xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" "#,
    r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
    r#"xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" "#,
    r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
    r#"xmlns:tableooo="http://openoffice.org/2009/table" "#,
    r#"office:version="1.3""#
);

/// Encodes `wb` as an `.ods` package.
pub fn write_ods(wb: &Workbook) -> Result<Vec<u8>, IoError> {
    let mut styles = StyleSet::default();
    let body = body(wb, &mut styles);
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:font-face-decls>{}</office:font-face-decls><office:automatic-styles>{}</office:automatic-styles><office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>"#,
        styles.font_faces(),
        styles.xml
    );
    let styles_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:styles><style:default-style style:family="table-cell"><style:text-properties style:font-name="{f}" fo:font-size="{s}pt"/></style:default-style><style:style style:name="Default" style:family="table-cell"/></office:styles></office:document-styles>"#,
        f = esc_attr(&default_font(wb).0),
        s = num(f64::from(default_font(wb).1)),
    );
    let p = &wb.props;
    let mut meta_body = String::from("<meta:generator>GridCraft</meta:generator>");
    for (tag, v) in [("dc:title", &p.title), ("dc:subject", &p.subject), ("meta:initial-creator", &p.author)] {
        if !v.is_empty() {
            let _ = write!(meta_body, "<{tag}>{}</{tag}>", esc(v));
        }
    }
    let meta =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-meta {NS}><office:meta>{meta_body}</office:meta></office:document-meta>"#);
    let manifest = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">"#,
        r#"<manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>"#,
        r#"<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>"#,
        r#"<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>"#,
        r#"<manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>"#,
        r#"</manifest:manifest>"#
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let err = |e: zip::result::ZipError| IoError::Format(format!("writing the ODS package: {e}"));
        let io = |e: std::io::Error| IoError::Format(format!("writing the ODS package: {e}"));
        // The mimetype is the first entry, stored, so a reader can identify the file from its first bytes.
        z.start_file("mimetype", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).map_err(err)?;
        z.write_all(MIME.as_bytes()).map_err(io)?;
        let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in
            [("content.xml", content.as_str()), ("styles.xml", styles_xml.as_str()), ("meta.xml", meta.as_str()), ("META-INF/manifest.xml", manifest)]
        {
            z.start_file(name, deflate).map_err(err)?;
            z.write_all(data.as_bytes()).map_err(io)?;
        }
        z.finish().map_err(err)?;
    }
    Ok(buf.into_inner())
}

fn default_font(wb: &Workbook) -> (String, f32) {
    let f = &wb.styles.get(StyleId::DEFAULT).font;
    (f.name.clone(), f.size)
}

/// A length in points as ODF writes it.
fn num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" { "0".into() } else { s.to_string() }
}

/// Pixels at 96 dpi → `…pt`.
fn px_pt(px: f32) -> String {
    format!("{}pt", num(f64::from(px) * 0.75))
}

// ------------------------------------------------------------------------------------------ styles

/// The automatic styles of `content.xml`, each written once and named by what it says.
#[derive(Default)]
struct StyleSet {
    xml: String,
    cols: HashMap<u32, String>,
    rows: HashMap<(u32, bool), String>,
    tables: HashMap<(bool, Option<u32>), String>,
    cells: HashMap<StyleId, Option<String>>,
    data: HashMap<String, Option<String>>,
    fonts: Vec<String>,
}

impl StyleSet {
    fn font_faces(&self) -> String {
        self.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="{0}"/>"#, esc_attr(f))).collect()
    }

    fn font(&mut self, name: &str) -> String {
        if !self.fonts.iter().any(|f| f == name) {
            self.fonts.push(name.to_string());
        }
        name.to_string()
    }

    fn table(&mut self, visible: bool, tab: Option<u32>) -> String {
        let n = self.tables.len();
        self.tables
            .entry((visible, tab))
            .or_insert_with(|| {
                let name = format!("ta{}", n + 1);
                let color = tab.map(|c| format!(r##" tableooo:tab-color="#{c:06x}""##)).unwrap_or_default();
                let _ = write!(self.xml, r#"<style:style style:name="{name}" style:family="table"><style:table-properties table:display="{visible}"{color}/></style:style>"#);
                name
            })
            .clone()
    }

    fn col(&mut self, width_px: f32) -> String {
        let key = (width_px * 100.0).round() as u32;
        let n = self.cols.len();
        self.cols
            .entry(key)
            .or_insert_with(|| {
                let name = format!("co{}", n + 1);
                let _ = write!(self.xml, r#"<style:style style:name="{name}" style:family="table-column"><style:table-column-properties fo:break-before="auto" style:column-width="{}"/></style:style>"#, px_pt(width_px));
                name
            })
            .clone()
    }

    fn row(&mut self, height_px: f32, custom: bool) -> String {
        let key = ((height_px * 100.0).round() as u32, custom);
        let n = self.rows.len();
        self.rows
            .entry(key)
            .or_insert_with(|| {
                let name = format!("ro{}", n + 1);
                let _ = write!(
                    self.xml,
                    r#"<style:style style:name="{name}" style:family="table-row"><style:table-row-properties style:row-height="{}" fo:break-before="auto" style:use-optimal-row-height="{}"/></style:style>"#,
                    px_pt(height_px),
                    !custom
                );
                name
            })
            .clone()
    }

    /// The cell style for `id`, `None` for the default look.
    fn cell(&mut self, wb: &Workbook, id: StyleId) -> Option<String> {
        if let Some(n) = self.cells.get(&id) {
            return n.clone();
        }
        let st = wb.styles.get(id);
        let base = wb.styles.get(StyleId::DEFAULT);
        let data = self.data_style(st.num_fmt.as_str());
        let mut cell_props = String::new();
        let mut para_props = String::new();
        let mut text_props = String::new();
        // Fill.
        if st.fill.pattern != PatternType::None
            && let Some(c) = hex(if st.fill.pattern == PatternType::Solid { st.fill.fg } else { st.fill.bg }, wb).or_else(|| hex(st.fill.fg, wb))
        {
            let _ = write!(cell_props, r#" fo:background-color="{c}""#);
        }
        // Borders.
        let b = &st.borders;
        let sides = [("left", &b.left), ("right", &b.right), ("top", &b.top), ("bottom", &b.bottom)];
        if sides.iter().any(|(_, l)| l.style != BorderStyle::None) {
            for (side, l) in sides {
                let _ = write!(cell_props, r#" fo:border-{side}="{}""#, border(l, wb));
            }
        }
        if b.diag_down.style != BorderStyle::None {
            let _ = write!(cell_props, r#" style:diagonal-tl-br="{}""#, border(&b.diag_down, wb));
        }
        if b.diag_up.style != BorderStyle::None {
            let _ = write!(cell_props, r#" style:diagonal-bl-tr="{}""#, border(&b.diag_up, wb));
        }
        // Alignment.
        let a = &st.align;
        if a.wrap {
            cell_props.push_str(r#" fo:wrap-option="wrap""#);
        }
        if a.shrink {
            cell_props.push_str(r#" style:shrink-to-fit="true""#);
        }
        let v = match a.v {
            VAlign::Top => Some("top"),
            VAlign::Center => Some("middle"),
            VAlign::Bottom => None,
            VAlign::Justify | VAlign::Distributed => Some("middle"),
        };
        if let Some(v) = v {
            let _ = write!(cell_props, r#" style:vertical-align="{v}""#);
        }
        if a.rotation == 255 {
            cell_props.push_str(r#" style:direction="ttb""#);
        } else if a.rotation != 0 {
            let deg = if a.rotation < 0 { 360 + i32::from(a.rotation) } else { i32::from(a.rotation) };
            let _ = write!(cell_props, r#" style:rotation-angle="{deg}""#);
        }
        let h = match a.h {
            HAlign::General => None,
            HAlign::Left => Some("start"),
            HAlign::Center | HAlign::CenterAcross => Some("center"),
            HAlign::Right => Some("end"),
            HAlign::Fill | HAlign::Justify | HAlign::Distributed => Some("justify"),
        };
        if let Some(h) = h {
            let _ = write!(para_props, r#" fo:text-align="{h}""#);
            cell_props.push_str(r#" style:text-align-source="fix""#);
        }
        if a.indent > 0 {
            let _ = write!(para_props, r#" fo:margin-left="{}pt""#, num(f64::from(a.indent) * 9.0));
        }
        // Font.
        let f = &st.font;
        if f.name != base.font.name && !f.name.is_empty() {
            let face = self.font(&f.name);
            let _ = write!(text_props, r#" style:font-name="{}""#, esc_attr(&face));
        }
        if (f.size - base.font.size).abs() > f32::EPSILON && f.size > 0.0 {
            let _ = write!(text_props, r#" fo:font-size="{}pt""#, num(f64::from(f.size)));
        }
        if f.bold {
            text_props.push_str(r#" fo:font-weight="bold""#);
        }
        if f.italic {
            text_props.push_str(r#" fo:font-style="italic""#);
        }
        match f.underline {
            Underline::None => {}
            Underline::Single | Underline::SingleAccounting => {
                text_props.push_str(r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#)
            }
            Underline::Double | Underline::DoubleAccounting => text_props.push_str(
                r#" style:text-underline-style="solid" style:text-underline-type="double" style:text-underline-width="auto" style:text-underline-color="font-color""#,
            ),
        }
        if f.strike {
            text_props.push_str(r#" style:text-line-through-style="solid""#);
        }
        match f.vert {
            VertAlign::Baseline => {}
            VertAlign::Superscript => text_props.push_str(r#" style:text-position="super 58%""#),
            VertAlign::Subscript => text_props.push_str(r#" style:text-position="sub 58%""#),
        }
        if let Some(c) = hex(f.color, wb) {
            let _ = write!(text_props, r#" fo:color="{c}""#);
        }
        if data.is_none() && cell_props.is_empty() && para_props.is_empty() && text_props.is_empty() {
            self.cells.insert(id, None);
            return None;
        }
        let name = format!("ce{}", self.cells.values().filter(|v| v.is_some()).count() + 1);
        let _ = write!(self.xml, r#"<style:style style:name="{name}" style:family="table-cell" style:parent-style-name="Default""#);
        if let Some(d) = &data {
            let _ = write!(self.xml, r#" style:data-style-name="{d}""#);
        }
        self.xml.push('>');
        if !cell_props.is_empty() {
            let _ = write!(self.xml, "<style:table-cell-properties{cell_props}/>");
        }
        if !para_props.is_empty() {
            let _ = write!(self.xml, "<style:paragraph-properties{para_props}/>");
        }
        if !text_props.is_empty() {
            let _ = write!(self.xml, "<style:text-properties{text_props}/>");
        }
        self.xml.push_str("</style:style>");
        self.cells.insert(id, Some(name.clone()));
        Some(name)
    }

    /// The `number:*-style` for an Excel number format code, `None` for General or a code this
    /// writer can't express (the value is still written with its type).
    fn data_style(&mut self, code: &str) -> Option<String> {
        if let Some(n) = self.data.get(code) {
            return n.clone();
        }
        let n = self.data.values().filter(|v| v.is_some()).count();
        let name = format!("N{}", n + 1);
        let built = data_style_xml(code, &name);
        let out = built.map(|xml| {
            self.xml.push_str(&xml);
            name
        });
        self.data.insert(code.to_string(), out.clone());
        out
    }
}

fn hex(c: Color, wb: &Workbook) -> Option<String> {
    c.resolve(&wb.theme).map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
}

fn border(l: &BorderLine, wb: &Workbook) -> String {
    if l.style == BorderStyle::None {
        return "none".into();
    }
    let kind = match l.style {
        BorderStyle::Dashed | BorderStyle::MediumDashed | BorderStyle::DashDot | BorderStyle::MediumDashDot | BorderStyle::SlantDashDot => "dashed",
        BorderStyle::Dotted | BorderStyle::Hair | BorderStyle::DashDotDot | BorderStyle::MediumDashDotDot => "dotted",
        BorderStyle::Double => "double",
        _ => "solid",
    };
    let w = f64::from(l.style.width()) * 0.75;
    format!("{}pt {kind} {}", num(w.max(0.5)), hex(l.color, wb).unwrap_or_else(|| "#000000".into()))
}

// ------------------------------------------------------------------------------- number formats

/// What a format code's first section is, as far as choosing the cell's value type goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    General,
    Number,
    Percent,
    Date,
    Time,
    Text,
}

/// Splits off the first section of a code (`;` outside quotes and brackets).
fn first_section(code: &str) -> &str {
    let mut quoted = false;
    let mut bracket = false;
    let mut escape = false;
    for (i, c) in code.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if !quoted => escape = true,
            '"' => quoted = !quoted,
            '[' if !quoted => bracket = true,
            ']' if !quoted => bracket = false,
            ';' if !quoted && !bracket => return &code[..i],
            _ => {}
        }
    }
    code
}

/// The tokens of a section: literal text, or a placeholder run (`0.00`, `yyyy`, `AM/PM`…).
#[derive(Debug, PartialEq)]
enum FTok {
    Lit(String),
    Code(String),
}

fn tokens(sec: &str) -> Vec<FTok> {
    let mut out = vec![];
    let chars: Vec<char> = sec.chars().collect();
    let mut i = 0;
    let lit = |out: &mut Vec<FTok>, s: &str| {
        if let Some(FTok::Lit(prev)) = out.last_mut() {
            prev.push_str(s);
        } else {
            out.push(FTok::Lit(s.to_string()));
        }
    };
    while let Some(&c) = chars.get(i) {
        match c {
            '"' => {
                let end = chars[i + 1..].iter().position(|c| *c == '"').map(|p| i + 1 + p).unwrap_or(chars.len());
                lit(&mut out, &chars[i + 1..end].iter().collect::<String>());
                i = end + 1;
            }
            '\\' => {
                if let Some(n) = chars.get(i + 1) {
                    lit(&mut out, &n.to_string());
                }
                i += 2;
            }
            '_' | '*' => i += 2,
            '[' => {
                let end = chars[i..].iter().position(|c| *c == ']').map(|p| i + p).unwrap_or(chars.len());
                let inner: String = chars[i + 1..end.min(chars.len())].iter().collect();
                // A currency `[$€-407]` shows its symbol; colours and conditions show nothing.
                if let Some(sym) = inner.strip_prefix('$') {
                    lit(&mut out, sym.split('-').next().unwrap_or(""));
                } else if matches!(inner.to_ascii_lowercase().as_str(), "h" | "hh" | "m" | "mm" | "s" | "ss") {
                    out.push(FTok::Code(inner.to_ascii_lowercase()));
                }
                i = end + 1;
            }
            _ if "0#?.,%Ee+-".contains(c) && !(matches!(c, '+' | '-') && !matches!(out.last(), Some(FTok::Code(s)) if s.ends_with(['E', 'e']))) => {
                let mut run = String::new();
                while let Some(&d) = chars.get(i) {
                    let sign_after_e = matches!(d, '+' | '-') && run.ends_with(['E', 'e']);
                    if "0#?.,%Ee".contains(d) || sign_after_e {
                        run.push(d);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(FTok::Code(run));
            }
            _ if "yYmMdDhHsS".contains(c) => {
                let mut run = String::new();
                while chars.get(i).is_some_and(|d| d.eq_ignore_ascii_case(&c)) {
                    run.push(c.to_ascii_lowercase());
                    i += 1;
                }
                // Fractions of a second: `ss.00`.
                if run.starts_with('s') && chars.get(i) == Some(&'.') && chars.get(i + 1) == Some(&'0') {
                    run.push('.');
                    i += 1;
                    while chars.get(i) == Some(&'0') {
                        run.push('0');
                        i += 1;
                    }
                }
                out.push(FTok::Code(run));
            }
            'A' | 'a' if chars[i..].iter().take(5).collect::<String>().eq_ignore_ascii_case("am/pm") => {
                out.push(FTok::Code("ampm".into()));
                i += 5;
            }
            'A' | 'a' if chars[i..].iter().take(3).collect::<String>().eq_ignore_ascii_case("a/p") => {
                out.push(FTok::Code("ampm".into()));
                i += 3;
            }
            '@' => {
                out.push(FTok::Code("@".into()));
                i += 1;
            }
            _ => {
                lit(&mut out, &c.to_string());
                i += 1;
            }
        }
    }
    out
}

fn kind_of(code: &str) -> Kind {
    if code.is_empty() || code.eq_ignore_ascii_case("General") {
        return Kind::General;
    }
    let toks = tokens(first_section(code));
    let codes: Vec<&str> = toks.iter().filter_map(|t| if let FTok::Code(c) = t { Some(c.as_str()) } else { None }).collect();
    if codes.iter().any(|c| c.starts_with(['y', 'd']) || *c == "mmm" || *c == "mmmm") {
        return Kind::Date;
    }
    if codes.iter().any(|c| c.starts_with(['h', 's']) || *c == "ampm") {
        // `m` beside h or s is minutes: a time.
        return Kind::Time;
    }
    if codes.iter().any(|c| c.starts_with('m')) {
        return Kind::Date;
    }
    if codes.contains(&"@") && !codes.iter().any(|c| c.contains(['0', '#'])) {
        return Kind::Text;
    }
    if codes.iter().any(|c| c.contains('%')) {
        return Kind::Percent;
    }
    if codes.iter().any(|c| c.contains(['0', '#', '?'])) {
        return Kind::Number;
    }
    Kind::General
}

fn data_style_xml(code: &str, name: &str) -> Option<String> {
    let kind = kind_of(code);
    let toks = tokens(first_section(code));
    let text = |s: &str| format!("<number:text>{}</number:text>", esc(s));
    match kind {
        Kind::General => None,
        Kind::Text => {
            let mut x = format!(r#"<number:text-style style:name="{name}">"#);
            for t in &toks {
                match t {
                    FTok::Lit(s) => x.push_str(&text(s)),
                    FTok::Code(_) => x.push_str("<number:text-content/>"),
                }
            }
            x.push_str("</number:text-style>");
            Some(x)
        }
        Kind::Number | Kind::Percent => {
            let tag = if kind == Kind::Percent { "number:percentage-style" } else { "number:number-style" };
            let mut x = format!(r#"<{tag} style:name="{name}">"#);
            let mut placed = false;
            for t in &toks {
                match t {
                    FTok::Lit(s) => x.push_str(&text(s)),
                    FTok::Code(c) if !placed && c.contains(['0', '#', '?']) => {
                        placed = true;
                        let (int, frac) = c.split_once('.').unwrap_or((c.as_str(), ""));
                        let decimals = frac.chars().filter(|ch| matches!(ch, '0' | '#' | '?')).count();
                        let min_decimals = frac.chars().filter(|ch| *ch == '0').count();
                        let min_int = int.chars().filter(|ch| *ch == '0').count();
                        let grouping = int.trim_end_matches(',').contains(',');
                        if let Some((_, exp)) = c.split_once(['E', 'e']) {
                            let exp_digits = exp.chars().filter(|ch| *ch == '0').count().max(1);
                            let (mant_int, mant_frac) = int.split_once(['E', 'e']).map(|(a, _)| (a, frac)).unwrap_or((int, frac));
                            let mant_frac = mant_frac.split(['E', 'e']).next().unwrap_or("");
                            let _ = write!(
                                x,
                                r#"<number:scientific-number number:decimal-places="{}" number:min-integer-digits="{}" number:min-exponent-digits="{exp_digits}"/>"#,
                                mant_frac.chars().filter(|ch| *ch == '0').count(),
                                mant_int.chars().filter(|ch| *ch == '0').count().max(1),
                            );
                        } else {
                            let _ = write!(
                                x,
                                r#"<number:number number:decimal-places="{decimals}" number:min-decimal-places="{min_decimals}" number:min-integer-digits="{min_int}"{}/>"#,
                                if grouping { r#" number:grouping="true""# } else { "" }
                            );
                        }
                        if c.contains('%') {
                            x.push_str(&text("%"));
                        }
                    }
                    FTok::Code(c) if c.contains('%') => x.push_str(&text("%")),
                    FTok::Code(_) => {}
                }
            }
            if !placed {
                return None;
            }
            let _ = write!(x, "</{tag}>");
            Some(x)
        }
        Kind::Date | Kind::Time => {
            let tag = if kind == Kind::Date { "number:date-style" } else { "number:time-style" };
            let elapsed = code.contains("[h") || code.contains("[m") || code.contains("[s");
            let mut x = format!(
                r#"<{tag} style:name="{name}"{}>"#,
                if elapsed && kind == Kind::Time { r#" number:truncate-on-overflow="false""# } else { "" }
            );
            let codes: Vec<&str> = toks.iter().filter_map(|t| if let FTok::Code(c) = t { Some(c.as_str()) } else { None }).collect();
            let long = |c: &str| if c.len() >= 2 { r#" number:style="long""# } else { "" };
            for (i, t) in toks.iter().enumerate() {
                match t {
                    FTok::Lit(s) => x.push_str(&text(s)),
                    FTok::Code(c) => {
                        let pos = toks[..i].iter().filter(|t| matches!(t, FTok::Code(_))).count();
                        let prev = pos.checked_sub(1).and_then(|p| codes.get(p)).copied().unwrap_or("");
                        let next = codes.get(pos + 1).copied().unwrap_or("");
                        let minutes = c.starts_with('m') && c.len() <= 2 && (prev.starts_with('h') || next.starts_with('s'));
                        match c.as_str() {
                            "yy" | "y" => x.push_str("<number:year/>"),
                            c if c.starts_with('y') => x.push_str(r#"<number:year number:style="long"/>"#),
                            c if minutes => {
                                let _ = write!(x, "<number:minutes{}/>", long(c));
                            }
                            "mmmmm" | "mmmm" => x.push_str(r#"<number:month number:style="long" number:textual="true"/>"#),
                            "mmm" => x.push_str(r#"<number:month number:textual="true"/>"#),
                            c if c.starts_with('m') => {
                                let _ = write!(x, "<number:month{}/>", long(c));
                            }
                            "dddd" => x.push_str(r#"<number:day-of-week number:style="long"/>"#),
                            "ddd" => x.push_str("<number:day-of-week/>"),
                            c if c.starts_with('d') => {
                                let _ = write!(x, "<number:day{}/>", long(c));
                            }
                            c if c.starts_with('h') => {
                                let _ = write!(x, "<number:hours{}/>", long(c));
                            }
                            c if c.starts_with('s') => {
                                let places = c.split_once('.').map(|(_, f)| f.len()).unwrap_or(0);
                                let _ = write!(
                                    x,
                                    r#"<number:seconds{}{}/>"#,
                                    long(c),
                                    if places > 0 { format!(r#" number:decimal-places="{places}""#) } else { String::new() }
                                );
                            }
                            "ampm" => x.push_str("<number:am-pm/>"),
                            _ => {}
                        }
                    }
                }
            }
            let _ = write!(x, "</{tag}>");
            Some(x)
        }
    }
}

// -------------------------------------------------------------------------------------- the body

fn body(wb: &Workbook, styles: &mut StyleSet) -> String {
    let mut out = String::new();
    // Serial numbers count from the workbook's date system's epoch.
    let _ = write!(
        out,
        r#"<table:calculation-settings><table:null-date table:date-value="{}"/></table:calculation-settings>"#,
        null_date(wb.date_system)
    );
    for sh in &wb.sheets {
        sheet(wb, sh, styles, &mut out);
    }
    let names: Vec<String> = wb.names.iter().filter(|n| n.scope.is_none()).filter_map(|n| named(wb, n)).collect();
    if !names.is_empty() {
        out.push_str("<table:named-expressions>");
        for n in names {
            out.push_str(&n);
        }
        out.push_str("</table:named-expressions>");
    }
    out
}

/// A workbook-scoped name: a single reference becomes a named range, anything else a named
/// expression (when OpenFormula can spell it).
fn named(wb: &Workbook, n: &gridcraft_model::DefinedName) -> Option<String> {
    let expr = gridcraft_formula::parse(&n.formula).ok()?;
    let base = wb.sheets.first().map(|s| format!("${}.$A$1", odf_sheet_name(&s.name))).unwrap_or_else(|| "$Sheet1.$A$1".into());
    if let gridcraft_formula::Expr::Ref(r) = &expr
        && matches!(r.sheet, gridcraft_formula::SheetSel::Named(_))
    {
        let text = gridcraft_formula::print_odf(&expr)?;
        let range = text.trim_start_matches('[').trim_end_matches(']');
        return Some(format!(
            r#"<table:named-range table:name="{}" table:base-cell-address="{}" table:cell-range-address="{}"/>"#,
            esc_attr(&n.name),
            esc_attr(&base),
            esc_attr(range)
        ));
    }
    let text = gridcraft_formula::print_odf(&expr)?;
    Some(format!(
        r#"<table:named-expression table:name="{}" table:base-cell-address="{}" table:expression="of:{}"/>"#,
        esc_attr(&n.name),
        esc_attr(&base),
        esc_attr(&text)
    ))
}

fn odf_sheet_name(name: &str) -> String {
    let plain = name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit());
    if plain { name.to_string() } else { format!("'{}'", name.replace('\'', "''")) }
}

fn sheet(wb: &Workbook, sh: &Sheet, styles: &mut StyleSet, out: &mut String) {
    let visible = sh.visibility == gridcraft_model::Visibility::Visible;
    let tab = match sh.tab_color {
        Some(Color::Rgb(v)) => Some(v),
        Some(c) => c.resolve(&wb.theme).map(|[r, g, b]| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
        None => None,
    };
    let ta = styles.table(visible, tab);
    let _ = write!(out, r#"<table:table table:name="{}" table:style-name="{ta}">"#, esc_attr(&sh.name));

    // The extent written: every stored cell, merge, sized line.
    let mut last_row = 0u32;
    let mut last_col = 0u32;
    let mut any = false;
    for (c, cell) in sh.cells.iter() {
        if cell.is_blank() {
            continue;
        }
        any = true;
        last_row = last_row.max(c.row);
        last_col = last_col.max(c.col);
    }
    for m in &sh.merges {
        any = true;
        last_row = last_row.max(m.end.row);
        last_col = last_col.max(m.end.col);
    }
    if let Some((&c, _)) = sh.cols.iter().rfind(|(_, i)| i.size.is_some() || i.hidden) {
        any = true;
        last_col = last_col.max(c);
    }
    if let Some((&r, _)) = sh.rows.iter().rfind(|(_, i)| i.size.is_some() || i.hidden) {
        any = true;
        last_row = last_row.max(r);
    }
    let last_col = last_col.min(gridcraft_core::MAX_COLS.saturating_sub(1));
    let last_row = last_row.min(gridcraft_core::MAX_ROWS.saturating_sub(1));

    // Columns, run-length encoded.
    let mut col = 0u32;
    while col <= last_col {
        let (w, hidden) = col_look(sh, col);
        let mut n = 1u32;
        while col + n <= last_col && col_look(sh, col + n) == (w, hidden) {
            n += 1;
        }
        let co = styles.col(w);
        let _ = write!(
            out,
            r#"<table:table-column table:style-name="{co}"{}{} table:default-cell-style-name="Default"/>"#,
            if n > 1 { format!(r#" table:number-columns-repeated="{n}""#) } else { String::new() },
            if hidden { r#" table:visibility="collapse""# } else { "" }
        );
        col += n;
    }

    // Merged areas: their top-left cell spans, the rest are covered.
    let mut spans: HashMap<CellRef, (u32, u32)> = HashMap::new();
    let mut covered: Vec<gridcraft_core::RangeRef> = vec![];
    for m in &sh.merges {
        spans.insert(m.start, (m.end.col - m.start.col + 1, m.end.row - m.start.row + 1));
        covered.push(*m);
    }
    let is_covered = |c: CellRef| covered.iter().any(|m| m.contains(c) && m.start != c);

    let mut cells = sh.cells.iter().filter(|(_, cell)| !cell.is_blank()).peekable();
    let mut row = 0u32;
    while any && row <= last_row {
        let row_cells: Vec<(CellRef, &Cell)> = {
            let mut v = vec![];
            while let Some((c, cell)) = cells.peek() {
                if c.row != row {
                    break;
                }
                v.push((*c, *cell));
                cells.next();
            }
            v
        };
        let has_merge = covered.iter().any(|m| m.start.row <= row && row <= m.end.row);
        let look = row_look(sh, row);
        // A run of empty rows that look alike is one repeated row.
        if row_cells.is_empty() && !has_merge {
            let mut n = 1u32;
            while row + n <= last_row
                && row_look(sh, row + n) == look
                && cells.peek().is_none_or(|(c, _)| c.row > row + n)
                && !covered.iter().any(|m| m.start.row <= row + n && row + n <= m.end.row)
            {
                n += 1;
            }
            let ro = styles.row(look.0, look.2);
            let _ = write!(
                out,
                r#"<table:table-row table:style-name="{ro}"{}{}><table:table-cell table:number-columns-repeated="{}"/></table:table-row>"#,
                if n > 1 { format!(r#" table:number-rows-repeated="{n}""#) } else { String::new() },
                if look.1 { r#" table:visibility="collapse""# } else { "" },
                last_col + 1
            );
            row += n;
            continue;
        }
        let ro = styles.row(look.0, look.2);
        let _ = write!(out, r#"<table:table-row table:style-name="{ro}"{}>"#, if look.1 { r#" table:visibility="collapse""# } else { "" });
        let by_col: HashMap<u32, &Cell> = row_cells.iter().map(|(c, cell)| (c.col, *cell)).collect();
        let mut col = 0u32;
        while col <= last_col {
            let at = CellRef::new(row, col);
            if is_covered(at) {
                let mut n = 1u32;
                while col + n <= last_col && is_covered(CellRef::new(row, col + n)) && !by_col.contains_key(&(col + n)) {
                    n += 1;
                }
                let _ = write!(
                    out,
                    "<table:covered-table-cell{}/>",
                    if n > 1 { format!(r#" table:number-columns-repeated="{n}""#) } else { String::new() }
                );
                col += n;
                continue;
            }
            match by_col.get(&col) {
                Some(cell) => {
                    cell_xml(wb, at, cell, spans.get(&at).copied(), styles, out);
                    col += 1;
                }
                None => {
                    let mut n = 1u32;
                    while col + n <= last_col
                        && !by_col.contains_key(&(col + n))
                        && !is_covered(CellRef::new(row, col + n))
                        && !spans.contains_key(&CellRef::new(row, col + n))
                    {
                        n += 1;
                    }
                    if let Some((w, h)) = spans.get(&at) {
                        let _ = write!(out, r#"<table:table-cell table:number-columns-spanned="{w}" table:number-rows-spanned="{h}"/>"#);
                        col += 1;
                    } else {
                        let _ = write!(
                            out,
                            "<table:table-cell{}/>",
                            if n > 1 { format!(r#" table:number-columns-repeated="{n}""#) } else { String::new() }
                        );
                        col += n;
                    }
                }
            }
        }
        out.push_str("</table:table-row>");
        row += 1;
    }
    if !any {
        // ODF requires a column and a row in every table.
        let co = styles.col(sh.default_col_width);
        let ro = styles.row(sh.default_row_height, false);
        let _ = write!(
            out,
            r#"<table:table-column table:style-name="{co}"/><table:table-row table:style-name="{ro}"><table:table-cell/></table:table-row>"#
        );
    }
    out.push_str("</table:table>");
}

fn col_look(sh: &Sheet, col: u32) -> (f32, bool) {
    let info = sh.cols.get(&col);
    (info.and_then(|i| i.size).unwrap_or(sh.default_col_width), info.is_some_and(|i| i.hidden))
}

/// (height, hidden, set by the user)
fn row_look(sh: &Sheet, row: u32) -> (f32, bool, bool) {
    let info = sh.rows.get(&row);
    (info.and_then(|i| i.size).unwrap_or(sh.default_row_height), info.is_some_and(|i| i.hidden), info.is_some_and(|i| i.size.is_some()))
}

fn cell_xml(wb: &Workbook, at: CellRef, cell: &Cell, span: Option<(u32, u32)>, styles: &mut StyleSet, out: &mut String) {
    let style: &Style = wb.styles.get(cell.style);
    let kind = kind_of(style.num_fmt.as_str());
    out.push_str("<table:table-cell");
    if let Some(name) = styles.cell(wb, cell.style) {
        let _ = write!(out, r#" table:style-name="{name}""#);
    }
    if let Some((w, h)) = span {
        let _ = write!(out, r#" table:number-columns-spanned="{w}" table:number-rows-spanned="{h}""#);
    }
    if let Some(f) = &cell.formula {
        let expr = f
            .expr
            .as_deref()
            .map(gridcraft_formula::print_odf)
            .unwrap_or_else(|| gridcraft_formula::parse(&f.text).ok().and_then(|e| gridcraft_formula::print_odf(&e)));
        if let Some(text) = expr {
            let _ = write!(out, r#" table:formula="of:={}""#, esc_attr(&text));
            if let Some(a) = f.array.filter(|a| a.start == at) {
                let _ = write!(
                    out,
                    r#" table:number-matrix-columns-spanned="{}" table:number-matrix-rows-spanned="{}""#,
                    a.end.col - a.start.col + 1,
                    a.end.row - a.start.row + 1
                );
            }
        }
    }
    let value = match &cell.value {
        Value::Array(a) => a.get(0, 0).cloned().unwrap_or(Value::Empty),
        v => v.clone(),
    };
    let shown: Option<String> = match &value {
        Value::Empty => None,
        Value::Number(n) => {
            let n = *n;
            let date = if kind == Kind::Date { datetime_from_serial(wb.date_system, n) } else { None };
            match kind {
                Kind::Date if let Some(d) = date => {
                    let iso = if n.fract() == 0.0 {
                        format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
                    } else {
                        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second)
                    };
                    let _ = write!(out, r#" office:value-type="date" office:date-value="{iso}" calcext:value-type="date""#);
                    Some(iso)
                }
                Kind::Time if n >= 0.0 && n.is_finite() => {
                    let secs = (n * 86_400.0).round() as u64;
                    let dur = format!("PT{:02}H{:02}M{:02}S", secs / 3600, (secs / 60) % 60, secs % 60);
                    let _ = write!(out, r#" office:value-type="time" office:time-value="{dur}" calcext:value-type="time""#);
                    Some(format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60))
                }
                Kind::Percent => {
                    let _ = write!(out, r#" office:value-type="percentage" office:value="{}" calcext:value-type="percentage""#, number_to_text(n));
                    Some(format!("{}%", number_to_text(n * 100.0)))
                }
                _ => {
                    let _ = write!(out, r#" office:value-type="float" office:value="{}" calcext:value-type="float""#, number_to_text(n));
                    Some(number_to_text(n))
                }
            }
        }
        Value::Bool(b) => {
            let _ = write!(out, r#" office:value-type="boolean" office:boolean-value="{b}" calcext:value-type="boolean""#);
            Some(if *b { "TRUE".into() } else { "FALSE".into() })
        }
        Value::Text(t) => {
            out.push_str(r#" office:value-type="string" calcext:value-type="string""#);
            Some(t.to_string())
        }
        Value::Error(e) => {
            out.push_str(r#" office:value-type="string" office:string-value="" calcext:value-type="error""#);
            Some(e.as_str().to_string())
        }
        Value::Array(_) => None,
    };
    match shown {
        None => out.push_str("/>"),
        Some(text) => {
            out.push('>');
            for line in text.split('\n') {
                out.push_str("<text:p>");
                out.push_str(&para_text(line));
                out.push_str("</text:p>");
            }
            out.push_str("</table:table-cell>");
        }
    }
}

/// Text for a `text:p`: runs of spaces after the first are `text:s`, tabs `text:tab` (ODF
/// collapses white space in element content).
fn para_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut spaces = 0usize;
    let flush = |out: &mut String, spaces: &mut usize, at_start: bool| {
        if *spaces == 0 {
            return;
        }
        let (plain, extra) = if at_start { (0, *spaces) } else { (1, *spaces - 1) };
        if plain == 1 {
            out.push(' ');
        }
        if extra == 1 {
            out.push_str("<text:s/>");
        } else if extra > 1 {
            let _ = write!(out, r#"<text:s text:c="{extra}"/>"#);
        }
        *spaces = 0;
    };
    let mut start = true;
    for c in s.chars() {
        match c {
            ' ' => spaces += 1,
            '\t' => {
                flush(&mut out, &mut spaces, start);
                out.push_str("<text:tab/>");
                start = false;
            }
            _ => {
                flush(&mut out, &mut spaces, start);
                start = false;
                let mut b = [0u8; 4];
                out.push_str(&esc(c.encode_utf8(&mut b)));
            }
        }
    }
    // Trailing spaces are kept as text:s too.
    if spaces > 0 {
        let _ = write!(out, r#"<text:s text:c="{spaces}"/>"#);
    }
    out
}

/// The date system's epoch, written so other readers count serials from the same day.
fn null_date(sys: DateSystem) -> &'static str {
    match sys {
        DateSystem::D1900 => "1899-12-30",
        DateSystem::D1904 => "1904-01-01",
    }
}
