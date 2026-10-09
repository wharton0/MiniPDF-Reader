//! PDF geometry, coordinate mappings, bookmarks, notes and rotation utilities.

use std::path::Path;
use eframe::egui;
use pdfium_render::prelude::*;
use crate::types::{CharInfo, NoteItem, OutlineItem};

/// Snap zoom to a fixed grid (avoids float drift and makes texture keys reusable).
pub const ZOOM_STEP: f32 = 0.05;
pub const RENDER_BASE_WIDTH: i32 = 1100;
pub const MAX_RENDER_WIDTH: i32 = 2600;

pub fn snap_zoom(z: f32) -> f32 {
    ((z / ZOOM_STEP).round() * ZOOM_STEP).clamp(0.2, 4.0)
}

pub fn render_width(zoom: f32) -> u32 {
    ((RENDER_BASE_WIDTH as f32 * zoom) as i32).clamp(1, MAX_RENDER_WIDTH) as u32
}

pub fn pdf_rect_to_screen_raw(
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    img: &egui::Rect,
    pw: f32,
    ph: f32,
) -> Option<egui::Rect> {
    if pw <= 0.0 || ph <= 0.0 {
        return None;
    }
    let sx = img.width() / pw;
    let sy = img.height() / ph;
    let x0 = img.min.x + left * sx;
    let x1 = img.min.x + right * sx;
    let y0 = img.max.y - top * sy;
    let y1 = img.max.y - bottom * sy;
    Some(egui::Rect::from_min_max(
        egui::pos2(x0.min(x1), y0.min(y1)),
        egui::pos2(x0.max(x1), y0.max(y1)),
    ))
}

pub fn pdf_rect_to_screen(c: &CharInfo, img: &egui::Rect, pw: f32, ph: f32) -> Option<egui::Rect> {
    pdf_rect_to_screen_raw(c.left, c.bottom, c.right, c.top, img, pw, ph)
}

pub fn rect_pt(l: f32, b: f32, r: f32, t: f32) -> PdfRect {
    PdfRect::new(
        PdfPoints::new(b),
        PdfPoints::new(l),
        PdfPoints::new(t),
        PdfPoints::new(r),
    )
}

pub fn merge_lines(rects: &[(f32, f32, f32, f32)]) -> Vec<(f32, f32, f32, f32)> {
    if rects.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<(f32, f32, f32, f32)> = rects.to_vec();
    sorted.sort_unstable_by(|a, b| {
        let ya = (a.1 + a.3) * 0.5;
        let yb = (b.1 + b.3) * 0.5;
        ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut lines: Vec<(f32, f32, f32, f32)> = Vec::new();
    for (l, b, r, t) in sorted {
        let yc = (b + t) * 0.5;
        let h = (t - b).max(0.5);
        if let Some(last) = lines.last_mut() {
            let lyc = (last.1 + last.3) * 0.5;
            let lh = (last.3 - last.1).max(0.5);
            if (yc - lyc).abs() < 0.4 * lh.max(h) {
                last.0 = last.0.min(l);
                last.1 = last.1.min(b);
                last.2 = last.2.max(r);
                last.3 = last.3.max(t);
                continue;
            }
        }
        lines.push((l, b, r, t));
    }
    lines
}

pub fn screen_to_pdf(pos: egui::Pos2, img: &egui::Rect, pw: f32, ph: f32) -> Option<(f32, f32)> {
    if pw <= 0.0 || ph <= 0.0 || img.width() <= 0.0 || img.height() <= 0.0 {
        return None;
    }
    let x = (pos.x - img.min.x) / img.width() * pw;
    let y = (img.max.y - pos.y) / img.height() * ph;
    Some((x, y))
}

pub fn pick_char(chars: &[CharInfo], x: f32, y: f32) -> Option<usize> {
    for (i, c) in chars.iter().enumerate() {
        if c.contains(x, y) {
            return Some(i);
        }
    }
    let mut best: Option<(usize, f32)> = None;
    for (i, c) in chars.iter().enumerate() {
        let (cx, cy) = c.center();
        let w = (c.right - c.left).max(1.0);
        let h = (c.top - c.bottom).max(1.0);
        let dx = (x - cx) / w;
        let dy = (y - cy) / h;
        let d = dx * dx + dy * dy;
        if d < 4.0 {
            match best {
                Some((_, bd)) if bd <= d => {}
                _ => best = Some((i, d)),
            }
        }
    }
    best.map(|(i, _)| i)
}

pub fn step_rotation(cur: PdfPageRenderRotation, left: bool) -> PdfPageRenderRotation {
    use PdfPageRenderRotation as R;
    match (cur, left) {
        (R::None, true) => R::Degrees270,
        (R::None, false) => R::Degrees90,
        (R::Degrees90, true) => R::None,
        (R::Degrees90, false) => R::Degrees180,
        (R::Degrees180, true) => R::Degrees90,
        (R::Degrees180, false) => R::Degrees270,
        (R::Degrees270, true) => R::Degrees180,
        (R::Degrees270, false) => R::None,
    }
}

pub fn note_snippet(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        s.push('…');
    }
    s
}

pub fn annotation_label(t: PdfPageAnnotationType) -> Option<&'static str> {
    Some(match t {
        PdfPageAnnotationType::Highlight => "Highlight",
        PdfPageAnnotationType::Underline => "Underline",
        PdfPageAnnotationType::Strikeout => "Strikethrough",
        PdfPageAnnotationType::Squiggly => "Squiggly",
        PdfPageAnnotationType::Square | PdfPageAnnotationType::Circle => "Markup",
        PdfPageAnnotationType::Ink => "Ink",
        PdfPageAnnotationType::FreeText => "Text box",
        PdfPageAnnotationType::Text => "Note",
        PdfPageAnnotationType::Stamp => "Stamp",
        PdfPageAnnotationType::Line
        | PdfPageAnnotationType::Polygon
        | PdfPageAnnotationType::Polyline => "Shape",
        PdfPageAnnotationType::Caret => "Caret",
        PdfPageAnnotationType::FileAttachment => "Attachment",
        PdfPageAnnotationType::Redacted => "Redaction",
        PdfPageAnnotationType::Link
        | PdfPageAnnotationType::Widget
        | PdfPageAnnotationType::XfaWidget
        | PdfPageAnnotationType::Popup
        | PdfPageAnnotationType::Unknown => return None,
        _ => "Annotation",
    })
}

