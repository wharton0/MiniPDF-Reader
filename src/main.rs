#![windows_subsystem = "windows"]
// MiniPDF - minimal local PDF reader (Rust + Pdfium + egui)
// No background services, no network, no registry writes.
// Features: tabs / open / pages / zoom / thumbnails / search(highlight+jump) /
// text select+copy / print(whole doc + current page) / wheel paging.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui;
use minipdf::{ensure_pdfium, find_cjk_font_path};
use pdfium_render::prelude::*;

const THUMB_WIDTH: i32 = 132;
const RENDER_BASE_WIDTH: i32 = 1100;

/// Catalina v2 ladder (restrained grays, forced light theme):
/// toolbar EB / sidebar F0 / status EB.
fn preview_chrome(dark: bool) -> (egui::Color32, egui::Color32, egui::Color32) {
    if dark {
        (
            egui::Color32::from_gray(50),
            egui::Color32::from_gray(50),
            egui::Color32::from_gray(50),
        )
    } else {
        (
            egui::Color32::from_gray(235),
            egui::Color32::from_gray(240),
            egui::Color32::from_gray(235),
        )
    }
}

/// Window/canvas behind pages: F5F5F5 (document itself stays white).
fn preview_canvas(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_gray(40)
    } else {
        egui::Color32::from_gray(245)
    }
}

/// Apple text-selection tint (NSColor.selectedTextBackgroundColor),
/// converted to an overlay alpha over the white page:
/// light #B3D7FF / dark #3F638B.
fn preview_selection(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgba_unmultiplied(30, 70, 110, 220)
    } else {
        egui::Color32::from_rgba_unmultiplied(0, 122, 255, 128)
    }
}

/// Apple link blue (NSColor.linkColor): light #007AFF / dark #0A84FF.
fn preview_link(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgb(10, 132, 255)
    } else {
        egui::Color32::from_rgb(0, 122, 255)
    }
}

/// 1px separator groove at a panel edge: rgba(0,0,0,0.10) single line.
/// Painted foreground right after layout; edge: 0 = bottom, 1 = top, 2 = right.
fn paint_groove(ui: &egui::Ui, rect: egui::Rect, edge: u8) {
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let p = ui.painter();
    let line = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 26);
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

#[cfg(windows)]
#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> isize;
}

/// Run a shell verb ("print" / "open") on a file via ShellExecuteW.
/// Returns Ok on success (return value > 32), Err with a readable reason otherwise.
#[cfg(windows)]
fn shell_verb(path: &Path, verb: &str) -> Result<(), String> {
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let op = wide(verb);
    let file = wide(&path.to_string_lossy());
    // SAFETY: ShellExecuteW only reads the given strings during the call.
    let ret = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if ret > 32 {
        Ok(())
    } else {
        Err(match ret as i32 {
            0 | 8 => "Out of memory".to_owned(),
            2 => "File not found".to_owned(),
            3 => "Path not found".to_owned(),
            5 => "Access denied".to_owned(),
            11 => "Bad file format".to_owned(),
            27 => "File association is incomplete".to_owned(),
            29 => "Print handler failed".to_owned(),
            30 => "Print handler is busy".to_owned(),
            31 => "No app handles this file (set a default PDF reader)".to_owned(),
            32 => "Required DLL not found".to_owned(),
            c => format!("System error {c}"),
        })
    }
}

#[cfg(not(windows))]
fn shell_verb(_path: &Path, _verb: &str) -> Result<(), String> {
    Err("One-click print is not supported on this platform".to_owned())
}

// ---------- text selection ----------

#[derive(Clone, Copy)]
struct CharInfo {
    ch: char,
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl CharInfo {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
    fn center(&self) -> (f32, f32) {
        ((self.left + self.right) * 0.5, (self.bottom + self.top) * 0.5)
    }
}

#[derive(Clone, Copy)]
struct TextSelection {
    page: i32,
    start: usize,
    end: usize, // inclusive, normalized start <= end
}

impl TextSelection {
    fn new(page: i32, a: usize, b: usize) -> Self {
        Self {
            page,
            start: a.min(b),
            end: a.max(b),
        }
    }
    fn len(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }
}

// pdf point -> screen rect inside an image rect
fn pdf_rect_to_screen_raw(
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
    // pdf y is bottom-up, screen y is top-down
    let y0 = img.max.y - top * sy;
    let y1 = img.max.y - bottom * sy;
    Some(egui::Rect::from_min_max(
        egui::pos2(x0.min(x1), y0.min(y1)),
        egui::pos2(x0.max(x1), y0.max(y1)),
    ))
}

fn pdf_rect_to_screen(
    c: &CharInfo,
    img: &egui::Rect,
    pw: f32,
    ph: f32,
) -> Option<egui::Rect> {
    pdf_rect_to_screen_raw(c.left, c.bottom, c.right, c.top, img, pw, ph)
}

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

fn screen_to_pdf(pos: egui::Pos2, img: &egui::Rect, pw: f32, ph: f32) -> Option<(f32, f32)> {
    if pw <= 0.0 || ph <= 0.0 || img.width() <= 0.0 || img.height() <= 0.0 {
        return None;
    }
    let x = (pos.x - img.min.x) / img.width() * pw;
    let y = (img.max.y - pos.y) / img.height() * ph;
    Some((x, y))
}

/// 1:1 lowercase mapping so search indices stay aligned with char indices.
fn lower1(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn pick_char(chars: &[CharInfo], x: f32, y: f32) -> Option<usize> {    // 1. direct hit
    for (i, c) in chars.iter().enumerate() {
        if c.contains(x, y) {
            return Some(i);
        }
    }
    // 2. nearest center within a tolerance (half of char size + slack)
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

/// Text markup kind (mirrors Preview: highlight / underline / strike out).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkupKind {
    Highlight,
    Underline,
    Strikeout,
}

impl MarkupKind {
    fn name(self) -> &'static str {
        match self {
            MarkupKind::Highlight => "Highlight",
            MarkupKind::Underline => "Underline",
            MarkupKind::Strikeout => "Strikethrough",
        }
    }
}

/// Highlight color choices (Preview offers yellow/green/blue/pink).
const HIGHLIGHT_COLORS: &[(&str, (u8, u8, u8))] = &[
    ("Yellow", (255, 255, 0)),
    ("Green", (146, 208, 80)),
    ("Blue", (155, 194, 230)),
    ("Pink", (255, 153, 204)),
];

// ---------- tabs ----------

/// Clickable link on a page: bounds in PDF points + target.
#[derive(Clone)]
enum LinkTarget {
    Url(String),
    Page(i32),
}

#[derive(Clone)]
struct PageLink {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    target: LinkTarget,
}

impl PageLink {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
    fn label(&self) -> String {
        match &self.target {
            LinkTarget::Url(u) => u.clone(),
            LinkTarget::Page(p) => format!("Go to page {}", p + 1),
        }
    }
}

/// One occurrence of the search query: char range [start, end] on a page.
#[derive(Clone, Copy)]
struct SearchMatch {
    page: i32,
    start: usize,
    end: usize, // inclusive
}

struct DocTab {
    path: PathBuf,
    pages: i32,
    cur: i32,
    zoom: f32,
    aspects: HashMap<i32, (f32, f32)>,
    scroll_target: Option<i32>,
    scroll_to_match: bool, // jump was triggered by search: center the match, not the page
    page_tex: HashMap<(i32, u32), egui::TextureHandle>,
    thumb_tex: HashMap<i32, egui::TextureHandle>,
    search_text: String,
    search_query: String, // last executed query (1:1-lowercased)
    search_matches: Vec<SearchMatch>, // every occurrence, ordered by page
    search_cursor: usize, // index into search_matches
    search_hits: Vec<i32>, // pages containing matches (thumbnail badges)
    text_cache: HashMap<i32, Vec<CharInfo>>,
    link_cache: HashMap<i32, Vec<PageLink>>,
    selection: Option<TextSelection>,
    highlight_rgb: (u8, u8, u8),
    markup_stack: Vec<i32>, // pages of markups created this session (for undo)
    drag_anchor: Option<(i32, usize)>,
}

impl DocTab {
    fn new(path: PathBuf, pages: i32, aspects: HashMap<i32, (f32, f32)>) -> Self {
        Self {
            path,
            pages,
            cur: 0,
            zoom: 1.0,
            aspects,
            scroll_target: None,
            scroll_to_match: false,
            page_tex: HashMap::new(),
            thumb_tex: HashMap::new(),
            search_text: String::new(),
            search_query: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            search_hits: Vec::new(),
            text_cache: HashMap::new(),
            link_cache: HashMap::new(),
            highlight_rgb: (255, 255, 0),
            markup_stack: Vec::new(),
            selection: None,
            drag_anchor: None,
        }
    }

    fn title(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }

    fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let chars = self.text_cache.get(&sel.page)?;
        if chars.is_empty() || sel.end >= chars.len() {
            return None;
        }
        Some(chars[sel.start..=sel.end].iter().map(|c| c.ch).collect())
    }
}

// ---------- app ----------

/// Sidebar content: page thumbnails or search-result list (Preview style).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Thumbs,
    Results,
}

