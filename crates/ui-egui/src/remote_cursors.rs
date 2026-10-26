//! The other people's selections in a live workbook (OneOS): a coloured outline around the cells each has selected, with their name on a
//! small tab above the outline's corner. Where they are is the host's awareness, resolved to this window's cells by the shim.

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, StrokeKind, pos2, vec2};
use gridcraft_engine::core::{CellRef, RangeRef};
use gridcraft_engine::model::Sheet;

use crate::grid::Geo;

/// One other person's selection.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteCursor {
    pub name: String,
    pub color: [u8; 3],
    /// The sheet's index in this window's workbook.
    pub sheet: usize,
    pub anchor: CellRef,
    pub focus: CellRef,
}

/// Paint the cursors that are on sheet `si`.
pub fn paint(cursors: &[RemoteCursor], p: &Painter, geo: &Geo, sh: &Sheet, si: usize) {
    for c in cursors.iter().filter(|c| c.sheet == si) {
        let rect = geo.range_rect(sh, RangeRef::new(c.anchor, c.focus));
        let [r, g, b] = c.color;
        let color = Color32::from_rgb(r, g, b);
        p.rect_stroke(rect, 0.0, Stroke::new(2.0, color), StrokeKind::Inside);
        if c.anchor != c.focus {
            p.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(r, g, b, 22));
        }
        // The name, on a tab above the top-left corner (inside the outline when there is no room above).
        let font = FontId::proportional(11.0);
        let galley = p.layout_no_wrap(c.name.clone(), font.clone(), Color32::WHITE);
        let tab = vec2(galley.size().x + 8.0, galley.size().y + 2.0);
        let top = if rect.top() - tab.y >= p.clip_rect().top() { rect.top() - tab.y } else { rect.top() };
        let tab_rect = Rect::from_min_size(pos2(rect.left(), top), tab);
        p.rect_filled(tab_rect, 2.0, color);
        p.text(tab_rect.center(), Align2::CENTER_CENTER, &c.name, font, Color32::WHITE);
    }
}
