// Repro of the app's markup path on target/mark_test.pdf:
// highlight chars 0..11 ("Hello world"), underline + strikeout on line 2,
// save a copy, render page 0 to target/mark_out.png for visual inspection.
// Run: cargo run --example mark_test

use minipdf::ensure_pdfium;
use pdfium_render::prelude::*;

fn rect_pt(l: f32, b: f32, r: f32, t: f32) -> PdfRect {
    PdfRect::new(
        PdfPoints::new(b),
        PdfPoints::new(l),
        PdfPoints::new(t),
        PdfPoints::new(r),
    )
}

/// Merge char boxes into per-line boxes (left, bottom, right, top).
fn merge_lines(rects: &[(f32, f32, f32, f32)]) -> Vec<(f32, f32, f32, f32)> {
    let mut lines: Vec<(f32, f32, f32, f32)> = Vec::new();
    for &(l, b, r, t) in rects {
        let yc = (b + t) * 0.5;
        let h = (t - b).max(0.5);
        let mut placed = false;
        for line in lines.iter_mut() {
            let lyc = (line.1 + line.3) * 0.5;
            let lh = (line.3 - line.1).max(0.5);
            if (yc - lyc).abs() < 0.4 * lh.max(h) {
                line.0 = line.0.min(l);
                line.1 = line.1.min(b);
                line.2 = line.2.max(r);
                line.3 = line.3.max(t);
                placed = true;
                break;
            }
        }
        if !placed {
            lines.push((l, b, r, t));
        }
    }
    lines
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pdfium = ensure_pdfium().map_err(|e| format!("engine: {e}"))?;
    let src = "target/mark_test.pdf";
    let document = pdfium.load_pdf_from_file(src, None)?;
    let pages = document.pages();
    let pg = pages.get(0)?;
    // owned char bounds first (drop all page borrows before mutating)
    let bounds: Vec<(f32, f32, f32, f32)> = {
        let text = pg.text()?;
        let mut v = Vec::new();
        for ch in text.chars().iter() {
            if ch.unicode_char().is_none() {
                continue;
            }
            let b = ch.loose_bounds()?;
            v.push((b.left().value, b.bottom().value, b.right().value, b.top().value));
        }
        v
    };
    let mut pg = pages.get(0)?;

    // Markup as line-merged translucent squares (works around broken
    // auto-generated markup AP in this pdfium build): highlight the first
    // line, underline + strikeout the second line.
    {
        let annots = pg.annotations_mut();
        let lines = merge_lines(&bounds[0..11]);
        for (l, b, r, t) in &lines {
            let mut a = annots.create_square_annotation()?;
            a.set_bounds(rect_pt(*l, *b, *r, *t))?;
            let fill = PdfColor::new(255, 255, 0, 128);
            a.set_fill_color(fill)?;
            a.set_stroke_color(fill)?;
        }
        let lines2 = merge_lines(&bounds[36..70]);
        for (l, b, r, t) in &lines2 {
            let h = (t - b).max(1.0);
            let mut u = annots.create_square_annotation()?;
            u.set_bounds(rect_pt(*l, *b + h * 0.08, *r, *b + h * 0.08 + 1.6))?;
            let red = PdfColor::new(220, 40, 40, 255);
            u.set_fill_color(red)?;
            u.set_stroke_color(red)?;
            let mid = *b + h * 0.38;
            let mut s = annots.create_square_annotation()?;
            s.set_bounds(rect_pt(*l, mid - 0.8, *r, mid + 0.8))?;
            s.set_fill_color(red)?;
            s.set_stroke_color(red)?;
        }
    }
    document.save_to_file("target/mark_test_out.pdf")?;
    drop(document);

    // render the saved file (fresh load, like the app does)
    let document = pdfium.load_pdf_from_file("target/mark_test_out.pdf", None)?;
    let pg = document.pages().get(0)?;
    let bmp = pg.render_with_config(
        &PdfRenderConfig::new()
            .set_target_width(900)
            .set_maximum_height(2700),
    )?;
    bmp.as_image()?.save("target/mark_out.png")?;
    println!("wrote target/mark_test_out.pdf + target/mark_out.png");
    Ok(())
}