struct MiniPdf {
    pdfium: Option<Pdfium>,
    tabs: Vec<DocTab>,
    active: usize,
    show_sidebar: bool,
    sidebar_mode: SidebarMode,
    status: String,
    focus_search: bool,
    fullscreen: bool,
    last_title: String,
    // UI icons decoded synchronously at startup (no async bytes-loader involved).
    tex_highlight: Option<egui::TextureHandle>,
    tex_highlight_disabled: Option<egui::TextureHandle>,
    tex_logo: Option<egui::TextureHandle>,
}

impl Default for MiniPdf {
    fn default() -> Self {
        Self {
            pdfium: None,
            tabs: Vec::new(),
            active: 0,
            show_sidebar: false,
            sidebar_mode: SidebarMode::Thumbs,
            status: "Drag a PDF here, or click Open".to_owned(),
            focus_search: false,
            fullscreen: false,
            last_title: String::new(),
            tex_highlight: None,
            tex_highlight_disabled: None,
            tex_logo: None,
        }
    }
}

impl MiniPdf {
    /// Single shared Pdfium instance: bind once, reuse for all documents.
    fn ensure_engine(&mut self) -> Result<(), String> {
        if self.pdfium.is_none() {
            self.pdfium = Some(ensure_pdfium()?);
        }
        Ok(())
    }

    fn active_tab(&self) -> Option<&DocTab> {
        self.tabs.get(self.active)
    }