pub fn fetch_notes(path: &Path) -> Result<Vec<NoteItem>, String> {
    let pdfium = minipdf::ensure_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| e.to_string())?;
    let pages = document.pages();
    let mut out = Vec::new();
    for page in 0..pages.len() {
        let pg = pages.get(page).map_err(|e| e.to_string())?;
        let annots = pg.annotations();
        if annots.is_empty() {
            continue;
        }
        for idx in annots.as_range() {
            if out.len() >= 2000 {
                return Ok(out);
            }
            let annot = annots.get(idx).map_err(|e| e.to_string())?;
            let annot_type = annot.annotation_type();
            let contents = annot.contents().unwrap_or_default();
            let label = if annot_type == PdfPageAnnotationType::Square && !contents.is_empty() {
                "Note"
            } else {
                match annotation_label(annot_type) {
                    Some(l) => l,
                    None => continue,
                }
            };
            out.push(NoteItem {
                page,
                index: idx,
                label,
                text: contents,
            });
        }
    }
    Ok(out)
}

pub fn bookmark_page(bm: &PdfBookmark<'_>) -> Option<i32> {
    if let Some(dest) = bm.destination() {
        if let Ok(p) = dest.page_index() {
            return Some(p as i32);
        }
    }
    if let Some(act) = bm.action() {
        if let Some(local) = act.as_local_destination_action() {
            if let Ok(dest) = local.destination() {
                if let Ok(p) = dest.page_index() {
                    return Some(p as i32);
                }
            }
        }
    }
    None
}

pub fn collect_outline(bm: &PdfBookmark<'_>, depth: usize, out: &mut Vec<OutlineItem>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    out.push(OutlineItem {
        title: bm.title().unwrap_or_default(),
        page: bookmark_page(bm),
        depth,
    });
    let mut child = bm.first_child();
    while let Some(c) = child {
        collect_outline(&c, depth + 1, out, cap);
        if out.len() >= cap {
            return;
        }
        child = c.next_sibling();
    }
}

pub fn fetch_outline(path: &Path) -> Result<Vec<OutlineItem>, String> {
    let pdfium = minipdf::ensure_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(root) = document.bookmarks().root() {
        let mut sib = root.first_child();
        while let Some(b) = sib {
            collect_outline(&b, 0, &mut out, 2000);
            sib = b.next_sibling();
        }
    }
    Ok(out)
}
