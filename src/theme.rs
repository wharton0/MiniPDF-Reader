//! UI colors, palettes, icon rendering, and styling helpers.

use eframe::egui;

/// Chrome surfaces (toolbar / sidebar / statusbar).
/// Light: Catalina EB/F0/EB. Dark: macOS ~#2B2B2B / #252525 / #2B2B2B.
pub fn preview_chrome(dark: bool) -> (egui::Color32, egui::Color32, egui::Color32) {
    if dark {
        (
            egui::Color32::from_gray(43), // toolbar  #2B2B2B
            egui::Color32::from_gray(37), // sidebar  #252525
            egui::Color32::from_gray(43), // statusbar
        )
    } else {
        (
            egui::Color32::from_gray(235), // toolbar  #EBEBEB
            egui::Color32::from_gray(240), // sidebar  #F0F0F0
            egui::Color32::from_gray(235), // statusbar
        )
    }
}

/// Page canvas background. Light: #F5F5F5. Dark: #1A1A1A.
pub fn preview_canvas(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_gray(26) // #1A1A1A — darker than chrome so pages pop
    } else {
        egui::Color32::from_gray(245)
    }
}

/// Text selection tint. Light: iOS blue 50%. Dark: iOS blue 55%.
pub fn preview_selection(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgba_unmultiplied(10, 132, 255, 140)
    } else {
        egui::Color32::from_rgba_unmultiplied(0, 122, 255, 128)
    }
}

/// Apple link blue (NSColor.linkColor): light #007AFF / dark #0A84FF.
pub fn preview_link(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgb(10, 132, 255)
    } else {
        egui::Color32::from_rgb(0, 122, 255)
    }
}

/// 1px separator groove at a panel edge: rgba(0,0,0,0.10) single line.
/// Painted foreground right after layout; edge: 0 = bottom, 1 = top, 2 = right.
pub fn paint_groove(ui: &egui::Ui, rect: egui::Rect, edge: u8) {
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let p = ui.painter();
    let dark = ui.visuals().dark_mode;
    let line = if dark {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18)
    } else {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 26)
    };
    match edge {
        0 => {
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(rect.min.x, rect.max.y - 1.0),
                    egui::pos2(rect.max.x, rect.max.y),
                ),
                0.0,
                line,
            );
        }
        1 => {
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(rect.min.x, rect.min.y),
                    egui::pos2(rect.max.x, rect.min.y + 1.0),
                ),
                0.0,
                line,
            );
        }
        _ => {
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(rect.max.x - 1.0, rect.min.y),
                    egui::pos2(rect.max.x, rect.max.y),
                ),
                0.0,
                line,
            );
        }
    }
}

/// Preview-style toolbar group separator: airy whitespace with a hairline.
pub fn toolbar_sep(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    let h = ui.spacing().interact_size.y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, h), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let col = if ui.visuals().dark_mode {
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
        } else {
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 20)
        };
        ui.painter().rect_filled(rect, 0.0, col);
    }
    ui.add_space(6.0);
}

/// Paint a toolbar icon: Lucide texture when loaded (white glyph, theme-tinted),
/// otherwise the optional hand-drawn vector fallback. Square-fit so wide buttons
/// don't stretch it.
pub fn icon_paint(
    tex: Option<egui::TextureId>,
    fallback: Option<fn(&egui::Painter, egui::Rect, egui::Color32)>,
) -> impl FnOnce(&egui::Painter, egui::Rect, egui::Color32) {
    move |painter, rect, color| match tex {
        Some(id) => {
            let side = rect.width().min(rect.height());
            let sq = egui::Rect::from_center_size(rect.center(), egui::vec2(side, side));
            painter.image(
                id,
                sq,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                color,
            );
        }
        None => {
            if let Some(f) = fallback {
                f(painter, rect, color);
            }
        }
    }
}