    fn open_file(&mut self, path: PathBuf) {
        if let Err(e) = self.ensure_engine() {
            self.status = e;
            return;
        }
        // already open? just activate it
        if let Some(idx) = self.tabs.iter().position(|t| t.path == path) {
            self.active = idx;
            self.tabs[idx].scroll_target = Some(self.tabs[idx].cur);
            self.status = format!("Switched to {}", self.tabs[idx].title());
            return;
        }
        let pdfium = self.pdfium.as_ref().unwrap();
        match pdfium.load_pdf_from_file(&path, None) {
            Ok(document) => {
                let pages = document.pages();
                let n = pages.len();
                let mut aspects = HashMap::new();
                for k in 0..n {
                    if let Ok(pg) = pages.get(k) {
                        aspects.insert(k, (pg.width().value, pg.height().value));
                    }
                }
                drop(document);
                let title = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.tabs.push(DocTab::new(path, n, aspects));
                self.active = self.tabs.len() - 1;
                self.status = format!("Opened {title}, {n} pages");
            }
            Err(e) => self.status = format!("Open failed: {e}"),
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let title = self.tabs[idx].title();
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active = 0;
            self.status = "Drag a PDF here, or click Open".to_owned();
        } else {
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if idx < self.active {
                self.active -= 1;
            } else if idx == self.active && self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
            self.status = format!("Closed {title}");
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .pick_files()
        {
            for p in paths {
                self.open_file(p);
            }
        }
    }

    fn with_doc<R>(
        &self,
        tab_idx: usize,
        f: impl FnOnce(&Pdfium, &Path) -> Result<R, PdfiumError>,
    ) -> Result<R, String> {
        let e = self.pdfium.as_ref().ok_or_else(|| "Engine not ready".to_owned())?;
        let t = self.tabs.get(tab_idx).ok_or_else(|| "No document open".to_owned())?;
        f(e, &t.path).map_err(|e| format!("{e}"))
    }

    /// Render one page to a texture. Callers gate on visibility to avoid UI stalls.
    fn render_tex(
        &mut self,
        tab_idx: usize,
        ctx: &egui::Context,
        page: i32,
        width_px: i32,
        cache: bool,
    ) -> Option<egui::TextureHandle> {
        if cache {
            if let Some(t) = self.tabs.get(tab_idx)?.page_tex.get(&(page, width_px as u32)) {
                return Some(t.clone());
            }
        } else if let Some(t) = self.tabs.get(tab_idx)?.thumb_tex.get(&page) {
            return Some(t.clone());
        }
        let img = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pg = document.pages().get(page)?;
            let bmp = pg.render_with_config(
                &PdfRenderConfig::new()
                    .set_target_width(width_px)
                    .set_maximum_height(width_px * 3),
            )?;
            bmp.as_image()
        });
        match img {
            Ok(dynamic) => {
                let rgba = dynamic.to_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                let cimg = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                let tex = ctx.load_texture(
                    format!("t{tab_idx}-p{page}-{width_px}"),
                    cimg,
                    egui::TextureOptions::LINEAR,
                );
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    if cache {
                        if tab.page_tex.len() > 20 {
                            tab.page_tex.clear();
                        }
                        tab.page_tex.insert((page, width_px as u32), tex.clone());
                    } else {
                        tab.thumb_tex.insert(page, tex.clone());
                    }
                }
                Some(tex)
            }
            Err(e) => {
                self.status = format!("Render page {} failed: {e}", page + 1);
                None
            }
        }
    }

    /// Load per-char text + bounds for a page (lazy, cached per tab).
    fn ensure_text(&mut self, tab_idx: usize, page: i32) {
        if self.tabs.get(tab_idx).map(|t| t.text_cache.contains_key(&page)).unwrap_or(true) {
            return;
        }
        let r = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pg = document.pages().get(page)?;
            let text = pg.text()?;
            let mut out = Vec::new();
            for ch in text.chars().iter() {
                let c = match ch.unicode_char() {
                    Some(c) => c,
                    None => continue,
                };
                // loose bounds cover the whole glyph; fall back to tight when needed
                let b = ch.loose_bounds().or_else(|_| ch.tight_bounds());
                if let Ok(r) = b {
                    out.push(CharInfo {
                        ch: c,
                        left: r.left().value,
                        bottom: r.bottom().value,
                        right: r.right().value,
                        top: r.top().value,
                    });
                }
            }
            Ok(out)
        });
        match r {
            Ok(v) => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.text_cache.insert(page, v);
                }
            }
            Err(e) => {
                self.status = format!("Text layer page {} failed: {e}", page + 1);
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.text_cache.insert(page, Vec::new());
                }
            }
        }
    }

    /// Load clickable links for a page (lazy, cached per tab).
    fn ensure_links(&mut self, tab_idx: usize, page: i32) {
        if self.tabs.get(tab_idx).map(|t| t.link_cache.contains_key(&page)).unwrap_or(true) {
            return;
        }
        let r = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pg = document.pages().get(page)?;
            let links = pg.links();
            let mut out = Vec::new();
            for idx in 0..links.len() {
                let link = match links.get(idx) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                let rect = match link.rect() {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                // external URL?
                let mut target: Option<LinkTarget> = link
                    .action()
                    .and_then(|a| a.as_uri_action().map(|u| u.uri()))
                    .and_then(|u| u.ok())
                    .map(|u| {
                        let l = u.to_lowercase();
                        if l.starts_with("http://")
                            || l.starts_with("https://")
                            || l.starts_with("mailto:")
                        {
                            LinkTarget::Url(u)
                        } else if !u.contains("://") {
                            // bare domain like "example.com" — treat as https
                            LinkTarget::Url(format!("https://{u}"))
                        } else {
                            LinkTarget::Url(u)
                        }
                    });
                // internal destination? (bind temporaries; combinators would
                // return references to dropped values)
                if target.is_none() {
                    let mut page_idx: Option<i32> = None;
                    if let Some(act) = link.action() {
                        if let Some(local) = act.as_local_destination_action() {
                            if let Ok(dest) = local.destination() {
                                if let Ok(p) = dest.page_index() {
                                    page_idx = Some(p as i32);
                                }
                            }
                        }
                    }
                    if page_idx.is_none() {
                        if let Some(dest) = link.destination() {
                            if let Ok(p) = dest.page_index() {
                                page_idx = Some(p as i32);
                            }
                        }
                    }
                    if let Some(p) = page_idx {
                        target = Some(LinkTarget::Page(p));
                    }
                }
                if let Some(target) = target {
                    out.push(PageLink {
                        left: rect.left().value,
                        bottom: rect.bottom().value,
                        right: rect.right().value,
                        top: rect.top().value,
                        target,
                    });
                }
            }
            Ok(out)
        });
        match r {
            Ok(v) => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.link_cache.insert(page, v);
                }
            }
            Err(_) => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.link_cache.insert(page, Vec::new());
                }
            }
        }
    }

    /// Activate a link: URLs open in the browser, page links jump.
    fn open_link(&mut self, tab_idx: usize, link: &PageLink) {
        match &link.target {
            LinkTarget::Url(u) => match Self::os_open(std::path::Path::new(u)) {
                Ok(()) => self.status = format!("Opened link: {u}"),
                Err(e) => self.status = format!("Open link failed: {e}"),
            },
            LinkTarget::Page(p) => {
                self.goto(tab_idx, *p);
            }
        }
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        let (text, n) = match self.active_tab() {
            Some(t) => match t.selection_text() {
                Some(s) if !s.trim().is_empty() => {
                    let n = t.selection.map(|x| x.len()).unwrap_or(0);
                    (Some(s), n)
                }
                _ => (None, 0),
            },
            None => (None, 0),
        };
        match text {
            Some(s) => {
                ctx.copy_text(s);
                self.status = format!("Copied {n} chars (Ctrl+C)");
            }
            None => {
                self.status = "No text selected (scanned page?)".to_owned();
            }
        }
    }

    /// Explicit save (re-writes the file via temp + rename).
    /// Markup already autosaves, so this is mostly a confidence action.
    fn save_now(&mut self, tab_idx: usize) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let tmp = path.with_extension("pdf.minipdf-tmp");
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match std::fs::rename(&tmp, &path) {
                Ok(()) => self.status = format!("Saved {name}"),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Save failed: {e}");
            }
        }
    }

    /// Save a copy to a chosen path and switch the tab to it.
    fn save_as(&mut self, tab_idx: usize) {
        let cur = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let stem = cur
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "document".to_owned());
        let dest = match rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .set_file_name(format!("{stem}.pdf"))
            .save_file()
        {
            Some(p) => p,
            None => return,
        };
        if dest == cur {
            self.save_now(tab_idx);
            return;
        }
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            document.save_to_file(&dest)?;
            Ok(())
        });
        match r {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.path = dest.clone();
                }
                self.status = format!(
                    "Saved as {}",
                    dest.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    /// Add a highlight / underline / strikethrough over the current selection,
    /// then save the file (Preview-style instant persistence).
    fn add_markup(&mut self, tab_idx: usize, kind: MarkupKind) {
        const MAX_MARK_CHARS: usize = 3000;
        let (page, rects): (i32, Vec<(f32, f32, f32, f32)>) = match self.tabs.get(tab_idx) {
            Some(t) => match t.selection {
                Some(sel) => match t.text_cache.get(&sel.page) {
                    Some(cs) if !cs.is_empty() && sel.end < cs.len() => (
                        sel.page,
                        cs[sel.start..=sel.end]
                            .iter()
                            .map(|c| (c.left, c.bottom, c.right, c.top))
                            .collect(),
                    ),
                    _ => {
                        self.status = "No text selected".to_owned();
                        return;
                    }
                },
                None => {
                    self.status = "Select text first, then mark it up".to_owned();
                    return;
                }
            },
            None => return,
        };
        if rects.len() > MAX_MARK_CHARS {
            self.status = "Selection too large to mark up".to_owned();
            return;
        }
        let rgb = match kind {
            MarkupKind::Highlight => self
                .tabs
                .get(tab_idx)
                .map(|t| t.highlight_rgb)
                .unwrap_or((255, 255, 0)),
            MarkupKind::Underline | MarkupKind::Strikeout => (220, 40, 40),
        };
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let tmp = path.with_extension("pdf.minipdf-tmp");
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            {
                // NOTE: text-markup annotations (Highlight/Underline/StrikeOut)
                // are implemented as line-merged translucent/opaque squares:
                // this pdfium build generates broken appearances for
                // API-created markup quads (only the quad's right edge draws).
                // Squares render correctly everywhere and look the same.
                let annots = pg.annotations_mut();
                let lines = merge_lines(&rects);
                match kind {
                    MarkupKind::Highlight => {
                        let fill = PdfColor::new(rgb.0, rgb.1, rgb.2, 128);
                        for (l, b, r, t) in &lines {
                            let mut a = annots.create_square_annotation()?;
                            a.set_bounds(rect_pt(*l, *b, *r, *t))?;
                            a.set_fill_color(fill)?;
                            a.set_stroke_color(fill)?;
                        }
                    }
                    MarkupKind::Underline => {
                        let red = PdfColor::new(rgb.0, rgb.1, rgb.2, 255);
                        for (l, b, r, t) in &lines {
                            let h = (t - b).max(1.0);
                            let mut a = annots.create_square_annotation()?;
                            a.set_bounds(rect_pt(*l, *b + h * 0.08, *r, *b + h * 0.08 + 1.6))?;
                            a.set_fill_color(red)?;
                            a.set_stroke_color(red)?;
                        }
                    }
                    MarkupKind::Strikeout => {
                        let red = PdfColor::new(rgb.0, rgb.1, rgb.2, 255);
                        for (l, b, r, t) in &lines {
                            let h = (t - b).max(1.0);
                            let mid = *b + h * 0.38;
                            let mut a = annots.create_square_annotation()?;
                            a.set_bounds(rect_pt(*l, mid - 0.8, *r, mid + 0.8))?;
                            a.set_fill_color(red)?;
                            a.set_stroke_color(red)?;
                        }
                    }
                }
            }
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match std::fs::rename(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                        tab.markup_stack.push(page);
                    }
                    self.status =
                        format!("{} added on page {} (saved)", kind.name(), page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Markup failed: {e}");
            }
        }
    }

    /// Remove the most recently created markup (this session) and re-save.
    fn undo_markup(&mut self, tab_idx: usize) {
        let page = match self.tabs.get_mut(tab_idx).and_then(|t| t.markup_stack.pop()) {
            Some(p) => p,
            None => {
                self.status = "Nothing to undo".to_owned();
                return;
            }
        };
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let tmp = path.with_extension("pdf.minipdf-tmp");
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            {
                let annots = pg.annotations_mut();
                if let Ok(last) = annots.last() {
                    annots.delete_annotation(last)?;
                }
            }
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match std::fs::rename(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                    }
                    self.status = format!("Undid markup on page {} (saved)", page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Undo failed: {e}");
            }
        }
    }

    fn run_search(&mut self, tab_idx: usize) {
        let q_raw = match self.tabs.get(tab_idx) {
            Some(t) => t.search_text.trim().to_owned(),
            None => return,
        };
        // 1:1 lowercase mapping (first char only) so indices stay aligned
        let q: Vec<char> = q_raw.chars().map(|c| lower1(c)).collect();
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.search_matches.clear();
            tab.search_hits.clear();
            tab.search_cursor = 0;
            tab.search_query = q_raw.to_lowercase();
        }
        if q.is_empty() {
            return;
        }
        // single document load: warm the text cache and collect occurrences
        const MAX_PER_PAGE: usize = 500;
        const MAX_TOTAL: usize = 5000;
        let r = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pages = document.pages();
            let mut all_chars: Vec<(i32, Vec<CharInfo>)> = Vec::new();
            for i in 0..pages.len() {
                if let Ok(pg) = pages.get(i) {
                    if let Ok(text) = pg.text() {
                        let mut v = Vec::new();
                        for ch in text.chars().iter() {
                            let c = match ch.unicode_char() {
                                Some(c) => c,
                                None => continue,
                            };
                            let b = ch.loose_bounds().or_else(|_| ch.tight_bounds());
                            if let Ok(r) = b {
                                v.push(CharInfo {
                                    ch: c,
                                    left: r.left().value,
                                    bottom: r.bottom().value,
                                    right: r.right().value,
                                    top: r.top().value,
                                });
                            }
                        }
                        all_chars.push((i, v));
                    }
                }
            }
            Ok(all_chars)
        });
        match r {
            Ok(all) => {
                let mut matches = Vec::new();
                let mut hit_pages = Vec::new();
                let mut truncated = false;
                for (page, v) in &all {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.text_cache.insert(*page, v.clone());
                    }
                    if matches.len() >= MAX_TOTAL {
                        truncated = true;
                        continue;
                    }
                    let lc: Vec<char> = v.iter().map(|c| lower1(c.ch)).collect();
                    let mut count_page = 0;
                    let mut k = 0;
                    while k + q.len() <= lc.len() {
                        if lc[k..k + q.len()] == q[..] {
                            matches.push(SearchMatch {
                                page: *page,
                                start: k,
                                end: k + q.len() - 1,
                            });
                            count_page += 1;
                            k += q.len(); // non-overlapping
                            if count_page >= MAX_PER_PAGE || matches.len() >= MAX_TOTAL {
                                if matches.len() >= MAX_TOTAL {
                                    truncated = true;
                                }
                                break;
                            }
                        } else {
                            k += 1;
                        }
                    }
                    if count_page > 0 {
                        hit_pages.push(*page);
                    }
                }
                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                    tab.search_matches = matches;
                    tab.search_hits = hit_pages;
                    tab.search_cursor = 0;
                }
                // Preview behavior: show the results list in the sidebar.
                self.show_sidebar = true;
                self.sidebar_mode = SidebarMode::Results;
                let (n, m) = (
                    self.tabs.get(tab_idx).map(|t| t.search_matches.len()).unwrap_or(0),
                    self.tabs.get(tab_idx).map(|t| t.search_hits.len()).unwrap_or(0),
                );
                if n == 0 {
                    self.status = "Search: no match".to_owned();
                } else {
                    if truncated {
                        self.status = format!(
                            "Search: {n}+ matches ({m} pages, showing first {MAX_TOTAL}), #1"
                        );
                    } else {
                        self.status = format!("Search: {n} matches ({m} pages), #1");
                    }
                    self.goto_match(tab_idx);
                }
            }
            Err(e) => self.status = format!("Search failed: {e}"),
        }
    }

    fn goto_match(&mut self, tab_idx: usize) {
        let (page, cursor, total) = match self.tabs.get(tab_idx) {
            Some(t) if !t.search_matches.is_empty() => {
                let m = t.search_matches[t.search_cursor.min(t.search_matches.len() - 1)];
                (m.page, t.search_cursor, t.search_matches.len())
            }
            _ => return,
        };
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.search_cursor = cursor.min(total - 1);
            let n = tab.pages;
            tab.cur = page.clamp(0, n - 1);
            tab.scroll_target = Some(tab.cur);
            tab.scroll_to_match = true;
        }
        self.status = format!("Search: match {}/{} (page {})", cursor + 1, total, page + 1);
    }

    fn step_match(&mut self, tab_idx: usize, dir: i32) {
        let total = self.tabs.get(tab_idx).map(|t| t.search_matches.len()).unwrap_or(0);
        if total == 0 {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            let c = tab.search_cursor as i32;
            tab.search_cursor = (c + dir).rem_euclid(total as i32) as usize;
        }
        self.goto_match(tab_idx);
    }

    fn clear_search(&mut self, tab_idx: usize) {
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.search_matches.clear();
            tab.search_hits.clear();
            tab.search_cursor = 0;
            tab.search_query.clear();
        }
        self.sidebar_mode = SidebarMode::Thumbs;
        self.status = "Search cleared".to_owned();
    }

    /// Render one page at high resolution and save as PNG, for "print current page".
    fn export_page_png(&mut self, tab_idx: usize, page: i32, width_px: i32) -> Result<PathBuf, String> {
        let img = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pg = document.pages().get(page)?;
            let bmp = pg.render_with_config(
                &PdfRenderConfig::new()
                    .set_target_width(width_px)
                    .set_maximum_height(width_px * 3),
            )?;
            bmp.as_image()
        })?;
        let stem = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.path.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "page".to_owned());
        let out = std::env::temp_dir().join(format!("minipdf_print_{stem}_p{}.png", page + 1));
        img.save(&out).map_err(|e| format!("Export image failed: {e}"))?;
        Ok(out)
    }

    /// Hand a file to the OS print verb (default app's print dialog).
    fn os_print(path: &Path) -> Result<(), String> {
        shell_verb(path, "print")
    }

    /// Open a file with its default app (fallback when the print verb is missing).
    fn os_open(path: &Path) -> Result<(), String> {
        shell_verb(path, "open")
    }

    /// Print the whole PDF of the active tab via the system handler.
    fn print_whole(&mut self, tab_idx: usize) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        match Self::os_print(&path) {
            Ok(()) => self.status = "Sent to system print (whole document)".to_owned(),
            Err(e) => self.status = format!("Print failed: {e}"),
        }
    }

    /// Print the current page: export hi-res PNG, then hand it to system print.
    fn print_current_page(&mut self, tab_idx: usize) {
        let page = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
        match self.export_page_png(tab_idx, page, 2200) {
            Ok(png) => match Self::os_print(&png) {
                Ok(()) => self.status = format!("Sent page {} to system print", page + 1),
                Err(e) => self.status = format!("Print failed: {e}"),
            },
            Err(e) => self.status = e,
        }
    }

    /// Open-with dialog: let the user pick another app for this PDF.
    fn open_with_other(&mut self, tab_idx: usize) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        match std::process::Command::new("rundll32.exe")
            .arg("shell32.dll,OpenAs_RunDLL")
            .arg(&path)
            .spawn()
        {
            Ok(_) => self.status = "Choose an app to open the PDF".to_owned(),
            Err(e) => self.status = format!("Open failed: {e}"),
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        self.status = if self.fullscreen {
            "Fullscreen (F11 or Esc to exit)".to_owned()
        } else {
            "Windowed".to_owned()
        };
    }

    fn goto(&mut self, tab_idx: usize, p: i32) {        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            if tab.pages <= 0 {
                return;
            }
            tab.cur = p.clamp(0, tab.pages - 1);
            tab.scroll_target = Some(tab.cur);
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.tabs.is_empty() {
                ui.label("No files open");
                if ui.button("Open").clicked() {
                    self.pick_files();
                }
                return;
            }
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut switch_to: Option<usize> = None;
                    let mut close: Option<usize> = None;
                    for (idx, t) in self.tabs.iter().enumerate() {
                        let selected = idx == self.active;
                        let name = t.title();
                        let label = if selected {
                            format!("● {}", name)
                        } else {
                            name.clone()
                        };
                        let resp = ui.selectable_label(selected, label);
                        if resp.clicked() {
                            switch_to = Some(idx);
                        }
                        resp.on_hover_text(t.path.to_string_lossy().to_string());
                        // close button per tab
                        if ui.small_button("×").on_hover_text("Close tab (Ctrl+W)").clicked() {
                            close = Some(idx);
                        }
                        ui.separator();
                    }
                    if let Some(c) = close {
                        self.close_tab(c);
                    } else if let Some(s) = switch_to {
                        self.active = s;
                    }
                    if ui.button("+" ).on_hover_text("Open PDFs in new tabs").clicked() {
                        self.pick_files();
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        ui.horizontal(|ui| {
            // ---- File menu (Preview style): Open / Save / Save as ----
            {
                ui.menu_button("File", |ui| {
                    if ui
                        .button("Open…")
                        .on_hover_text("Open PDFs in new tabs (Ctrl+O)")
                        .clicked()
                    {
                        self.pick_files();
                        ui.close();
                    }
                    if ui
                        .button("Save")
                        .on_hover_text("Save the document (Ctrl+S)")
                        .clicked()
                    {
                        self.save_now(tab_idx);
                        ui.close();
                    }
                    if ui
                        .button("Save as…")
                        .on_hover_text("Save a copy to a new file")
                        .clicked()
                    {
                        self.save_as(tab_idx);
                        ui.close();
                    }
                });
            }
            if ui
                .selectable_label(self.show_sidebar, "Sidebar")
                .on_hover_text("Toggle sidebar (F9)")
                .clicked()
            {
                self.show_sidebar = !self.show_sidebar;
            }
            ui.separator();
            let can_nav = pages > 0;
            if ui.add_enabled(can_nav, egui::Button::new("◀")).clicked() {
                let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
                self.goto(tab_idx, cur - 1);
            }
            let mut n = self.tabs.get(tab_idx).map(|t| t.cur + 1).unwrap_or(1);
            let page_resp = ui.add_enabled(
                can_nav,
                egui::DragValue::new(&mut n)
                    .range(1..=pages.max(1))
                    .prefix("Page ")
                    .suffix(""),
            );
            // Commit only: jumping on every keystroke breaks multi-digit input.
            let committed = page_resp.drag_stopped()
                || (page_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
            if committed && can_nav && n - 1 != cur {
                self.goto(tab_idx, n - 1);
            }
            ui.label(format!("/ {pages}"));
            if ui.add_enabled(can_nav, egui::Button::new("▶")).clicked() {
                let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
                self.goto(tab_idx, cur + 1);
            }
            ui.separator();
            let zoom = self.tabs.get(tab_idx).map(|t| t.zoom).unwrap_or(1.0);
            if ui
                .add_enabled(can_nav, egui::Button::new("-" ))
                .on_hover_text("Zoom out (or Ctrl+wheel)")
                .clicked()
            {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = (t.zoom - 0.1).max(0.2);
                    t.page_tex.clear();
                }
            }
            ui.label(format!("{:.0}%", zoom * 100.0));
            if ui
                .add_enabled(can_nav, egui::Button::new("+" ))
                .on_hover_text("Zoom in (or Ctrl+wheel)")
                .clicked()
            {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = (t.zoom + 0.1).min(4.0);
                    t.page_tex.clear();
                }
            }
            if ui.add_enabled(can_nav, egui::Button::new("Fit width")).clicked() {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = 1.0;
                    t.page_tex.clear();
                }
            }
            ui.separator();
            // fullscreen toggle
            {
                let label = if self.fullscreen { "Exit fullscreen" } else { "Fullscreen" };
                if ui
                    .button(label)
                    .on_hover_text("Toggle fullscreen (F11, Esc exits)")
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.toggle_fullscreen(&ctx);
                }
            }
            ui.separator();
            // copy selected text
            let (has_sel, sel_len) = match self.tabs.get(tab_idx) {
                Some(t) => match t.selection {
                    Some(s) => (true, s.len()),
                    None => (false, 0),
                },
                None => (false, 0),
            };
            if ui
                .add_enabled(has_sel, egui::Button::new(format!("Copy ({sel_len})")))
                .on_hover_text("Copy selected text (Ctrl+C). Drag on a page to select.")
                .clicked()
            {
                self.copy_selection(ui.ctx());
            }
            // one-click highlight shortcut: Lucide "highlighter" glyph
            // (ISC licensed, baked on toolbar gray), with the selection actions.
            // Uses the pre-decoded startup texture: synchronous, never the ⚠ placeholder.
            {
                let h = ui.spacing().interact_size.y.max(20.0);
                let img = if has_sel {
                    match &self.tex_highlight {
                        Some(tex) => egui::Image::new((tex.id(), egui::vec2(18.0, 18.0)))
                            .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                        None => egui::Image::from_bytes(
                            "bytes://minipdf/highlight.png",
                            include_bytes!("../assets/highlight.png").as_slice(),
                        )
                        .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                    }
                } else {
                    match &self.tex_highlight_disabled {
                        Some(tex) => egui::Image::new((tex.id(), egui::vec2(18.0, 18.0)))
                            .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                        None => egui::Image::from_bytes(
                            "bytes://minipdf/highlight-disabled.png",
                            include_bytes!("../assets/highlight-disabled.png").as_slice(),
                        )
                        .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                    }
                };
                let resp = ui
                    .add_sized(egui::vec2(30.0, h), egui::Button::image(img))
                    .on_hover_text("Highlight the selection (uses the Markup color)");
                if has_sel && resp.clicked() {
                    self.add_markup(tab_idx, MarkupKind::Highlight);
                }
            }
            // ---- markup menu (Preview: highlight / underline / strike out) ----
            {
                let can_undo = self
                    .tabs
                    .get(tab_idx)
                    .map(|t| !t.markup_stack.is_empty())
                    .unwrap_or(false);
                ui.menu_button("Markup", |ui| {
                    if ui
                        .add_enabled(has_sel, egui::Button::new("Highlight selection"))
                        .on_hover_text("Highlight the selected text (yellow by default)")
                        .clicked()
                    {
                        self.add_markup(tab_idx, MarkupKind::Highlight);
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_sel, egui::Button::new("Underline selection"))
                        .clicked()
                    {
                        self.add_markup(tab_idx, MarkupKind::Underline);
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_sel, egui::Button::new("Strikethrough selection"))
                        .clicked()
                    {
                        self.add_markup(tab_idx, MarkupKind::Strikeout);
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("Highlight color", |ui| {
                        for (name, rgb) in HIGHLIGHT_COLORS {
                            let cur = self
                                .tabs
                                .get(tab_idx)
                                .map(|t| t.highlight_rgb == *rgb)
                                .unwrap_or(false);
                            if ui.selectable_label(cur, *name).clicked() {
                                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                    tab.highlight_rgb = *rgb;
                                }
                                self.status = format!("Highlight color: {name}");
                                ui.close();
                            }
                        }
                    });
                    if ui
                        .add_enabled(can_undo, egui::Button::new("Undo last markup"))
                        .clicked()
                    {
                        self.undo_markup(tab_idx);
                        ui.close();
                    }
                });
            }
            ui.separator();
            // ---- print menu ----
            {
                ui.menu_button("Print", |ui| {
                    if ui
                        .button("Whole document (system print dialog)")
                        .on_hover_text("Print via the default PDF app (Ctrl+P)")
                        .clicked()
                    {
                        self.print_whole(tab_idx);
                        ui.close();
                    }
                    if ui
                        .button("Current page (export hi-res image)")
                        .on_hover_text("Export the current page as hi-res PNG, then print it")
                        .clicked()
                    {
                        self.print_current_page(tab_idx);
                        ui.close();
                    }
                    if ui
                        .button("Open with other app…")
                        .on_hover_text("Pick an app to open this PDF with")
                        .clicked()
                    {
                        self.open_with_other(tab_idx);
                        ui.close();
                    }
                });
            }
            ui.separator();
            // ---- search at the right end (Preview layout; RTL insertion order) ----
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("×")
                    .on_hover_text("Clear search highlights")
                    .clicked()
                {
                    self.clear_search(tab_idx);
                }
                {
                    let (total, cursor) = self
                        .tabs
                        .get(tab_idx)
                        .map(|t| (t.search_matches.len(), t.search_cursor))
                        .unwrap_or((0, 0));
                    let has = total > 0;
                    if ui
                        .add_enabled(has, egui::Button::new("▶"))
                        .on_hover_text("Next match (Enter or F3)")
                        .clicked()
                    {
                        self.step_match(tab_idx, 1);
                    }
                    if has {
                        ui.label(format!("{}/{}", cursor + 1, total));
                    } else {
                        ui.label("0/0");
                    }
                    if ui
                        .add_enabled(has, egui::Button::new("◀"))
                        .on_hover_text("Previous match (Shift+Enter or Shift+F3)")
                        .clicked()
                    {
                        self.step_match(tab_idx, -1);
                    }
                }
                enum SearchAct {
                    None,
                    Run,
                    Next,
                    Prev,
                }
                let mut act = SearchAct::None;
                let want_focus = self.focus_search;
                {
                    if let Some(t) = self.tabs.get_mut(tab_idx) {
                        let resp = ui.add_enabled(
                            can_nav,
                            egui::TextEdit::singleline(&mut t.search_text)
                                .hint_text("Search, Enter/F3 = next")
                                .desired_width(130.0)
                                .frame(
                                    egui::Frame::new()
                                        .fill(egui::Color32::WHITE)
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(198, 198, 200),
                                        ))
                                        .corner_radius(egui::CornerRadius::same(6))
                                        .inner_margin(egui::Margin::symmetric(6, 3)),
                                ),
                        );
                        if want_focus {
                            resp.request_focus();
                        }
                        if (resp.has_focus() || resp.lost_focus())
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            let stale = t.search_text.trim().to_lowercase() != t.search_query;
                            let shift = ui.input(|i| i.modifiers.shift);
                            act = if stale {
                                SearchAct::Run
                            } else if shift {
                                SearchAct::Prev
                            } else if t.search_matches.is_empty() {
                                SearchAct::Run
                            } else {
                                SearchAct::Next
                            };
                        }
                    }
                }
                self.focus_search = false;
                match act {
                    SearchAct::Run => self.run_search(tab_idx),
                    SearchAct::Next => self.step_match(tab_idx, 1),
                    SearchAct::Prev => self.step_match(tab_idx, -1),
                    SearchAct::None => {}
                }
                ui.label("Search:");
            });
        });
    }

    /// Thumbnails: render only the visible range, max 3 per frame, fill in gradually.
    fn thumbs(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        let mut budget = 3;
        let mut pending = false;
        let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
        // search match counts per page, for badges
        let match_counts: HashMap<i32, usize> = self
            .tabs
            .get(tab_idx)
            .map(|t| {
                let mut m = HashMap::new();
                for s in &t.search_matches {
                    *m.entry(s.page).or_insert(0) += 1;
                }
                m
            })
            .unwrap_or_default();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for i in 0..pages {
                let selected = i == cur;
                let w = (ui.available_width() - 12.0).clamp(70.0, 420.0);
                let tex = self
                    .tabs
                    .get(tab_idx)
                    .and_then(|t| t.thumb_tex.get(&i).cloned());
                if let Some(tex) = tex {
                    let size = tex.size_vec2();
                    let h = w * size.y / size.x.max(1.0);
                    let btn =
                        egui::ImageButton::new(egui::Image::new((tex.id(), egui::vec2(w, h))))
                            .selected(selected);
                    if ui.add_sized([w + 8.0, h + 4.0], btn).clicked() {
                        self.goto(tab_idx, i);
                    }
                    match match_counts.get(&i) {
                        Some(n) => ui.label(format!("Page {} ({n} matches)", i + 1)),
                        None => ui.label(format!("Page {}", i + 1)),
                    };
                } else {
                    let resp = ui.add_sized(
                        [w + 8.0, 60.0],
                        egui::Button::new(format!("Page {}", i + 1)).selected(selected),
                    );
                    if resp.clicked() {
                        self.goto(tab_idx, i);
                    }
                    let visible = ui.is_rect_visible(resp.rect);
                    if (visible || i == cur) && budget > 0 {
                        self.render_tex(tab_idx, ui.ctx(), i, THUMB_WIDTH, false);
                        budget -= 1;
                    } else if visible {
                        pending = true;
                    }
                }
                ui.separator();
            }
        });
        if pending {
            ui.ctx().request_repaint();
        }
    }

    /// Sidebar with Preview-style mode switch: thumbnails or search results.
    fn sidebar(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        let n_results = self
            .tabs
            .get(tab_idx)
            .map(|t| t.search_matches.len())
            .unwrap_or(0);
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.sidebar_mode == SidebarMode::Thumbs, "Thumbnails")
                .clicked()
            {
                self.sidebar_mode = SidebarMode::Thumbs;
            }
            if ui
                .selectable_label(
                    self.sidebar_mode == SidebarMode::Results,
                    format!("Results ({n_results})"),
                )
                .clicked()
            {
                self.sidebar_mode = SidebarMode::Results;
            }
        });
        ui.separator();
        if self.sidebar_mode == SidebarMode::Results {
            self.results(ui, tab_idx);
        } else {
            self.thumbs(ui, tab_idx, pages);
        }
    }

    /// One-line snippet around a match for the results list.
    fn match_snippet(tab: &DocTab, m: &SearchMatch) -> String {
        const CTX: usize = 28;
        match tab.text_cache.get(&m.page) {
            Some(cs) if !cs.is_empty() => {
                let s = m.start.min(cs.len() - 1);
                let e = m.end.min(cs.len() - 1);
                let a = s.saturating_sub(CTX);
                let b = (e + CTX + 1).min(cs.len());
                let raw: String = cs[a..b].iter().map(|c| c.ch).collect();
                let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("p.{} — {}", m.page + 1, flat)
            }
            _ => format!("Page {}", m.page + 1),
        }
    }

    /// Search-result list: every match with a snippet, click to jump.
    fn results(&mut self, ui: &mut egui::Ui, tab_idx: usize) {
        let mut jump_to: Option<usize> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let (total, cursor) = match self.tabs.get(tab_idx) {
                Some(t) => (t.search_matches.len(), t.search_cursor),
                None => (0, 0),
            };
            if total == 0 {
                ui.label("No matches. Type a keyword above and press Enter.");
                return;
            }
            for j in 0..total {
                let (text, page) = match self.tabs.get(tab_idx) {
                    Some(t) => {
                        let m = &t.search_matches[j];
                        (Self::match_snippet(t, m), m.page)
                    }
                    None => break,
                };
                if ui
                    .selectable_label(j == cursor, text)
                    .on_hover_text(format!("Go to match {} (page {})", j + 1, page + 1))
                    .clicked()
                {
                    jump_to = Some(j);
                }
                ui.separator();
            }
        });
        if let Some(j) = jump_to {
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                if j < tab.search_matches.len() {
                    tab.search_cursor = j;
                }
            }
            self.goto_match(tab_idx);
        }
    }

    /// Continuous vertical reading with drag-to-select text overlay.
    fn body(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        // keyboard: arrows/PgUp/PgDn jump to prev/next page
        if ui.input(|i| i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::PageDown))
        {
            let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
            self.goto(tab_idx, cur + 1);
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::PageUp)) {
            let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
            self.goto(tab_idx, cur - 1);
        }
        // F3 / Shift+F3: next / previous search match (Windows convention)
        if ui.input(|i| i.key_pressed(egui::Key::F3)) {
            let dir = if ui.input(|i| i.modifiers.shift) {
                -1
            } else {
                1
            };
            self.step_match(tab_idx, dir);
        }
        // Ctrl+A: select all text on current page
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
            let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
            self.ensure_text(tab_idx, cur);
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                if let Some(chars) = tab.text_cache.get(&cur) {
                    if !chars.is_empty() {
                        tab.selection = Some(TextSelection::new(cur, 0, chars.len() - 1));
                        self.status = format!("Selected {} chars on page {}", chars.len(), cur + 1);
                    } else {
                        self.status = "This page has no selectable text".to_owned();
                    }
                }
            }
        }
        // Ctrl+wheel = zoom (consumed). Plain wheel scrolls the document.
        let (dy, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl));
        if ctrl && dy != 0.0 {
            let f = if dy > 0.0 { 1.1 } else { 1.0 / 1.1 };
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                let z = (tab.zoom * f).clamp(0.2, 4.0);
                if (z - tab.zoom).abs() > f32::EPSILON {
                    tab.zoom = z;
                    tab.page_tex.clear();
                }
            }
            ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
        }
        let zoom = self.tabs.get(tab_idx).map(|t| t.zoom).unwrap_or(1.0);
        let mut budget = 2;
        let mut pending = false;
        let mut first_visible: Option<i32> = None;
        let mut scrolled = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            // Preview-style gray canvas behind the pages.
            {
                let canvas = preview_canvas(ui.visuals().dark_mode);
                ui.painter().rect_filled(ui.clip_rect(), 0.0, canvas);
            }
            let vp = ui.clip_rect().width().max(50.0);
            let render_w = ((RENDER_BASE_WIDTH as f32) * zoom) as i32;
            for i in 0..pages {
                let (pw, ph) = self
                    .tabs
                    .get(tab_idx)
                    .and_then(|t| t.aspects.get(&i).copied())
                    .unwrap_or((1.0, 1.4142));
                let disp_w = (render_w as f32).min(vp * zoom);
                let disp = egui::vec2(disp_w, disp_w * ph / pw.max(0.01));
                let est = egui::Rect::from_min_size(ui.cursor().min, disp);
                let visible = ui.is_rect_visible(est);
                let tex = self
                    .tabs
                    .get(tab_idx)
                    .and_then(|t| t.page_tex.get(&(i, render_w as u32)).cloned());
                if let Some(tex) = tex {
                    let left = ((vp - disp.x) / 2.0).max(0.0);
                    let origin = egui::pos2(ui.cursor().min.x + left, ui.cursor().min.y);
                    let img_rect = egui::Rect::from_min_size(origin, disp);
                    // drop shadow first, page on top (Preview look)
                    {
                        let painter = ui.painter();
                        let shadow = img_rect.expand(2.0).translate(egui::vec2(0.0, 4.0));
                        painter.rect_filled(shadow, 3.0, egui::Color32::from_black_alpha(28));
                        painter.image(
                            tex.id(),
                            img_rect,
                            egui::Rect::from_min_max(
                                egui::pos2(0.0, 0.0),
                                egui::pos2(1.0, 1.0),
                            ),
                            egui::Color32::WHITE,
                        );
                    }
                    let _ = ui.allocate_rect(img_rect, egui::Sense::hover());
                    if visible && first_visible.is_none() {
                        first_visible = Some(i);
                    }
                    let scroll_target =
                        self.tabs.get(tab_idx).and_then(|t| t.scroll_target);
                    if scroll_target == Some(i) && !scrolled {
                        // Search jumps center the current match; plain jumps center the page.
                        let mut dest = img_rect;
                        if let Some(tab) = self.tabs.get(tab_idx) {
                            if tab.scroll_to_match {
                                if let Some(m) =
                                    tab.search_matches.get(tab.search_cursor).filter(|m| m.page == i)
                                {
                                    if let Some(chars) = tab.text_cache.get(&i) {
                                        let last = chars.len().saturating_sub(1);
                                        let mut union: Option<egui::Rect> = None;
                                        for k in m.start..=m.end.min(last) {
                                            if let Some(r) = pdf_rect_to_screen(
                                                &chars[k], &img_rect, pw, ph,
                                            ) {
                                                union = Some(match union {
                                                    Some(u) => u.union(r),
                                                    None => r,
                                                });
                                            }
                                        }
                                        if let Some(u) = union {
                                            dest = u;
                                        }
                                    }
                                }
                            }
                        }
                        ui.scroll_to_rect(dest, Some(egui::Align::Center));
                        scrolled = true;
                    }
                    // ---- text selection layer ----
                    self.ensure_text(tab_idx, i);
                    self.ensure_links(tab_idx, i);
                    // paint search matches: yellow, current one orange
                    if let Some(tab) = self.tabs.get(tab_idx) {
                        if !tab.search_matches.is_empty() {
                            if let Some(chars) = tab.text_cache.get(&i) {
                                let painter = ui.painter();
                                let last = chars.len().saturating_sub(1);
                                for (mi, m) in tab
                                    .search_matches
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, m)| m.page == i)
                                {
                                    let col = if mi == tab.search_cursor {
                                        egui::Color32::from_rgba_unmultiplied(255, 140, 0, 110)
                                    } else {
                                        egui::Color32::from_rgba_unmultiplied(255, 235, 0, 90)
                                    };
                                    for k in m.start..=m.end.min(last) {
                                        if let Some(r) =
                                            pdf_rect_to_screen(&chars[k], &img_rect, pw, ph)
                                        {
                                            painter.rect_filled(r, 1.0, col);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // paint text selection highlight for this page
                    if let Some(tab) = self.tabs.get(tab_idx) {
                        if let Some(sel) = tab.selection {
                            if sel.page == i {
                                if let Some(chars) = tab.text_cache.get(&i) {
                                    let painter = ui.painter();
                                    let sel_col =
                                        preview_selection(ui.visuals().dark_mode);
                                    for k in sel.start..=sel.end.min(chars.len().saturating_sub(1)) {
                                        if let Some(r) = pdf_rect_to_screen(&chars[k], &img_rect, pw, ph) {
                                            painter.rect_filled(
                                                r,
                                                1.0,
                                                sel_col,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ---- links: underline, hover highlight, tooltip ----
                    let page_links: Vec<PageLink> = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.link_cache.get(&i).cloned())
                        .unwrap_or_default();
                    let hover_idx: Option<usize> = ui
                        .ctx()
                        .pointer_hover_pos()
                        .filter(|p| img_rect.contains(*p))
                        .and_then(|p| screen_to_pdf(p, &img_rect, pw, ph))
                        .and_then(|(px, py)| {
                            page_links.iter().position(|l| l.contains(px, py))
                        });
                    {
                        let painter = ui.painter();
                        let link_col = preview_link(ui.visuals().dark_mode);
                        for (k, l) in page_links.iter().enumerate() {
                            if let Some(r) = pdf_rect_to_screen_raw(
                                l.left, l.bottom, l.right, l.top, &img_rect, pw, ph,
                            ) {
                                if Some(k) == hover_idx {
                                    painter.rect_filled(
                                        r,
                                        2.0,
                                        egui::Color32::from_rgba_unmultiplied(80, 140, 255, 70),
                                    );
                                }
                                let y = r.max.y - 1.0;
                                painter.line_segment(
                                    [egui::pos2(r.min.x, y), egui::pos2(r.max.x, y)],
                                    egui::Stroke::new(1.5, link_col),
                                );
                            }
                        }
                    }
                    // interaction for drag-select (only when pointer over image)
                    let sid = egui::Id::new(format!("page-sel-{tab_idx}-{i}"));
                    let mut inter = ui.interact(img_rect, sid, egui::Sense::click_and_drag());
                    if let Some(l) = hover_idx.and_then(|k| page_links.get(k)) {
                        inter = inter.on_hover_text(l.label());
                    }
                    // right-click menu: copy / select all / link actions
                    {
                        let menu_link: Option<PageLink> =
                            hover_idx.and_then(|k| page_links.get(k).cloned());
                        inter.context_menu(|ui| {
                            let has_sel = self
                                .tabs
                                .get(tab_idx)
                                .and_then(|t| t.selection)
                                .is_some();
                            if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                                let ctx = ui.ctx().clone();
                                self.copy_selection(&ctx);
                                ui.close();
                            }
                            if ui.button("Select all on this page").clicked() {
                                self.ensure_text(tab_idx, i);
                                if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                    if let Some(chars) = tab.text_cache.get(&i) {
                                        if !chars.is_empty() {
                                            tab.selection =
                                                Some(TextSelection::new(i, 0, chars.len() - 1));
                                            self.status = format!(
                                                "Selected {} chars on page {}",
                                                chars.len(),
                                                i + 1
                                            );
                                        }
                                    }
                                }
                                ui.close();
                            }
                            // Preview-style text markup on the selection
                            if has_sel {
                                ui.separator();
                                if ui.button("Highlight").clicked() {
                                    self.add_markup(tab_idx, MarkupKind::Highlight);
                                    ui.close();
                                }
                                if ui.button("Underline").clicked() {
                                    self.add_markup(tab_idx, MarkupKind::Underline);
                                    ui.close();
                                }
                                if ui.button("Strikethrough").clicked() {
                                    self.add_markup(tab_idx, MarkupKind::Strikeout);
                                    ui.close();
                                }
                                ui.menu_button("Highlight color", |ui| {
                                    for (name, rgb) in HIGHLIGHT_COLORS {
                                        let cur = self
                                            .tabs
                                            .get(tab_idx)
                                            .map(|t| t.highlight_rgb == *rgb)
                                            .unwrap_or(false);
                                        if ui.selectable_label(cur, *name).clicked() {
                                            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                                tab.highlight_rgb = *rgb;
                                            }
                                            self.status =
                                                format!("Highlight color: {name}").to_owned();
                                            ui.close();
                                        }
                                    }
                                });
                            }
                            if ui
                                .add_enabled(
                                    self.tabs
                                        .get(tab_idx)
                                        .map(|t| !t.markup_stack.is_empty())
                                        .unwrap_or(false),
                                    egui::Button::new("Undo last markup"),
                                )
                                .clicked()
                            {
                                self.undo_markup(tab_idx);
                                ui.close();
                            }
                            if let Some(l) = &menu_link {
                                ui.separator();
                                match &l.target {
                                    LinkTarget::Url(u) => {
                                        let addr = u.clone();
                                        if ui.button("Open link").clicked() {
                                            let lc = l.clone();
                                            self.open_link(tab_idx, &lc);
                                            ui.close();
                                        }
                                        if ui.button("Copy link address").clicked() {
                                            ui.ctx().copy_text(addr);
                                            self.status = "Link address copied".to_owned();
                                            ui.close();
                                        }
                                    }
                                    LinkTarget::Page(p) => {
                                        let dest = *p;
                                        if ui.button(format!("Go to page {}", dest + 1)).clicked()
                                        {
                                            self.goto(tab_idx, dest);
                                            ui.close();
                                        }
                                    }
                                }
                            }
                        });
                    }
                    if inter.drag_started() {
                        if let Some(origin) = inter.interact_pointer_pos() {
                            if let Some((px, py)) = screen_to_pdf(origin, &img_rect, pw, ph) {
                                let idx = self
                                    .tabs
                                    .get(tab_idx)
                                    .and_then(|t| t.text_cache.get(&i))
                                    .and_then(|cs| pick_char(cs, px, py));
                                if let Some(a) = idx {
                                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                        tab.drag_anchor = Some((i, a));
                                        tab.selection = Some(TextSelection::new(i, a, a));
                                    }
                                } else if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                    tab.drag_anchor = None;
                                    if tab.selection.map(|s| s.page) == Some(i) {
                                        tab.selection = None;
                                    }
                                }
                            }
                        }
                    } else if inter.dragged() {
                        let anchor = self.tabs.get(tab_idx).and_then(|t| t.drag_anchor);
                        if let Some((apage, a)) = anchor {
                            if apage == i {
                                if let Some(pos) = inter.interact_pointer_pos() {
                                    if let Some((px, py)) = screen_to_pdf(pos, &img_rect, pw, ph) {
                                        let b = self
                                            .tabs
                                            .get(tab_idx)
                                            .and_then(|t| t.text_cache.get(&i))
                                            .and_then(|cs| pick_char(cs, px, py));
                                        if let Some(b) = b {
                                            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                                tab.selection = Some(TextSelection::new(i, a, b));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if inter.drag_stopped() {
                        if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            tab.drag_anchor = None;
                            if let Some(sel) = tab.selection {
                                if sel.page == i {
                                    let n = sel.len();
                                    let txt_ok = tab
                                        .text_cache
                                        .get(&i)
                                        .map(|cs| {
                                            cs[sel.start..=sel.end.min(cs.len().saturating_sub(1))]
                                                .iter()
                                                .any(|c| !c.ch.is_whitespace())
                                        })
                                        .unwrap_or(false);
                                    if n == 0 || !txt_ok {
                                        // keep tiny whitespace-only drags out
                                    }
                                    self.status = format!("Selected {n} chars on page {}", i + 1);
                                }
                            }
                        }
                    } else if inter.clicked() {
                        // click on a link activates it; otherwise clear selection here
                        let hit_link: Option<PageLink> = inter
                            .interact_pointer_pos()
                            .and_then(|pos| screen_to_pdf(pos, &img_rect, pw, ph))
                            .and_then(|(px, py)| {
                                page_links.iter().find(|l| l.contains(px, py)).cloned()
                            });
                        if let Some(l) = hit_link {
                            self.open_link(tab_idx, &l);
                        } else if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            if tab.selection.map(|s| s.page) == Some(i) {
                                tab.selection = None;
                            }
                        }
                    }
                    // hand cursor over links, text beam elsewhere on the page
                    if hover_idx.is_some() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    } else if inter.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                    }
                } else {
                    // placeholder keeps layout stable until rendered (same centering as pages)
                    let left = ((vp - disp.x) / 2.0).max(0.0);
                    let origin = egui::pos2(ui.cursor().min.x + left, ui.cursor().min.y);
                    let rect = egui::Rect::from_min_size(origin, disp);
                    let _ = ui.allocate_rect(rect, egui::Sense::hover());
                    ui.put(rect, egui::Label::new("Loading…"));
                    // A jump target must render even while off-screen,
                    // otherwise scroll_to_rect never fires and the jump stalls.
                    let is_target =
                        self.tabs.get(tab_idx).and_then(|t| t.scroll_target) == Some(i);
                    if visible || is_target {
                        if visible && first_visible.is_none() {
                            first_visible = Some(i);
                        }
                        if budget > 0 {
                            budget -= 1;
                            self.render_tex(tab_idx, ui.ctx(), i, render_w, true);
                        } else {
                            pending = true;
                        }
                    }
                }
                ui.add_space(10.0);
            }
        });
        if pending {
            ui.ctx().request_repaint();
        }
        if scrolled {
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                tab.scroll_target = None;
                tab.scroll_to_match = false;
            }
        }
        // keep toolbar / thumbnails in sync while free-scrolling
        if let Some(v) = first_visible {
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                if tab.scroll_target.is_none() {
                    tab.cur = v;
                }
            }
        }
    }
}

fn log_line(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("minipdf.log"))
    {
        let _ = writeln!(f, "[{:?}] {msg}", std::time::SystemTime::now());
    }
}

impl eframe::App for MiniPdf {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        use std::sync::Once;
        static FIRST: Once = Once::new();
        FIRST.call_once(|| log_line("first frame"));
        // drag & drop: open every pdf as its own tab
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        let mut opened_any = false;
        for p in dropped {
            if p.extension()
                .map(|e| e.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false)
            {
                self.open_file(p);
                opened_any = true;
            } else {
                self.status = "Only .pdf files are supported".to_owned();
            }
        }
        let _ = opened_any;
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
            self.pick_files();
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::W)) && !self.tabs.is_empty() {
            let idx = self.active;
            self.close_tab(idx);
        }
        // Ctrl+Tab / Ctrl+Shift+Tab to cycle tabs
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) && self.tabs.len() > 1 {
            let shift = ui.input(|i| i.modifiers.shift);
            if shift {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
            } else {
                self.active = (self.active + 1) % self.tabs.len();
            }
        }
        // Ctrl+C copies current selection
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::C)) {
            if self.active_tab().and_then(|t| t.selection).is_some() {
                let ctx = ui.ctx().clone();
                self.copy_selection(&ctx);
            }
        }
        // Ctrl+P prints the whole document of the active tab
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P)) && !self.tabs.is_empty()
        {
            let idx = self.active;
            self.print_whole(idx);
        }
        // Ctrl+S saves the active tab
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) && !self.tabs.is_empty()
        {
            let idx = self.active;
            self.save_now(idx);
        }
        // Ctrl+F focuses the search field
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F)) && !self.tabs.is_empty()
        {
            self.focus_search = true;
        }
        if ui.input(|i| i.key_pressed(egui::Key::F9)) {
            self.show_sidebar = !self.show_sidebar;
        }
        // F11 toggles fullscreen, Esc exits it
        if ui.input(|i| i.key_pressed(egui::Key::F11)) {
            let ctx = ui.ctx().clone();
            self.toggle_fullscreen(&ctx);
        } else if self.fullscreen && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            let ctx = ui.ctx().clone();
            self.toggle_fullscreen(&ctx);
        }

        let has_tabs = !self.tabs.is_empty();
        let (pages, active_idx) = match self.active_tab() {
            Some(t) => (t.pages, self.active),
            None => (0, 0),
        };
        // Preview-style window title: current file name.
        {
            let title = match self.active_tab() {
                Some(t) => format!("{} — MiniPDF", t.title()),
                None => "MiniPDF".to_owned(),
            };
            if title != self.last_title {
                self.last_title = title.clone();
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(title));
            }
        }

        // Fullscreen = content only: no toolbar, no sidebar, no status bar.
        let fs = self.fullscreen;
        let dark = ui.visuals().dark_mode;
        let (toolbar_fill, sidebar_fill, status_fill) = preview_chrome(dark);
        if !fs {
            let top_out = egui::TopBottomPanel::top("toolbar")
                .frame(egui::Frame::side_top_panel(ui.style()).fill(toolbar_fill))
                .show_inside(ui, |ui| {
                    ui.vertical(|ui| {
                        self.tab_bar(ui);
                        if has_tabs {
                            self.toolbar(ui, active_idx, pages);
                        }
                    });
                });
            paint_groove(ui, top_out.response.rect, 0);
        }
        if has_tabs && self.show_sidebar && !fs {
            let side_out = egui::SidePanel::left("sidebar")
                .resizable(true)
                .default_size(220.0)
                .frame(egui::Frame::side_top_panel(ui.style()).fill(sidebar_fill))
                .show_inside(ui, |ui| {
                    self.sidebar(ui, active_idx, pages);
                });
            paint_groove(ui, side_out.response.rect, 2);
        }
        if has_tabs {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                if !fs {
                    ui.small(
                        egui::RichText::new("Drag to select text • Right-click for menu • Links are clickable • Ctrl+F search • Ctrl+P print")
                            .color(egui::Color32::from_rgb(99, 99, 102)),
                    );
                }
                self.body(ui, active_idx, pages);
            });
            // floating exit in fullscreen (toolbar is hidden)
            if fs {
                egui::Area::new(egui::Id::new("fs-exit"))
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
                    .show(ui.ctx(), |ui| {
                        if ui
                            .small_button("Exit fullscreen (F11)")
                            .on_hover_text("Exit fullscreen (F11 or Esc)")
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.toggle_fullscreen(&ctx);
                        }
                    });
            }
        } else {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        if let Some(tex) = &self.tex_logo {
                            ui.add(
                                egui::Image::new((tex.id(), egui::vec2(112.0, 112.0)))
                                    .max_size(egui::vec2(112.0, 112.0)),
                            );
                        } else {
                            ui.add(
                                egui::Image::from_bytes(
                                    "bytes://minipdf/logo-256.png",
                                    include_bytes!("../assets/logo-256.png").as_slice(),
                                )
                                .max_size(egui::vec2(112.0, 112.0)),
                            );
                        }
                        ui.add_space(8.0);
                        ui.heading("MiniPDF");
                        ui.label("Drag PDF files here, or press Ctrl+O");
                        ui.label("(multiple files open in tabs)");
                    });
                });
            });
        }
        if !fs {
            let bot_out = egui::TopBottomPanel::bottom("status")
                .frame(egui::Frame::side_top_panel(ui.style()).fill(status_fill))
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(&self.status);
                        if let Some(d) = self.active_tab() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(d.path.to_string_lossy().to_string());
                            });
                        }
                    });
                });
            paint_groove(ui, bot_out.response.rect, 1);
        }
    }
}