pub fn paint_chevron_left(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let h = rect.height() * 0.32;
    let w = h * 0.6;
    let stroke = egui::Stroke::new(1.8, color);
    painter.line_segment(
        [egui::pos2(cx + w, cy - h), egui::pos2(cx - w * 0.2, cy)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(cx - w * 0.2, cy), egui::pos2(cx + w, cy + h)],
        stroke,
    );
}

pub fn paint_chevron_right(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let h = rect.height() * 0.32;
    let w = h * 0.6;
    let stroke = egui::Stroke::new(1.8, color);
    painter.line_segment(
        [egui::pos2(cx - w, cy - h), egui::pos2(cx + w * 0.2, cy)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(cx + w * 0.2, cy), egui::pos2(cx - w, cy + h)],
        stroke,
    );
}

pub fn paint_minus(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let w = rect.width() * 0.28;
    painter.line_segment(
        [egui::pos2(cx - w, cy), egui::pos2(cx + w, cy)],
        egui::Stroke::new(1.8, color),
    );
}

pub fn paint_plus(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let w = rect.width() * 0.28;
    painter.line_segment(
        [egui::pos2(cx - w, cy), egui::pos2(cx + w, cy)],
        egui::Stroke::new(1.8, color),
    );
    painter.line_segment(
        [egui::pos2(cx, cy - w), egui::pos2(cx, cy + w)],
        egui::Stroke::new(1.8, color),
    );
}

pub fn paint_sidebar_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let w = rect.width() * 0.30;
    let dh = rect.height() * 0.18;
    let stroke = egui::Stroke::new(1.5, color);
    for dy in [-dh, 0.0_f32, dh] {
        painter.line_segment(
            [egui::pos2(cx - w, cy + dy), egui::pos2(cx + w, cy + dy)],
            stroke,
        );
    }
    painter.line_segment(
        [
            egui::pos2(cx - w * 0.3, cy - dh * 1.6),
            egui::pos2(cx - w * 0.3, cy + dh * 1.6),
        ],
        stroke,
    );
}

pub fn paint_sun(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let r = rect.height() * 0.14;
    let ray = rect.height() * 0.25;
    let stroke = egui::Stroke::new(1.5, color);
    painter.circle_stroke(egui::pos2(cx, cy), r, stroke);
    for i in 0..8_i32 {
        let angle = i as f32 * std::f32::consts::TAU / 8.0;
        let (s, c2) = angle.sin_cos();
        let inner = r + 2.0;
        painter.line_segment(
            [
                egui::pos2(cx + c2 * inner, cy + s * inner),
                egui::pos2(cx + c2 * ray, cy + s * ray),
            ],
            stroke,
        );
    }
}

pub fn paint_moon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let r = rect.height() * 0.22;
    let stroke = egui::Stroke::new(1.5, color);
    let outer: Vec<egui::Pos2> = (0..=20)
        .map(|i| {
            let a = std::f32::consts::PI * 0.6 + i as f32 * std::f32::consts::PI / 20.0;
            egui::pos2(cx + a.cos() * r, cy + a.sin() * r)
        })
        .collect();
    for w in outer.windows(2) {
        painter.line_segment([w[0], w[1]], stroke);
    }
    let offset_x = r * 0.35;
    let inner_r = r * 0.82;
    let inner: Vec<egui::Pos2> = (0..=18)
        .map(|i| {
            let a = std::f32::consts::PI * 0.62 + i as f32 * std::f32::consts::PI * 0.96 / 18.0;
            egui::pos2(cx + offset_x + a.cos() * inner_r, cy + a.sin() * inner_r)
        })
        .collect();
    for w in inner.windows(2) {
        painter.line_segment([w[0], w[1]], stroke);
    }
    if let (Some(&p1), Some(&p2)) = (outer.first(), inner.first()) {
        painter.line_segment([p1, p2], stroke);
    }
    if let (Some(&p1), Some(&p2)) = (outer.last(), inner.last()) {
        painter.line_segment([p1, p2], stroke);
    }
}