/// CJK fallback font for the UI itself (buttons / status bar).
fn setup_cjk_ui_font(ctx: &egui::Context) {
    let path = match find_cjk_font_path() {
        Some(p) => p,
        None => return,
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
    for fam in [
        egui::FontFamily::Proportional,
        egui::FontFamily::Monospace,
    ] {
        fonts.families.entry(fam).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Decode an embedded PNG into a texture synchronously.
/// Unlike `Image::from_bytes` (async bytes-loader), this can never show the ⚠ placeholder.
fn load_embedded_tex(
    ctx: &egui::Context,
    name: &str,
    bytes: &'static [u8],
) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let cimg = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
    Some(ctx.load_texture(name, cimg, egui::TextureOptions::LINEAR))
}

fn find_window_icon() -> Option<egui::IconData> {
    // Embedded logo first: always available, no files needed next to the exe.
    if let Ok(img) = image::load_from_memory(include_bytes!("../assets/logo-256.png").as_slice())
    {
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        return Some(egui::IconData {
            rgba: rgba.into_raw(),
            width: w,
            height: h,
        });
    }
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("logo-256.png"));
            cands.push(dir.join("acro_icon_0.png"));
        }
    }
    cands.push(PathBuf::from("assets/logo-256.png"));
    cands.push(PathBuf::from("assets/acro_icon_0.png"));
    for p in cands {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(img) = image::load_from_memory(&bytes) {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                return Some(egui::IconData {
                    rgba: rgba.into_raw(),
                    width: w,
                    height: h,
                });
            }
        }
    }
    None
}

fn main() -> eframe::Result<()> {
    std::panic::set_hook(Box::new(|info| log_line(&format!("PANIC: {info}"))));
    log_line("main enter");
    // support opening multiple pdfs from command line, each in its own tab
    let mut args = std::env::args_os();
    let _exe = args.next();
    let pdf_args: Vec<PathBuf> = args
        .map(PathBuf::from)
        .filter(|p| {
            p.extension()
                .map(|e| e.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false)
                && p.exists()
        })
        .collect();

    let mut app = MiniPdf::default();
    for p in pdf_args {
        log_line(&format!("opening arg: {}", p.display()));
        app.open_file(p);
    }
    log_line(&format!("open done: {}", app.status));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 750.0])
        .with_title("MiniPDF");
    if let Some(icon) = find_window_icon() {
        log_line("window icon loaded");
        viewport = viewport.with_icon(icon);
    } else {
        log_line("window icon MISSING");
    }
    log_line("run_native enter");
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "MiniPDF",
        options,
        Box::new(|cc| {
            // Classic Catalina look: always light, never follow the OS dark theme.
            // Flat transparent buttons with dark-gray glyphs; hover/pressed are
            // faint black washes (rgba .06/.12). Search field keeps its own
            // hairline border via top-level bg_stroke + radius (TextEdit path).
            let mut visuals = egui::Visuals::light();
            let flat_radius = egui::CornerRadius::same(4);
            let glyph = egui::Stroke::new(1.0, egui::Color32::from_gray(77));
            let glyph_hover = egui::Stroke::new(1.0, egui::Color32::from_gray(38));
            visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
            visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.inactive.corner_radius = flat_radius;
            visuals.widgets.inactive.fg_stroke = glyph;
            visuals.widgets.hovered.bg_fill =
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 15);
            visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.hovered.corner_radius = flat_radius;
            visuals.widgets.hovered.fg_stroke = glyph_hover;
            visuals.widgets.active.bg_fill =
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 31);
            visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.active.corner_radius = flat_radius;
            visuals.widgets.active.fg_stroke = glyph_hover;
            cc.egui_ctx.set_visuals(visuals);
            // Pre-decode UI icons to textures (synchronous; never shows ⚠).
            app.tex_highlight = load_embedded_tex(
                &cc.egui_ctx,
                "minipdf-highlight",
                include_bytes!("../assets/highlight.png"),
            );
            app.tex_highlight_disabled = load_embedded_tex(
                &cc.egui_ctx,
                "minipdf-highlight-disabled",
                include_bytes!("../assets/highlight-disabled.png"),
            );
            app.tex_logo = load_embedded_tex(
                &cc.egui_ctx,
                "minipdf-logo",
                include_bytes!("../assets/logo-256.png"),
            );
            cc.egui_ctx.include_bytes(
                "bytes://minipdf/highlight.png",
                include_bytes!("../assets/highlight.png"),
            );
            cc.egui_ctx.include_bytes(
                "bytes://minipdf/highlight-disabled.png",
                include_bytes!("../assets/highlight-disabled.png"),
            );
            setup_cjk_ui_font(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
