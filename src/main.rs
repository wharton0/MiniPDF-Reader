#![windows_subsystem = "windows"]
// MiniPDF - minimal local PDF reader (Rust + Pdfium + egui)
// No background services, no network, no registry writes.
// Features: tabs / open / pages / zoom / thumbnails / search(highlight+jump) /
// text select+copy / print(whole doc + current page) / wheel paging.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

mod pdf_util;
mod platform;
mod search;
mod theme;
mod types;

use eframe::egui;
use minipdf::{ensure_pdfium, find_cjk_font_path};
use pdfium_render::prelude::*;

use pdf_util::*;
use platform::*;
use search::*;
use theme::*;
use types::*;

const TEXTURE_BUDGET: usize = 192 * 1024 * 1024;
const MAX_LAYER_THREADS: usize = 2;
/// Fixed render width for image export (~200 dpi on a Letter page).
const EXPORT_WIDTH: u32 = 2000;
const THUMB_WIDTH: i32 = 132;
/// Max concurrent background render threads (page + thumbnail combined).
const MAX_RENDER_THREADS: usize = 4;
const HIGHLIGHT_COLORS: &[(&str, (u8, u8, u8))] = &[
    ("Yellow", (255, 255, 0)),
    ("Green", (146, 208, 80)),
    ("Blue", (155, 194, 230)),
    ("Pink", (255, 153, 204)),
];

fn worker_result<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err("Background worker panicked".to_owned()))
}

fn spawn_worker<T: Send + 'static>(
    name: &str,
    tx: mpsc::Sender<T>,
    ctx: Option<egui::Context>,
    message: impl FnOnce() -> T + Send + 'static,
    failed: impl FnOnce(String) -> T,
) {
    let done = tx.clone();
    let wake = ctx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = done.send(message());
            if let Some(ctx) = wake {
                ctx.request_repaint();
            }
        })
    {
        let _ = tx.send(failed(format!("Worker spawn failed: {error}")));
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }
}

const MAX_RECENT: usize = 8;
const MAX_PLACES: usize = 50;

fn state_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("minipdf-state.json");
        }
    }
    std::env::temp_dir().join("minipdf-state.json")
}

fn load_state() -> AppStateFile {
    std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(state: &AppStateFile) {
    if let Ok(b) = serde_json::to_vec(state) {
        let _ = std::fs::write(state_path(), b);
    }
}

/// Generate a unique temporary path alongside the target file to avoid race conditions.
fn unique_temp_pdf_path(path: &Path) -> PathBuf {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_extension(format!("pdf.minipdf-tmp-{pid}-{time}-{count}"))
}

/// Atomically replaces destination file with source file, retrying briefly on Windows if locked.
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last_err = None;
    for attempt in 0..10 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                // Windows may momentarily lock file if background reader was closing
                std::thread::sleep(Duration::from_millis(15 * (attempt + 1)));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

/// Push a path to the front of recent (dedupe, cap). Pure for testability.
fn push_recent(recent: &mut Vec<PathBuf>, path: PathBuf) {
    recent.retain(|p| *p != path);
    recent.insert(0, path);
    recent.truncate(MAX_RECENT);
}

// ---------- app ----------

/// Sidebar content: page thumbnails or search-result list (Preview style).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Thumbs,
    Results,
    Outline,
    Notes,
}

/// State for a print job in progress: the printer DC + page list + progress.
/// Processed one page per frame in `process_print_job`.
struct PrintJob {
    path: PathBuf,
    hdc: isize,
    h_dev_mode: isize,
    h_dev_names: isize,
    doc_name: String,
    pages: Vec<i32>,
    copies: usize,
    cur_idx: usize,
    copy_num: usize,
    started: bool,
    printer_w: i32,
    printer_h: i32,
}

struct MiniPdf {
    pdfium: Option<Pdfium>,
    tabs: Vec<DocTab>,
    active: usize,
    show_sidebar: bool,
    sidebar_mode: SidebarMode,
    status: String,
    focus_search: bool,
    note_draft: Option<NoteDraft>, // pending text-box / sticky-note creation
    fullscreen: bool,
    dark_mode: bool,
    last_title: String,
    // UI icons decoded synchronously at startup (no async bytes-loader involved).
    tex_logo: Option<egui::TextureHandle>,
    // Lucide toolbar icons (ISC): white glyph + alpha, tinted per theme at paint time.
    tool_icons: HashMap<&'static str, egui::TextureHandle>,
    printing_job: Option<PrintJob>,
    // Pending open results from background threads: (path, page_count)
    open_rx: Vec<std::sync::mpsc::Receiver<(PathBuf, Result<i32, String>)>>,
    // Files currently being opened (splash instead of the empty landing page)
    opening: Vec<PathBuf>,
    search_rx: mpsc::Receiver<SearchMessage>,
    search_tx: mpsc::Sender<SearchMessage>,
    search_in_flight: HashSet<(DocVersion, u64)>,
    search_pending: Option<(DocVersion, u64)>,
    render_rx: mpsc::Receiver<RenderMessage>,
    render_tx: mpsc::Sender<RenderMessage>,
    render_in_flight: HashSet<RenderKey>,
    render_failed: HashSet<RenderKey>,
    layer_rx: mpsc::Receiver<LayerMessage>,
    layer_tx: mpsc::Sender<LayerMessage>,
    layer_in_flight: HashSet<LayerKey>,
    layer_failed: HashSet<LayerKey>,
    outline_rx: mpsc::Receiver<OutlineMessage>,
    outline_tx: mpsc::Sender<OutlineMessage>,
    outline_in_flight: HashSet<DocVersion>,
    notes_rx: mpsc::Receiver<NotesMessage>,
    notes_tx: mpsc::Sender<NotesMessage>,
    notes_in_flight: HashSet<DocVersion>,

    recent: Vec<PathBuf>,
    places: HashMap<PathBuf, FilePlace>,
    resume_snapshot: Vec<(PathBuf, i32, f32)>,
    last_resume_save: Instant,
}

impl Default for MiniPdf {
    fn default() -> Self {
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        let (layer_tx, layer_rx) = std::sync::mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        let (outline_tx, outline_rx) = mpsc::channel();
        let (notes_tx, notes_rx) = mpsc::channel();
        let saved = load_state();
        Self {
            pdfium: None,
            tabs: Vec::new(),
            active: 0,
            show_sidebar: false,
            sidebar_mode: SidebarMode::Thumbs,
            status: "Drag a PDF here, or click Open".to_owned(),
            focus_search: false,
            note_draft: None,
            fullscreen: false,
            dark_mode: false,
            last_title: String::new(),
            tex_logo: None,
            tool_icons: HashMap::new(),
            printing_job: None,
            open_rx: Vec::new(),
            opening: Vec::new(),
            search_rx,
            search_tx,
            search_in_flight: HashSet::new(),
            search_pending: None,
            render_rx,
            render_tx,
            render_in_flight: std::collections::HashSet::new(),
            render_failed: HashSet::new(),
            layer_rx,
            layer_tx,
            layer_in_flight: HashSet::new(),
            layer_failed: HashSet::new(),
            outline_rx,
            outline_tx,
            outline_in_flight: HashSet::new(),
            notes_rx,
            notes_tx,
            notes_in_flight: HashSet::new(),
            recent: saved.recent,
            places: saved.places,
            resume_snapshot: Vec::new(),
            last_resume_save: Instant::now(),
        }
    }
}

impl MiniPdf {
    /// Borderless icon button matching Apple Preview's toolbar button style.
    /// `selected` keeps the active wash painted (sidebar toggle, mode switches).
    fn icon_button(
        ui: &mut egui::Ui,
        size: egui::Vec2,
        enabled: bool,
        selected: bool,
        tooltip: &str,
        paint_fn: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let resp = resp.on_hover_text(tooltip);
        if ui.is_rect_visible(rect) {
            let visuals = if !enabled {
                ui.visuals().widgets.noninteractive
            } else if resp.is_pointer_button_down_on() {
                ui.visuals().widgets.active
            } else if resp.hovered() || selected {
                ui.visuals().widgets.hovered
            } else {
                ui.visuals().widgets.inactive
            };
            if selected || resp.hovered() || resp.is_pointer_button_down_on() {
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(4), visuals.bg_fill);
            }
            let color = if enabled {
                visuals.fg_stroke.color
            } else {
                ui.visuals().widgets.noninteractive.fg_stroke.color
            };
            paint_fn(ui.painter(), rect, color);
        }
        if enabled {
            resp
        } else {
            let mut resp = resp;
            resp.sense = egui::Sense::hover();
            resp
        }
    }

    /// Lucide toolbar icon texture id, if loaded.
    fn tool_icon(&self, name: &str) -> Option<egui::TextureId> {
        self.tool_icons.get(name).map(|t| t.id())
    }

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

        self.status = format!(
            "Opening {}…",
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        );

        let (tx, rx) = std::sync::mpsc::channel();
        self.open_rx.push(rx);
        self.opening.push(path.clone());

        let path_clone = path.clone();
        std::thread::Builder::new()
            .name("minipdf-open".into())
            .spawn(move || {
                // Fast path: page count only. Page aspects are filled lazily by
                // the layer worker when each page is first visited, so huge
                // documents don't stall behind a full pre-scan.
                let result: Result<i32, String> = (|| {
                    let pdfium = minipdf::ensure_pdfium()?;
                    let document = pdfium
                        .load_pdf_from_file(&path_clone, None)
                        .map_err(|e| format!("Open failed: {e}"))?;
                    Ok(document.pages().len())
                })();
                let _ = tx.send((path_clone, result));
            })
            .ok();
    }

    /// Poll for completed background open operations. Call once per frame.
    fn poll_open(&mut self, ctx: &egui::Context) {
        if self.open_rx.is_empty() {
            return;
        }
        let mut still_pending = false;
        let mut completed: Vec<(PathBuf, Result<i32, String>)> = Vec::new();
        self.open_rx.retain(|rx| match rx.try_recv() {
            Ok(r) => {
                completed.push(r);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                still_pending = true;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for (path, result) in completed {
            self.opening.retain(|p| *p != path);
            match result {
                Ok(n) => {
                    let title = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    push_recent(&mut self.recent, path.clone());
                    let mut tab = DocTab::new(path, n, HashMap::new());
                    if let Some(place) = self.places.get(&tab.path) {
                        tab.cur = place.page.clamp(0, n - 1).max(0);
                        if place.zoom >= 0.2 && place.zoom <= 4.0 {
                            tab.zoom = place.zoom;
                        }
                        tab.page_box = (tab.cur + 1).to_string();
                        tab.page_box_cur = tab.cur;
                    }
                    self.tabs.push(tab);
                    self.active = self.tabs.len() - 1;
                    self.persist();
                    self.status = format!("Opened {title}, {n} pages");
                }
                Err(e) => self.status = e,
            }
        }
        if still_pending {
            ctx.request_repaint();
        }
    }

    /// Poll for completed background search. Call once per frame.
    fn poll_search(&mut self, ctx: &egui::Context) {
        while let Ok((version, generation, result)) = self.search_rx.try_recv() {
            if !self.search_in_flight.remove(&(version, generation)) {
                continue;
            }
            let Some(idx) = self.tabs.iter().position(|t| {
                t.version == version
                    && t.search_generation == generation
                    && !t.search_cancel.load(Ordering::Relaxed)
            }) else {
                continue;
            };
            match result {
                Ok(out) => {
                    let tab = &mut self.tabs[idx];
                    tab.search_matches = out.matches;
                    tab.search_snippets = out.snippets;
                    tab.search_by_page.clear();
                    for (mi, m) in tab.search_matches.iter().enumerate() {
                        tab.search_by_page.entry(m.page).or_default().push(mi);
                    }
                    tab.search_hits = tab.search_by_page.keys().copied().collect();
                    tab.search_hits.sort_unstable();
                    tab.search_cursor = 0;
                    if idx == self.active {
                        self.show_sidebar = true;
                        self.sidebar_mode = SidebarMode::Results;
                        self.goto_match(idx);
                        self.status = format!(
                            "Search: {}{} matches",
                            self.tabs[idx].search_matches.len(),
                            if out.truncated { "+" } else { "" }
                        );
                    }
                }
                Err(e) => self.status = format!("Search failed: {e}"),
            }
        }
        self.start_pending_search();
        if !self.search_in_flight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Upload completed render results to GPU textures. Call once per frame.
    fn poll_render(&mut self, ctx: &egui::Context) {
        while let Ok((key, result)) = self.render_rx.try_recv() {
            if !self.render_in_flight.remove(&key) {
                continue;
            }
            let (version, page, width) = key;
            let Some(tab) = self.tabs.iter_mut().find(|t| t.version == version) else {
                continue;
            };
            if width != THUMB_WIDTH as u32 && width != render_width(tab.zoom) {
                continue;
            }
            match result {
                Ok((data, w, h))
                    if w > 0
                        && h > 0
                        && w.checked_mul(h).and_then(|n| n.checked_mul(4)) == Some(data.len())
                        && data.len() <= TEXTURE_BUDGET =>
                {
                    let tex = ctx.load_texture(
                        format!("{version:?}-{page}-{width}"),
                        egui::ColorImage::from_rgba_unmultiplied([w, h], &data),
                        egui::TextureOptions::LINEAR,
                    );
                    if width == THUMB_WIDTH as u32 {
                        tab.thumb_tex.insert(page, tex);
                    } else {
                        tab.page_tex.insert((page, width), tex);
                    }
                }
                other => {
                    self.render_failed.insert(key);
                    self.status = match other {
                        Err(e) => e,
                        _ => "Invalid render size".to_owned(),
                    };
                }
            }
        }
        self.evict_textures();
        if !self.render_in_flight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn evict_textures(&mut self) {
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        for (idx, tab) in self.tabs.iter().enumerate() {
            for (&(page, width), tex) in &tab.page_tex {
                let size = tex.size()[0]
                    .saturating_mul(tex.size()[1])
                    .saturating_mul(4);
                bytes = bytes.saturating_add(size);
                entries.push((
                    idx != self.active,
                    (page - tab.cur).abs(),
                    idx,
                    page,
                    width,
                    size,
                ));
            }
            for (&page, tex) in &tab.thumb_tex {
                let size = tex.size()[0]
                    .saturating_mul(tex.size()[1])
                    .saturating_mul(4);
                bytes = bytes.saturating_add(size);
                entries.push((
                    idx != self.active,
                    (page - tab.cur).abs(),
                    idx,
                    page,
                    0,
                    size,
                ));
            }
        }
        entries.sort_unstable();
        for (_, _, idx, page, width, size) in entries.into_iter().rev() {
            if bytes <= TEXTURE_BUDGET {
                break;
            }
            if width == 0 {
                self.tabs[idx].thumb_tex.remove(&page);
            } else {
                self.tabs[idx].page_tex.remove(&(page, width));
            }
            bytes = bytes.saturating_sub(size);
        }
    }

    /// Apply completed background layer extractions to the caches. Call once per frame.
    fn poll_layers(&mut self, ctx: &egui::Context) {
        while let Ok((key, result)) = self.layer_rx.try_recv() {
            if !self.layer_in_flight.remove(&key) {
                continue;
            }
            let Some(tab) = self.tabs.iter_mut().find(|t| t.version == key.0) else {
                continue;
            };
            match result {
                Ok((chars, links, images, aspect)) => {
                    tab.text_cache.insert(key.1, chars);
                    tab.link_cache.insert(key.1, links);
                    tab.image_cache.insert(key.1, images);
                    tab.aspects.insert(key.1, aspect);
                    tab.evict_layers();
                }
                Err(e) => {
                    self.layer_failed.insert(key);
                    self.status = e;
                }
            }
        }
        if !self.layer_in_flight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Fetch the document outline in background on first Contents visit.
    fn ensure_outline(&mut self, tab_idx: usize) {
        let (version, path, missing) = match self.tabs.get(tab_idx) {
            Some(t) => (t.version, t.path.clone(), t.outline.is_none()),
            None => return,
        };
        if !missing || self.outline_in_flight.contains(&version) {
            return;
        }
        self.outline_in_flight.insert(version);
        spawn_worker(
            "minipdf-outline",
            self.outline_tx.clone(),
            None,
            move || (version, fetch_outline(&path)),
            move |e| (version, Err(e)),
        );
    }

    /// Apply completed background outline fetches. Call once per frame.
    fn poll_outline(&mut self, ctx: &egui::Context) {
        while let Ok((version, result)) = self.outline_rx.try_recv() {
            if !self.outline_in_flight.remove(&version) {
                continue;
            }
            let Some(tab) = self.tabs.iter_mut().find(|t| t.version == version) else {
                continue;
            };
            match result {
                Ok(items) => {
                    tab.outline = Some(items);
                }
                Err(e) => {
                    tab.outline = Some(Vec::new());
                    self.status = e;
                }
            }
        }
        if !self.outline_in_flight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Fetch annotations in background on first Notes visit.
    fn ensure_notes(&mut self, tab_idx: usize) {
        let (version, path, missing) = match self.tabs.get(tab_idx) {
            Some(t) => (t.version, t.path.clone(), t.notes.is_none()),
            None => return,
        };
        if !missing || self.notes_in_flight.contains(&version) {
            return;
        }
        self.notes_in_flight.insert(version);
        spawn_worker(
            "minipdf-notes",
            self.notes_tx.clone(),
            None,
            move || (version, fetch_notes(&path)),
            move |e| (version, Err(e)),
        );
    }

    /// Apply completed background annotation fetches. Call once per frame.
    fn poll_notes(&mut self, ctx: &egui::Context) {
        while let Ok((version, result)) = self.notes_rx.try_recv() {
            if !self.notes_in_flight.remove(&version) {
                continue;
            }
            let Some(tab) = self.tabs.iter_mut().find(|t| t.version == version) else {
                continue;
            };
            match result {
                Ok(items) => {
                    tab.notes = Some(items);
                }
                Err(e) => {
                    tab.notes = Some(Vec::new());
                    self.status = e;
                }
            }
        }
        if !self.notes_in_flight.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Delete one annotation from the Notes list, then re-save the file.
    fn delete_note(&mut self, tab_idx: usize, page: i32, index: usize) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            {
                let annots = pg.annotations_mut();
                let target = annots.get(index)?;
                annots.delete_annotation(target)?;
            }
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.notes = None;
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                        tab.version.revision += 1;
                    }
                    self.status = format!("Deleted annotation on page {} (saved)", page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Delete failed: {e}");
            }
        }
    }

    /// Rotate one page 90° left/right, then save (Preview-style instant persistence).
    fn rotate_page(&mut self, tab_idx: usize, page: i32, left: bool) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            let before = pg.rotation()?;
            pg.set_rotation(step_rotation(before, left));
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        // Rotation changes the page's display geometry: drop its
                        // caches so render + text/link layers are re-extracted
                        // in the rotated space. Search hits reference char
                        // indices, so they are cleared as well.
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                        tab.text_cache.remove(&page);
                        tab.link_cache.remove(&page);
                        tab.image_cache.remove(&page);
                        tab.aspects.remove(&page);
                        tab.search_matches.clear();
                        tab.search_snippets.clear();
                        tab.search_by_page.clear();
                        tab.search_hits.clear();
                        tab.notes = None;
                        tab.version.revision += 1;
                    }
                    let dir = if left { "left" } else { "right" };
                    self.status = format!("Rotated page {} {dir} (saved)", page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Rotate failed: {e}");
            }
        }
    }

    /// Delete one page, then save (Preview-style instant persistence).
    /// Page indices shift, so all per-page caches are dropped.
    fn delete_page(&mut self, tab_idx: usize, page: i32) {
        let (path, count) = match self.tabs.get(tab_idx) {
            Some(t) => (t.path.clone(), t.pages),
            None => return,
        };
        if count <= 1 {
            self.status = "Cannot delete the only page".to_owned();
            return;
        }
        if page < 0 || page >= count {
            return;
        }
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            pages.get(page)?.delete()?;
            document.save_to_file(&tmp)?;
            Ok(pages.len())
        });
        match r {
            Ok(new_len) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.pages = new_len;
                        tab.cur = tab.cur.clamp(0, new_len - 1);
                        tab.scroll_target = None;
                        tab.selection = None;
                        tab.page_tex.clear();
                        tab.thumb_tex.clear();
                        tab.text_cache.clear();
                        tab.link_cache.clear();
                        tab.image_cache.clear();
                        tab.aspects.clear();
                        tab.search_matches.clear();
                        tab.search_snippets.clear();
                        tab.search_by_page.clear();
                        tab.search_hits.clear();
                        tab.search_cursor = 0;
                        tab.markup_stack.clear();
                        tab.outline = None;
                        tab.notes = None;
                        tab.version.revision += 1;
                    }
                    self.note_place(tab_idx);
                    self.persist();
                    self.status =
                        format!("Deleted page {} (saved, {new_len} left)", page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Delete failed: {e}");
            }
        }
    }

    /// Append another PDF to the end of this document, then save
    /// (Preview-style: drag a PDF into the sidebar to merge).
    /// pdfium-render 0.9.x only offers whole-document append, so this is
    /// end-of-document only; page indices of existing pages never shift.
    fn append_pdf(&mut self, tab_idx: usize) {
        let dest = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let src = match rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .pick_file()
        {
            Some(p) => p,
            None => return,
        };
        // Merging a file into itself: load the source from bytes so no second
        // handle pins the destination path.
        let same = std::fs::canonicalize(&src)
            .ok()
            .zip(std::fs::canonicalize(&dest).ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        let src_bytes = if same {
            match std::fs::read(&src) {
                Ok(b) => Some(b),
                Err(e) => {
                    self.status = format!("Append failed: {e}");
                    return;
                }
            }
        } else {
            None
        };
        let tmp = unique_temp_pdf_path(&dest);
        let src_name = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let mut document = pdfium.load_pdf_from_file(p, None)?;
            let before = document.pages().len();
            if let Some(bytes) = &src_bytes {
                let other = pdfium.load_pdf_from_byte_vec(bytes.clone(), None)?;
                document.pages_mut().append(&other)?;
            } else {
                let other = pdfium.load_pdf_from_file(&src, None)?;
                document.pages_mut().append(&other)?;
            }
            let after = document.pages().len();
            document.save_to_file(&tmp)?;
            Ok((before, after))
        });
        match r {
            Ok((before, after)) => match atomic_replace_file(&tmp, &dest) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.pages = after;
                        // Existing pages are untouched: their caches stay
                        // valid. Outline/bookmarks may have merged: refetch.
                        tab.outline = None;
                        tab.notes = None;
                        tab.version.revision += 1;
                    }
                    self.note_place(tab_idx);
                    self.persist();
                    self.status = format!(
                        "Appended {} pages from {src_name} (saved, {after} total)",
                        after - before
                    );
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Append failed: {e}");
            }
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let title = self.tabs[idx].title();
        let version = self.tabs[idx].version;
        self.tabs[idx].search_cancel.store(true, Ordering::Relaxed);
        self.note_place(idx);
        self.tabs.remove(idx);
        self.persist();
        self.render_in_flight.retain(|(v, _, _)| *v != version);
        self.render_failed.retain(|(v, _, _)| *v != version);
        self.layer_in_flight.retain(|(v, _)| *v != version);
        self.layer_failed.retain(|(v, _)| *v != version);
        self.outline_in_flight.retain(|v| *v != version);
        self.notes_in_flight.retain(|v| *v != version);
        self.search_in_flight.retain(|(v, _)| *v != version);
        if let Some((v, _)) = &self.search_pending {
            if *v == version {
                self.search_pending = None;
            }
        }
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
        let e = self
            .pdfium
            .as_ref()
            .ok_or_else(|| "Engine not ready".to_owned())?;
        let t = self
            .tabs
            .get(tab_idx)
            .ok_or_else(|| "No document open".to_owned())?;
        f(e, &t.path).map_err(|e| format!("{e}"))
    }

    /// Render one page to a texture. Callers gate on visibility to avoid UI stalls.
    /// Schedule background render of a page. Returns cached texture if already ready,
    /// or None (shows Loading placeholder) while the render is in flight.
    /// Preview strategy: never block the UI thread on rendering.
    fn render_tex(
        &mut self,
        tab_idx: usize,
        ctx: &egui::Context,
        page: i32,
        width_px: i32,
        cache: bool,
    ) -> Option<egui::TextureHandle> {
        let width = width_px.clamp(1, MAX_RENDER_WIDTH) as u32;
        if cache {
            if let Some(t) = self.tabs.get(tab_idx)?.page_tex.get(&(page, width)) {
                return Some(t.clone());
            }
        } else if let Some(t) = self.tabs.get(tab_idx)?.thumb_tex.get(&page) {
            return Some(t.clone());
        }
        let version = self.tabs.get(tab_idx)?.version;
        let key: RenderKey = (version, page, width);
        if self.render_in_flight.contains(&key) || self.render_failed.contains(&key) {
            return None;
        }
        if self.render_in_flight.len() >= MAX_RENDER_THREADS {
            return None;
        }
        self.render_in_flight.insert(key);
        let path = self.tabs.get(tab_idx)?.path.clone();
        let tx = self.render_tx.clone();
        spawn_worker(
            "minipdf-render",
            tx,
            Some(ctx.clone()),
            move || {
                (
                    key,
                    worker_result(|| {
                        let pdfium = minipdf::ensure_pdfium().map_err(|e| e.to_string())?;
                        let document = pdfium
                            .load_pdf_from_file(&path, None)
                            .map_err(|e| e.to_string())?;
                        let pg = document.pages().get(page).map_err(|e| e.to_string())?;
                        let bmp = pg
                            .render_with_config(
                                &PdfRenderConfig::new()
                                    .set_target_width(width as i32)
                                    .set_maximum_height(width as i32 * 3),
                            )
                            .map_err(|e| e.to_string())?;
                        let dynamic = bmp.as_image().map_err(|e| e.to_string())?;
                        let rgba = dynamic.to_rgba8();
                        Ok((
                            rgba.as_raw().to_vec(),
                            rgba.width() as usize,
                            rgba.height() as usize,
                        ))
                    }),
                )
            },
            move |e| (key, Err(e)),
        );
        None
    }

    /// Schedule background extraction of text/link/image layers for a page.
    /// Results are delivered via poll_layers(); returns immediately without blocking.
    fn ensure_page_layers(&mut self, tab_idx: usize, page: i32) {
        let need_text = self
            .tabs
            .get(tab_idx)
            .map(|t| !t.text_cache.contains_key(&page))
            .unwrap_or(false);
        let need_links = self
            .tabs
            .get(tab_idx)
            .map(|t| !t.link_cache.contains_key(&page))
            .unwrap_or(false);
        let need_images = self
            .tabs
            .get(tab_idx)
            .map(|t| !t.image_cache.contains_key(&page))
            .unwrap_or(false);
        if !(need_text || need_links || need_images) {
            return;
        }
        let version = match self.tabs.get(tab_idx) {
            Some(t) => t.version,
            None => return,
        };
        let key: LayerKey = (version, page);
        if self.layer_in_flight.contains(&key) || self.layer_failed.contains(&key) {
            return;
        }
        if self.layer_in_flight.len() >= MAX_LAYER_THREADS {
            return;
        }
        self.layer_in_flight.insert(key);
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => {
                self.layer_in_flight.remove(&key);
                return;
            }
        };
        let tx = self.layer_tx.clone();
        spawn_worker(
            "minipdf-layers",
            tx,
            None,
            move || {
                let result: Result<LayerData, String> = worker_result(|| {
                    let pdfium = minipdf::ensure_pdfium().map_err(|e| e.to_string())?;
                    let document = pdfium
                        .load_pdf_from_file(&path, None)
                        .map_err(|e| e.to_string())?;
                    let pg = document.pages().get(page).map_err(|e| e.to_string())?;

                    let chars: Vec<CharInfo> = if need_text {
                        Self::extract_chars(&pg).map_err(|e| e.to_string())?
                    } else {
                        Vec::new()
                    };
                    let links: Vec<PageLink> = if need_links {
                        Self::extract_links(&pg).map_err(|e| e.to_string())?
                    } else {
                        Vec::new()
                    };
                    let images: Vec<PdfImage> = if need_images {
                        Self::extract_images(&pg).map_err(|e| e.to_string())?
                    } else {
                        Vec::new()
                    };
                    Ok((chars, links, images, (pg.width().value, pg.height().value)))
                });
                (key, result)
            },
            move |e| (key, Err(e)),
        );
    }

    fn extract_chars(pg: &PdfPage<'_>) -> Result<Vec<CharInfo>, PdfiumError> {
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
    }

    fn extract_links(pg: &PdfPage<'_>) -> Result<Vec<PageLink>, PdfiumError> {
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
                        LinkTarget::Url(format!("https://{u}"))
                    } else {
                        LinkTarget::Url(u)
                    }
                });
            if target.is_none() {
                let mut page_idx: Option<i32> = None;
                if let Some(act) = link.action() {
                    if let Some(local) = act.as_local_destination_action() {
                        if let Ok(dest) = local.destination() {
                            if let Ok(p) = dest.page_index() {
                                page_idx = Some(p);
                            }
                        }
                    }
                }
                if page_idx.is_none() {
                    if let Some(dest) = link.destination() {
                        if let Ok(p) = dest.page_index() {
                            page_idx = Some(p);
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
    }

    fn extract_images(pg: &PdfPage<'_>) -> Result<Vec<PdfImage>, PdfiumError> {
        let objs = pg.objects();
        let mut out = Vec::new();
        for idx in 0..objs.len() {
            let o = match objs.get(idx) {
                Ok(o) => o,
                Err(_) => continue,
            };
            if o.object_type() != PdfPageObjectType::Image {
                continue;
            }
            let b = match o.bounds() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let r = b.to_rect();
            out.push(PdfImage {
                obj: idx,
                left: r.left().value,
                bottom: r.bottom().value,
                right: r.right().value,
                top: r.top().value,
            });
        }
        Ok(out)
    }

    /// Load per-char text + bounds for a page (lazy, cached per tab).
    /// Load per-char text + bounds for a page (lazy, cached per tab).
    /// Delegates to ensure_page_layers which opens the doc once for text+links+images.
    fn ensure_text(&mut self, tab_idx: usize, page: i32) {
        self.ensure_page_layers(tab_idx, page);
    }

    /// Load clickable links for a page (lazy, cached per tab).

    /// Extract one embedded image as RGBA pixel data (re-opens the document).
    /// `obj` is the index into `page.objects()`. Returns (rgba_bytes, w, h).
    fn extract_image(
        &mut self,
        tab_idx: usize,
        page: i32,
        obj: usize,
    ) -> Result<(Vec<u8>, usize, usize), ()> {
        let res = self.with_doc(tab_idx, |pdfium, path| {
            let document = pdfium.load_pdf_from_file(path, None)?;
            let pg = document.pages().get(page)?;
            let o = pg.objects().get(obj)?;
            let Some(img_obj) = o.as_image_object() else {
                return Ok((Vec::new(), 0usize, 0usize));
            };
            let img = img_obj.get_raw_image()?;
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            Ok((rgba.into_raw(), w, h))
        });
        match res {
            Ok((data, w, h)) => {
                if w == 0 || h == 0 || data.len() != w * h * 4 {
                    self.status = "Image has no pixel data".to_owned();
                    Err(())
                } else {
                    Ok((data, w, h))
                }
            }
            Err(e) => {
                self.status = format!("Image extract failed: {e}");
                Err(())
            }
        }
    }

    /// Copy an embedded image to the system clipboard as PNG (Windows CF_DIB via arboard).
    fn copy_image(&mut self, tab_idx: usize, page: i32, img: &PdfImage) {
        if let Ok((data, w, h)) = self.extract_image(tab_idx, page, img.obj) {
            match arboard::Clipboard::new() {
                Ok(mut cb) => {
                    let imd = arboard::ImageData {
                        width: w,
                        height: h,
                        bytes: std::borrow::Cow::Owned(data),
                    };
                    match cb.set_image(imd) {
                        Ok(()) => {
                            self.status =
                                format!("Copied image ({}x{} px) from page {}", w, h, page + 1)
                        }
                        Err(e) => self.status = format!("Clipboard write failed: {e}"),
                    }
                }
                Err(e) => self.status = format!("Clipboard unavailable: {e}"),
            }
        } // status already set by extract_image on Err
    }

    /// Save an embedded image to a user-chosen file (PNG).
    fn save_image(&mut self, tab_idx: usize, page: i32, img: &PdfImage) {
        let (data, w, h) = match self.extract_image(tab_idx, page, img.obj) {
            Ok(v) => v,
            Err(_) => return, // status already set
        };
        let name = self
            .tabs
            .get(tab_idx)
            .map(|t| {
                t.path
                    .file_stem()
                    .map(|s| format!("{}-p{}-img", s.to_string_lossy(), page + 1))
                    .unwrap_or_else(|| format!("minipdf-p{}-img", page + 1))
            })
            .unwrap_or_else(|| format!("minipdf-p{}-img", page + 1));
        let path = rfd::FileDialog::new()
            .set_file_name(format!("{name}.png"))
            .add_filter("PNG image", &["png"])
            .save_file();
        let Some(path) = path else {
            return;
        };
        match image::save_buffer(
            &path,
            &data,
            w as u32,
            h as u32,
            image::ExtendedColorType::Rgba8,
        ) {
            Ok(()) => self.status = format!("Saved image to {}", path.display()),
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    /// Render one page to RGBA bytes synchronously (Preview: export as image).
    /// Same pipeline as the background render worker, but blocking: used for
    /// single-page export where the wait is under a second.
    fn render_page_rgba(
        &self,
        tab_idx: usize,
        page: i32,
        width: u32,
    ) -> Result<(Vec<u8>, u32, u32), String> {
        let pdfium = self
            .pdfium
            .as_ref()
            .ok_or_else(|| "Engine not ready".to_owned())?;
        let tab = self
            .tabs
            .get(tab_idx)
            .ok_or_else(|| "No document open".to_owned())?;
        let document = pdfium
            .load_pdf_from_file(&tab.path, None)
            .map_err(|e| e.to_string())?;
        let pg = document.pages().get(page).map_err(|e| e.to_string())?;
        let bmp = pg
            .render_with_config(
                &PdfRenderConfig::new()
                    .set_target_width(width as i32)
                    .set_maximum_height(width as i32 * 3),
            )
            .map_err(|e| e.to_string())?;
        let rgba = bmp.as_image().map_err(|e| e.to_string())?.to_rgba8();
        Ok((rgba.as_raw().to_vec(), rgba.width(), rgba.height()))
    }

    /// Write rendered RGBA bytes to a file as PNG or JPEG (quality 90).
    fn write_export_image(
        path: &std::path::Path,
        data: &[u8],
        w: u32,
        h: u32,
        jpeg: bool,
    ) -> Result<(), String> {
        if jpeg {
            let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
            enc.encode(data, w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        } else {
            image::save_buffer(path, data, w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())
        }
    }

    /// Export one page as PNG/JPEG via a save dialog.
    fn export_page_image(&mut self, tab_idx: usize, page: i32, jpeg: bool) {
        let (stem, ext, filter) = match self.tabs.get(tab_idx) {
            Some(t) => (
                t.path
                    .file_stem()
                    .map(|s| format!("{}-p{}", s.to_string_lossy(), page + 1))
                    .unwrap_or_else(|| format!("minipdf-p{}", page + 1)),
                if jpeg { "jpg" } else { "png" },
                if jpeg {
                    ("JPEG image", vec!["jpg", "jpeg"])
                } else {
                    ("PNG image", vec!["png"])
                },
            ),
            None => return,
        };
        let dest = match rfd::FileDialog::new()
            .set_file_name(format!("{stem}.{ext}"))
            .add_filter(filter.0, &filter.1)
            .save_file()
        {
            Some(p) => p,
            None => return,
        };
        match self.render_page_rgba(tab_idx, page, EXPORT_WIDTH) {
            Ok((data, w, h)) => match Self::write_export_image(&dest, &data, w, h, jpeg) {
                Ok(()) => {
                    self.status = format!(
                        "Exported page {} to {}",
                        page + 1,
                        dest.file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    )
                }
                Err(e) => self.status = format!("Export failed: {e}"),
            },
            Err(e) => self.status = format!("Export failed: {e}"),
        }
    }

    /// Export every page as PNG/JPEG into a chosen folder.
    fn export_all_images(&mut self, tab_idx: usize, jpeg: bool) {
        let stem = match self.tabs.get(tab_idx) {
            Some(t) => t
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "minipdf".to_owned()),
            None => return,
        };
        let pages = self.tabs.get(tab_idx).map(|t| t.pages).unwrap_or(0);
        let dir = match rfd::FileDialog::new().pick_folder() {
            Some(d) => d,
            None => return,
        };
        let ext = if jpeg { "jpg" } else { "png" };
        self.status = format!("Exporting {pages} pages…");
        let mut done = 0;
        for page in 0..pages {
            let dest = dir.join(format!("{stem}-p{:03}.{ext}", page + 1));
            match self.render_page_rgba(tab_idx, page, EXPORT_WIDTH) {
                Ok((data, w, h)) => {
                    if Self::write_export_image(&dest, &data, w, h, jpeg).is_err() {
                        self.status =
                            format!("Export failed on page {} (saved {done}/{pages})", page + 1);
                        return;
                    }
                    done += 1;
                }
                Err(e) => {
                    self.status =
                        format!("Export failed on page {}: {e} (saved {done}/{pages})", page + 1);
                    return;
                }
            }
        }
        self.status = format!("Exported {done} pages to {}", dir.display());
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
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match atomic_replace_file(&tmp, &path) {
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
                    tab.version.revision += 1;
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
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            let lines = merge_lines(&rects);
            let count = lines.len();
            {
                let annots = pg.annotations_mut();
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
            Ok((before_len, count))
        });
        match r {
            Ok((before_len, count)) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if count == 0 {
                        self.status = "Nothing to mark up".to_owned();
                        return;
                    }
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                        tab.version.revision += 1;
                        tab.notes = None;
                        tab.markup_stack.push(MarkupUndo {
                            page,
                            before_len,
                            count,
                        });
                    }
                    self.status = format!("{} added on page {} (saved)", kind.name(), page + 1);
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
        let undo = match self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.markup_stack.last().copied())
        {
            Some(u) => u,
            None => {
                self.status = "Nothing to undo".to_owned();
                return;
            }
        };
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        let tmp = unique_temp_pdf_path(&path);
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(undo.page)?;
            {
                let annots = pg.annotations_mut();
                if !undo.valid(annots.len()) {
                    return Err(PdfiumError::PageAnnotationIndexOutOfBounds);
                }
                for _ in 0..undo.count {
                    if let Ok(last) = annots.last() {
                        annots.delete_annotation(last)?;
                    }
                }
            }
            document.save_to_file(&tmp)?;
            Ok(())
        });
        match r {
            Ok(()) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.markup_stack.pop();
                        tab.page_tex.retain(|(p, _), _| *p != undo.page);
                        tab.thumb_tex.remove(&undo.page);
                        tab.version.revision += 1;
                        tab.notes = None;
                    }
                    self.status = format!("Undid markup on page {} (saved)", undo.page + 1);
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

    /// Add a sticky note (Text) at a PDF position,
    /// then save the file (Preview-style instant persistence).
    fn add_note(&mut self, tab_idx: usize, page: i32, x: f32, y: f32, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status = "Note text is empty".to_owned();
            return;
        }
        let (path, pw, ph) = match self.tabs.get(tab_idx) {
            Some(t) => (
                t.path.clone(),
                t.aspects.get(&page).map(|a| a.0).unwrap_or(595.0),
                t.aspects.get(&page).map(|a| a.1).unwrap_or(842.0),
            ),
            None => return,
        };
        let tmp = unique_temp_pdf_path(&path);
        let text_owned = text.to_owned();
        let r = self.with_doc(tab_idx, |pdfium, p| {
            let document = pdfium.load_pdf_from_file(p, None)?;
            let pages = document.pages();
            let mut pg = pages.get(page)?;
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            // Visible marker: a solid yellow square. (This Pdfium build draws
            // broken appearances for API-created markup quads and only a tiny
            // fixed-size glyph for Text icons, so squares are the one shape
            // guaranteed to paint.) The note text rides in Contents and is
            // shown in the Notes sidebar.
            let (s, hx) = (24.0f32, 12.0f32);
            let l = (x - hx).clamp(0.0, (pw - s).max(0.0));
            let b = (y - hx).clamp(0.0, (ph - s).max(0.0));
            let mut a = annots.create_square_annotation()?;
            a.set_bounds(rect_pt(l, b, (l + s).min(pw), (b + s).min(ph)))?;
            let fill = PdfColor::new(255, 235, 0, 255);
            let edge = PdfColor::new(120, 100, 0, 255);
            a.set_fill_color(fill)?;
            a.set_stroke_color(edge)?;
            a.set_contents(&text_owned)?;
            document.save_to_file(&tmp)?;
            Ok(before_len)
        });
        match r {
            Ok(before_len) => match atomic_replace_file(&tmp, &path) {
                Ok(()) => {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.page_tex.retain(|(p, _), _| *p != page);
                        tab.thumb_tex.remove(&page);
                        tab.version.revision += 1;
                        tab.notes = None;
                        tab.markup_stack.push(MarkupUndo {
                            page,
                            before_len,
                            count: 1,
                        });
                    }
                    self.status = format!("Note added on page {} (saved)", page + 1);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.status = format!("Save failed: {e}");
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.status = format!("Note failed: {e}");
            }
        }
    }

    /// Small modal-ish dialog collecting the text for a pending note draft.
    fn note_dialog(&mut self, ui: &mut egui::Ui) {
        if self.note_draft.is_none() {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        if let Some(d) = self.note_draft.as_mut() {
            egui::Window::new("Add note")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Page {}", d.page + 1));
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut d.text)
                            .desired_rows(4)
                            .desired_width(300.0)
                            .hint_text("Type note text…"),
                    );
                    if d.fresh {
                        resp.request_focus();
                        d.fresh = false;
                    }
                    if resp.has_focus()
                        && ui.input(|i| {
                            i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl
                        })
                    {
                        commit = true;
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .button("Add")
                            .on_hover_text("Add the note (Ctrl+Enter)")
                            .clicked()
                        {
                            commit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
        }
        if cancel {
            self.note_draft = None;
            self.status = "Note cancelled".to_owned();
        } else if commit {
            if let Some(d) = self.note_draft.take() {
                if d.text.trim().is_empty() {
                    self.status = "Note text is empty".to_owned();
                    self.note_draft = Some(NoteDraft { fresh: false, ..d });
                } else {
                    self.add_note(d.tab, d.page, d.x, d.y, &d.text);
                }
            }
        }
    }

    fn run_search(&mut self, tab_idx: usize) {
        for tab in &self.tabs {
            tab.search_cancel.store(true, Ordering::Relaxed);
        }
        self.search_pending = None;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        tab.search_generation += 1;
        tab.search_cancel = Arc::new(AtomicBool::new(false));
        tab.search_matches.clear();
        tab.search_snippets.clear();
        tab.search_by_page.clear();
        tab.search_hits.clear();
        tab.search_cursor = 0;
        tab.search_query = normalized_query(&tab.search_text);
        if tab.search_query.is_empty() {
            return;
        }
        self.search_pending = Some((tab.version, tab.search_generation));
        self.status = "Searching…".to_owned();
        self.start_pending_search();
    }

    fn start_pending_search(&mut self) {
        if !self.search_in_flight.is_empty() {
            return;
        }
        let Some((version, generation)) = self.search_pending.take() else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| {
            t.version == version
                && t.search_generation == generation
                && !t.search_cancel.load(Ordering::Relaxed)
        }) else {
            return;
        };
        let path = tab.path.clone();
        let query: Vec<char> = tab.search_query.chars().collect();
        let cancel = tab.search_cancel.clone();
        self.search_in_flight.insert((version, generation));
        spawn_worker(
            "minipdf-search",
            self.search_tx.clone(),
            None,
            move || {
                (
                    version,
                    generation,
                    worker_result(|| {
                        let pdfium = ensure_pdfium()?;
                        let document = pdfium
                            .load_pdf_from_file(&path, None)
                            .map_err(|e| e.to_string())?;
                        let pages = document.pages();
                        let mut out = SearchOutput::default();
                        for page in 0..pages.len() {
                            if cancel.load(Ordering::Relaxed) {
                                return Err("Search cancelled".to_owned());
                            }
                            let pg = pages.get(page).map_err(|e| e.to_string())?;
                            let chars = Self::extract_chars(&pg).map_err(|e| e.to_string())?;
                            search_page(&mut out, page, &chars, &query, &cancel);
                            if out.matches.len() >= 5000 {
                                out.truncated = true;
                                break;
                            }
                        }
                        Ok(out)
                    }),
                )
            },
            |e| (version, generation, Err(e)),
        );
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
        let total = self
            .tabs
            .get(tab_idx)
            .map(|t| t.search_matches.len())
            .unwrap_or(0);
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
            tab.search_cancel.store(true, Ordering::Relaxed);
            tab.search_matches.clear();
            tab.search_snippets.clear();
            tab.search_by_page.clear();
            tab.search_hits.clear();
            tab.search_cursor = 0;
            tab.search_query.clear();
        }
        self.sidebar_mode = SidebarMode::Thumbs;
        self.status = "Search cleared".to_owned();
    }

    /// Open a file with its default app (used for URL link clicks).
    fn os_open(path: &Path) -> Result<(), String> {
        shell_verb(path, "open")
    }

    /// Show the standard Windows Print dialog, then set up a print job that
    /// is processed one page per frame in `process_print_job`.
    #[cfg(windows)]
    fn start_print(&mut self, tab_idx: usize) {
        if self.printing_job.is_some() {
            self.status = "Already printing…".to_owned();
            return;
        }
        let (path, total_pages) = match self.tabs.get(tab_idx) {
            Some(t) => (t.path.clone(), t.pages),
            None => return,
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "PDF".to_owned());

        let total = total_pages.max(1);
        log_line(&format!("start_print: total_pages={total}"));
        let result = show_print_dialog(total);
        log_line(&format!(
            "start_print: dialog result = {}",
            match &result {
                Ok(r) => format!(
                    "Ok hdc={} pages={} copies={}",
                    r.hdc,
                    r.pages.len(),
                    r.copies
                ),
                Err(e) => format!("Err({e})"),
            }
        ));

        match result {
            Ok(info) => {
                let n = info.pages.len();
                let copies = info.copies;
                self.printing_job = Some(PrintJob {
                    path: path.clone(),
                    hdc: info.hdc,
                    h_dev_mode: info.h_dev_mode,
                    h_dev_names: info.h_dev_names,
                    doc_name: name,
                    pages: info.pages,
                    copies,
                    cur_idx: 0,
                    copy_num: 0,
                    started: false,
                    printer_w: info.printer_w,
                    printer_h: info.printer_h,
                });
                self.status = format!("Printing 1/{} …", n * copies);
            }
            Err(e) => self.status = e,
        }
    }

    #[cfg(not(windows))]
    fn start_print(&mut self, _tab_idx: usize) {
        self.status = "Printing is not supported on this platform".to_owned();
    }

    /// Process one page of the active print job per frame. Renders the page
    /// to a bitmap via Pdfium, then sends it to the printer via GDI
    /// (StartPage → StretchDIBits → EndPage).
    #[cfg(windows)]
    fn process_print_job(&mut self) {
        let mut job = match self.printing_job.take() {
            Some(j) => j,
            None => return,
        };
        let total = job.pages.len() * job.copies;

        if !job.started {
            let doc_w: Vec<u16> = job
                .doc_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let di = DOCINFOW {
                cb_size: std::mem::size_of::<DOCINFOW>() as i32,
                doc_name: doc_w.as_ptr(),
                output: std::ptr::null(),
                datatype: std::ptr::null(),
                fw_type: 0,
            };
            let r = unsafe { StartDocW(job.hdc, &di) };
            log_line(&format!("process_print_job: StartDoc returned {r}"));
            if r <= 0 {
                unsafe {
                    DeleteDC(job.hdc);
                    if job.h_dev_mode != 0 {
                        GlobalFree(job.h_dev_mode);
                    }
                    if job.h_dev_names != 0 {
                        GlobalFree(job.h_dev_names);
                    }
                }
                self.status = format!("Print failed: StartDoc error {r}");
                return;
            }
            job.started = true;
        }

        let page = job.pages[job.cur_idx];
        let render_w = job.printer_w.clamp(200, 4000);

        let engine = match self.pdfium.as_ref() {
            Some(e) => e,
            None => {
                self.status = "Engine not ready".to_owned();
                return;
            }
        };
        let img_result: Result<image::DynamicImage, String> = (|| {
            let document = engine
                .load_pdf_from_file(&job.path, None)
                .map_err(|e| e.to_string())?;
            let pg = document.pages().get(page).map_err(|e| e.to_string())?;
            let bmp = pg
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(render_w)
                        .set_maximum_height(render_w * 3),
                )
                .map_err(|e| e.to_string())?;
            bmp.as_image().map_err(|e| e.to_string())
        })();

        match img_result {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (bw, bh) = (rgba.width() as i32, rgba.height() as i32);
                let mut data = rgba.into_raw();
                // GDI 32-bit BI_RGB expects BGR order; swap R↔B.
                for chunk in data.chunks_exact_mut(4) {
                    chunk.swap(0, 2);
                }

                let pw = job.printer_w as f32;
                let ph = job.printer_h as f32;
                let scale = (pw / bw as f32).min(ph / bh as f32);
                let dest_w = (bw as f32 * scale) as i32;
                let dest_h = (bh as f32 * scale) as i32;
                let dest_x = ((pw - dest_w as f32) * 0.5) as i32;
                let dest_y = ((ph - dest_h as f32) * 0.5) as i32;

                let bmi = BITMAPINFO {
                    header: BITMAPINFOHEADER {
                        bi_size: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        bi_width: bw,
                        bi_height: -bh, // negative = top-down (matches pdfium output)
                        bi_planes: 1,
                        bi_bit_count: 32,
                        bi_compression: 0, // BI_RGB
                        bi_size_image: 0,
                        bi_x_pels_per_meter: 0,
                        bi_y_pels_per_meter: 0,
                        bi_clr_used: 0,
                        bi_clr_important: 0,
                    },
                };

                unsafe {
                    StartPage(job.hdc);
                    StretchDIBits(
                        job.hdc,
                        dest_x,
                        dest_y,
                        dest_w,
                        dest_h,
                        0,
                        0,
                        bw,
                        bh,
                        data.as_ptr(),
                        &bmi,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                    EndPage(job.hdc);
                }

                job.cur_idx += 1;
                if job.cur_idx >= job.pages.len() {
                    job.cur_idx = 0;
                    job.copy_num += 1;
                    if job.copy_num >= job.copies {
                        unsafe {
                            EndDoc(job.hdc);
                            DeleteDC(job.hdc);
                            if job.h_dev_mode != 0 {
                                GlobalFree(job.h_dev_mode);
                            }
                            if job.h_dev_names != 0 {
                                GlobalFree(job.h_dev_names);
                            }
                        }
                        self.status =
                            format!("Printed {} pages × {} copies", job.pages.len(), job.copies);
                        return;
                    }
                }
                let done = job.copy_num * job.pages.len() + job.cur_idx + 1;
                self.status = format!("Printing {}/{} …", done, total);
                self.printing_job = Some(job);
            }
            Err(e) => {
                unsafe {
                    if job.started {
                        EndDoc(job.hdc);
                    }
                    DeleteDC(job.hdc);
                    if job.h_dev_mode != 0 {
                        GlobalFree(job.h_dev_mode);
                    }
                    if job.h_dev_names != 0 {
                        GlobalFree(job.h_dev_names);
                    }
                }
                self.status = format!("Print aborted: {e}");
            }
        }
    }

    #[cfg(not(windows))]
    fn process_print_job(&mut self) {}

    /// Open-with dialog: let the user pick another app for this PDF.
    fn open_with_other(&mut self, tab_idx: usize) {
        let path = match self.tabs.get(tab_idx) {
            Some(t) => t.path.clone(),
            None => return,
        };
        // On Win10/11 the "How do you want to open this file?" picker is a UWP-style
        // dialog. Raw ShellExecuteW("openas") returns SE_ERR_NOASSOC (31) for desktop
        // callers on some builds; the Shell.Application COM object reaches it fine.
        match shell_openas_com(&path) {
            Ok(()) => self.status = "Choose an app to open the PDF".to_owned(),
            Err(e) => {
                // last resort: plain open with the default app
                self.status = match shell_verb(&path, "open") {
                    Ok(()) => "Opened with default app".to_owned(),
                    Err(e2) => format!("Open failed: {e}; retry: {e2}"),
                };
            }
        }
    }

    /// Switch between light and dark theme, matching Apple Preview's color palette.
    fn apply_theme(ctx: &egui::Context, dark: bool) {
        let mut visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        let flat_radius = egui::CornerRadius::same(4);

        if dark {
            // Apple Preview dark mode palette (macOS Sonoma/Ventura dark)
            // Toolbar/sidebar: NSColor.windowBackgroundColor ~= #1E1E1E
            // Button glyphs: #EBEBF5 (slightly off-white, not pure white)
            let glyph = egui::Stroke::new(1.0, egui::Color32::from_gray(210));
            let glyph_hover = egui::Stroke::new(1.0, egui::Color32::from_gray(235));

            visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
            visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.inactive.corner_radius = flat_radius;
            visuals.widgets.inactive.fg_stroke = glyph;

            visuals.widgets.hovered.bg_fill =
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18);
            visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.hovered.corner_radius = flat_radius;
            visuals.widgets.hovered.fg_stroke = glyph_hover;

            visuals.widgets.active.bg_fill =
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36);
            visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.active.corner_radius = flat_radius;
            visuals.widgets.active.fg_stroke = glyph_hover;

            // Panel/window background: #1E1E1E
            visuals.panel_fill = egui::Color32::from_gray(30);
            // Slightly lighter for widgets on dark
            visuals.widgets.noninteractive.bg_fill = egui::Color32::from_gray(38);
            visuals.widgets.open.bg_fill = egui::Color32::from_gray(45);

            // Text input background: #2C2C2E (Apple's dark text field)
            visuals.extreme_bg_color = egui::Color32::from_gray(44);

            // Selection: iOS 17/macOS accent blue, 55% opacity on dark.
            // stroke doubles as the selected-item text color (egui button_style),
            // so it must stay opaque white — NONE renders selected labels black.
            visuals.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(10, 132, 255, 140);
            visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(235));

            // NOTE: noninteractive.fg_stroke is also the color of ALL plain
            // labels and TextEdit text (egui text_color()), so it must stay a
            // readable light gray in dark mode — never a dark separator tone.
            visuals.widgets.noninteractive.fg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_gray(205));
        } else {
            // Light theme: Catalina/Preview palette (unchanged from original)
            let glyph = egui::Stroke::new(1.0, egui::Color32::from_gray(77));
            let glyph_hover = egui::Stroke::new(1.0, egui::Color32::from_gray(38));

            visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
            visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.inactive.corner_radius = flat_radius;
            visuals.widgets.inactive.fg_stroke = glyph;

            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 15);
            visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.hovered.corner_radius = flat_radius;
            visuals.widgets.hovered.fg_stroke = glyph_hover;

            visuals.widgets.active.bg_fill = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 31);
            visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
            visuals.widgets.active.corner_radius = flat_radius;
            visuals.widgets.active.fg_stroke = glyph_hover;
        }

        ctx.set_visuals(visuals);
    }

    fn toggle_dark_mode(&mut self, ctx: &egui::Context) {
        self.dark_mode = !self.dark_mode;
        Self::apply_theme(ctx, self.dark_mode);
        self.status = if self.dark_mode {
            "Dark mode".to_owned()
        } else {
            "Light mode".to_owned()
        };
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        self.status = if self.fullscreen {
            "Fullscreen (F11 / Ctrl+L or Esc to exit)".to_owned()
        } else {
            "Windowed".to_owned()
        };
    }

    fn goto(&mut self, tab_idx: usize, p: i32) {
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
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
                        if ui
                            .small_button("×")
                            .on_hover_text("Close tab (Ctrl+W)")
                            .clicked()
                        {
                            close = Some(idx);
                        }
                        ui.separator();
                    }
                    if let Some(c) = close {
                        self.close_tab(c);
                    } else if let Some(s) = switch_to {
                        self.active = s;
                    }
                    if ui
                        .button("+")
                        .on_hover_text("Open PDFs in new tabs")
                        .clicked()
                    {
                        self.pick_files();
                    }
                });
            });
        });
    }

    /// File menu contents: Open / Save / Save as.
    fn file_menu(&mut self, ui: &mut egui::Ui, tab_idx: usize) {
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
        if ui
            .button("Append PDF…")
            .on_hover_text("Merge another PDF at the end (saves file)")
            .clicked()
        {
            self.append_pdf(tab_idx);
            ui.close();
        }
        ui.separator();
        ui.menu_button("Export as image", |ui| {
            let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
            if ui.button("This page (PNG)…").clicked() {
                self.export_page_image(tab_idx, cur, false);
                ui.close();
            }
            if ui.button("This page (JPEG)…").clicked() {
                self.export_page_image(tab_idx, cur, true);
                ui.close();
            }
            ui.separator();
            if ui.button("All pages (PNG)…").clicked() {
                self.export_all_images(tab_idx, false);
                ui.close();
            }
            if ui.button("All pages (JPEG)…").clicked() {
                self.export_all_images(tab_idx, true);
                ui.close();
            }
        });
        ui.separator();
        ui.menu_button("Recent", |ui| {
            if self.recent.is_empty() {
                ui.add_enabled(false, egui::Button::new("(empty)"));
                return;
            }
            let mut open: Option<PathBuf> = None;
            let mut gone: Option<PathBuf> = None;
            for p in self.recent.clone() {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.to_string_lossy().to_string());
                let btn = ui
                    .button(name)
                    .on_hover_text(p.to_string_lossy().to_string());
                if btn.clicked() {
                    if p.exists() {
                        open = Some(p);
                    } else {
                        gone = Some(p);
                    }
                    ui.close();
                }
            }
            if let Some(p) = gone {
                self.recent.retain(|q| *q != p);
                self.persist();
                self.status = format!(
                    "File not found: {}",
                    p.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
            } else if let Some(p) = open {
                self.open_file(p);
            }
        });
    }

    /// Write recent files + reading positions to disk (errors ignored).
    fn persist(&mut self) {
        let open: Vec<PathBuf> = self.tabs.iter().map(|t| t.path.clone()).collect();
        self.places
            .retain(|p, _| self.recent.contains(p) || open.contains(p));
        while self.places.len() > MAX_PLACES {
            if let Some(k) = self.places.keys().next().cloned() {
                self.places.remove(&k);
            } else {
                break;
            }
        }
        save_state(&AppStateFile {
            recent: self.recent.clone(),
            places: self.places.clone(),
        });
        self.last_resume_save = Instant::now();
    }

    /// Record one tab's reading position.
    fn note_place(&mut self, tab_idx: usize) {
        if let Some(t) = self.tabs.get(tab_idx) {
            self.places.insert(
                t.path.clone(),
                FilePlace {
                    page: t.cur,
                    zoom: t.zoom,
                },
            );
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        ui.horizontal(|ui| {
            // Preview-like airy toolbar: taller hit area, roomy groups.
            ui.spacing_mut().interact_size.y = 26.0;
            ui.add_space(2.0);
            // ---- File menu (Preview style): Open / Save / Save as ----
            {
                match self.tool_icon("file") {
                    Some(id) => {
                        let glyph = ui.visuals().widgets.inactive.fg_stroke.color;
                        ui.menu_image_button(
                            egui::Image::new((id, egui::vec2(18.0, 18.0))).tint(glyph),
                            |ui| self.file_menu(ui, tab_idx),
                        );
                    }
                    None => {
                        ui.menu_button("File", |ui| self.file_menu(ui, tab_idx));
                    }
                }
            }
            if Self::icon_button(
                ui,
                egui::vec2(22.0, 18.0),
                true,
                self.show_sidebar,
                "Toggle sidebar (F9)",
                icon_paint(self.tool_icon("panel-left"), Some(paint_sidebar_icon)),
            )
            .clicked()
            {
                self.show_sidebar = !self.show_sidebar;
            }
            toolbar_sep(ui);
            let can_nav = pages > 0;
            if Self::icon_button(
                ui,
                egui::vec2(20.0, 18.0),
                can_nav,
                false,
                "Previous page",
                icon_paint(self.tool_icon("chevron-left"), Some(paint_chevron_left)),
            )
            .clicked()
            {
                let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
                self.goto(tab_idx, cur - 1);
            }
            // Page box: plain text field, Enter jumps. (DragValue swallowed
            // Enter inside its own editor, so the outer key check never fired.)
            let mut do_goto: Option<i32> = None;
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                let resp = ui
                    .add_enabled(
                        can_nav,
                        egui::TextEdit::singleline(&mut tab.page_box).desired_width(44.0),
                    )
                    .on_hover_text("Page number, Enter to jump");
                // Keep the field in sync when the page changes elsewhere,
                // but never clobber what the user is currently typing.
                if !resp.has_focus() && tab.page_box_cur != tab.cur {
                    tab.page_box = (tab.cur + 1).to_string();
                    tab.page_box_cur = tab.cur;
                }
                if (resp.has_focus() || resp.lost_focus())
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    match tab.page_box.trim().parse::<i32>() {
                        Ok(n) if (1..=pages).contains(&n) && n - 1 != tab.cur => {
                            do_goto = Some(n - 1);
                        }
                        _ => {
                            // invalid input: snap the field back to the current page
                            tab.page_box = (tab.cur + 1).to_string();
                            tab.page_box_cur = tab.cur;
                        }
                    }
                }
            }
            if let Some(p) = do_goto {
                if can_nav {
                    self.goto(tab_idx, p);
                }
            }
            ui.label(format!("of {pages}"));
            if Self::icon_button(
                ui,
                egui::vec2(20.0, 18.0),
                can_nav,
                false,
                "Next page",
                icon_paint(self.tool_icon("chevron-right"), Some(paint_chevron_right)),
            )
            .clicked()
            {
                let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
                self.goto(tab_idx, cur + 1);
            }
            toolbar_sep(ui);
            let zoom = self.tabs.get(tab_idx).map(|t| t.zoom).unwrap_or(1.0);
            if Self::icon_button(
                ui,
                egui::vec2(20.0, 18.0),
                can_nav,
                false,
                "Zoom out (or Ctrl+wheel)",
                icon_paint(self.tool_icon("minus"), Some(paint_minus)),
            )
            .clicked()
            {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = snap_zoom(t.zoom - 0.1);
                    t.evict_page_tex();
                    let render_w = render_width(t.zoom);
                    t.evict_stale_zoom(render_w);
                }
            }
            if Self::icon_button(
                ui,
                egui::vec2(20.0, 18.0),
                can_nav,
                false,
                "Zoom in (or Ctrl+wheel)",
                icon_paint(self.tool_icon("plus"), Some(paint_plus)),
            )
            .clicked()
            {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = snap_zoom(t.zoom + 0.1);
                    t.evict_page_tex();
                    let render_w = render_width(t.zoom);
                    t.evict_stale_zoom(render_w);
                }
            }
            if ui
                .add_enabled(can_nav, egui::Button::new(format!("{:.0}%", zoom * 100.0)))
                .on_hover_text("Reset zoom to fit width")
                .clicked()
            {
                if let Some(t) = self.tabs.get_mut(tab_idx) {
                    t.zoom = 1.0;
                    t.evict_page_tex();
                    let render_w = render_width(t.zoom);
                    t.evict_stale_zoom(render_w);
                }
            }
            toolbar_sep(ui);
            // fullscreen toggle: icon only
            {
                let (tip, name) = if self.fullscreen {
                    ("Exit fullscreen (F11 / Ctrl+L or Esc)", "minimize")
                } else {
                    ("Fullscreen (F11 / Ctrl+L)", "maximize")
                };
                if Self::icon_button(
                    ui,
                    egui::vec2(22.0, 18.0),
                    true,
                    false,
                    tip,
                    icon_paint(self.tool_icon(name), None),
                )
                .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.toggle_fullscreen(&ctx);
                }
            }
            toolbar_sep(ui);
            let has_sel = self
                .tabs
                .get(tab_idx)
                .map(|t| t.selection.is_some())
                .unwrap_or(false);
            // ---- markup (Preview: highlight / underline / strike out) ----
            // Click = highlight the selection; right-click = full markup menu.
            {
                let can_undo = self
                    .tabs
                    .get(tab_idx)
                    .map(|t| !t.markup_stack.is_empty())
                    .unwrap_or(false);
                let resp = Self::icon_button(
                    ui,
                    egui::vec2(22.0, 18.0),
                    true,
                    false,
                    "Markup: click = highlight, right-click = underline / color / undo",
                    icon_paint(self.tool_icon("highlighter"), None),
                );
                if resp.clicked() {
                    self.add_markup(tab_idx, MarkupKind::Highlight);
                }
                resp.context_menu(|ui| {
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
            toolbar_sep(ui);
            // ---- print: click = system print dialog, right-click = open with ----
            {
                let resp = Self::icon_button(
                    ui,
                    egui::vec2(22.0, 18.0),
                    true,
                    false,
                    "Print (Ctrl+P), right-click to open with another app",
                    icon_paint(self.tool_icon("printer"), None),
                );
                if resp.clicked() {
                    self.start_print(tab_idx);
                }
                resp.context_menu(|ui| {
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
            toolbar_sep(ui);
            // dark mode toggle, right of print: Lucide sun/moon icon (the ☀/☽
            // glyphs are missing from the UI font and render as tofu boxes).
            {
                let tip = if self.dark_mode {
                    "Switch to light mode (Ctrl+D)"
                } else {
                    "Switch to dark mode (Ctrl+D)"
                };
                let paint = if self.dark_mode {
                    icon_paint(self.tool_icon("sun"), Some(paint_sun))
                } else {
                    icon_paint(self.tool_icon("moon"), Some(paint_moon))
                };
                if Self::icon_button(ui, egui::vec2(20.0, 18.0), true, false, tip, paint).clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.toggle_dark_mode(&ctx);
                }
            }
            toolbar_sep(ui);
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
                    if Self::icon_button(
                        ui,
                        egui::vec2(18.0, 16.0),
                        has,
                        false,
                        "Next match (Enter or F3)",
                        icon_paint(self.tool_icon("chevron-right"), Some(paint_chevron_right)),
                    )
                    .clicked()
                    {
                        self.step_match(tab_idx, 1);
                    }
                    if has {
                        ui.label(format!("{}/{}", cursor + 1, total));
                    } else {
                        ui.label("0/0");
                    }
                    if Self::icon_button(
                        ui,
                        egui::vec2(18.0, 16.0),
                        has,
                        false,
                        "Previous match (Shift+Enter or Shift+F3)",
                        icon_paint(self.tool_icon("chevron-left"), Some(paint_chevron_left)),
                    )
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
                        let dark = ui.visuals().dark_mode;
                        let fill = if dark {
                            egui::Color32::from_gray(44)
                        } else {
                            egui::Color32::WHITE
                        };
                        let edge = if dark {
                            egui::Color32::from_gray(75)
                        } else {
                            egui::Color32::from_rgb(198, 198, 200)
                        };
                        let resp = ui.add_enabled(
                            can_nav,
                            egui::TextEdit::singleline(&mut t.search_text)
                                .hint_text("Search, Enter/F3 = next")
                                .desired_width(130.0)
                                .frame(
                                    egui::Frame::new()
                                        .fill(fill)
                                        .stroke(egui::Stroke::new(1.0, edge))
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
                if Self::icon_button(
                    ui,
                    egui::vec2(20.0, 18.0),
                    can_nav,
                    false,
                    "Search (Ctrl+F)",
                    icon_paint(self.tool_icon("search"), None),
                )
                .clicked()
                {
                    self.focus_search = true;
                }
            });
        });
    }

    /// Thumbnails: render only the visible range, max 3 per frame, fill in gradually.
    fn thumbs(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        let mut budget = 3;
        let mut pending = false;
        let cur = self.tabs.get(tab_idx).map(|t| t.cur).unwrap_or(0);
        let match_counts: HashMap<i32, usize> = self
            .tabs
            .get(tab_idx)
            .map(|t| {
                t.search_by_page
                    .iter()
                    .map(|(p, v)| (*p, v.len()))
                    .collect()
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
                    let btn = egui::Button::image(egui::Image::new((tex.id(), egui::vec2(w, h))))
                        .selected(selected);
                    let tresp = ui.add_sized([w + 8.0, h + 4.0], btn);
                    if tresp.clicked() {
                        self.goto(tab_idx, i);
                    }
                    // Preview-style page management on right-click.
                    tresp.context_menu(|ui| {
                        if ui.button("Rotate left").clicked() {
                            self.rotate_page(tab_idx, i, true);
                            ui.close();
                        }
                        if ui.button("Rotate right").clicked() {
                            self.rotate_page(tab_idx, i, false);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(
                                pages > 1,
                                egui::Button::new("Delete page"),
                            )
                            .on_hover_text("Delete this page (saves file)")
                            .clicked()
                        {
                            self.delete_page(tab_idx, i);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Append PDF…")
                            .on_hover_text("Merge another PDF at the end (saves file)")
                            .clicked()
                        {
                            self.append_pdf(tab_idx);
                            ui.close();
                        }
                    });
                    if ui.is_rect_visible(tresp.rect) {
                        let dark = ui.visuals().dark_mode;
                        let edge = if selected {
                            preview_link(dark)
                        } else if dark {
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30)
                        } else {
                            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 40)
                        };
                        ui.painter().rect_stroke(
                            tresp.rect,
                            3.0,
                            egui::Stroke::new(if selected { 1.5 } else { 1.0 }, edge),
                            egui::StrokeKind::Inside,
                        );
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
                    resp.context_menu(|ui| {
                        if ui.button("Rotate left").clicked() {
                            self.rotate_page(tab_idx, i, true);
                            ui.close();
                        }
                        if ui.button("Rotate right").clicked() {
                            self.rotate_page(tab_idx, i, false);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(
                                pages > 1,
                                egui::Button::new("Delete page"),
                            )
                            .on_hover_text("Delete this page (saves file)")
                            .clicked()
                        {
                            self.delete_page(tab_idx, i);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Append PDF…")
                            .on_hover_text("Merge another PDF at the end (saves file)")
                            .clicked()
                        {
                            self.append_pdf(tab_idx);
                            ui.close();
                        }
                    });
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

    /// Sidebar with Preview-style mode switch: thumbnails, search results, outline.
    fn sidebar(&mut self, ui: &mut egui::Ui, tab_idx: usize, pages: i32) {
        let n_results = self
            .tabs
            .get(tab_idx)
            .map(|t| t.search_matches.len())
            .unwrap_or(0);
        ui.horizontal_wrapped(|ui| {
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
            if ui
                .selectable_label(self.sidebar_mode == SidebarMode::Outline, "Contents")
                .on_hover_text("Document outline (bookmarks)")
                .clicked()
            {
                self.sidebar_mode = SidebarMode::Outline;
            }
            if ui
                .selectable_label(
                    self.sidebar_mode == SidebarMode::Notes,
                    format!(
                        "Notes ({})",
                        self.tabs
                            .get(tab_idx)
                            .and_then(|t| t.notes.as_ref().map(|v| v.len()))
                            .unwrap_or(0)
                    ),
                )
                .on_hover_text("Annotations in this document")
                .clicked()
            {
                self.sidebar_mode = SidebarMode::Notes;
            }
        });
        ui.separator();
        if self.sidebar_mode == SidebarMode::Results {
            self.results(ui, tab_idx);
        } else if self.sidebar_mode == SidebarMode::Outline {
            self.ensure_outline(tab_idx);
            self.outline_view(ui, tab_idx);
        } else if self.sidebar_mode == SidebarMode::Notes {
            self.ensure_notes(tab_idx);
            self.notes_view(ui, tab_idx);
        } else {
            self.thumbs(ui, tab_idx, pages);
        }
    }

    /// Annotations list: jump on click, delete via × (Preview Highlights & Notes).
    fn notes_view(&mut self, ui: &mut egui::Ui, tab_idx: usize) {
        const MAX_SHOWN: usize = 1000;
        let mut jump_to: Option<i32> = None;
        let mut delete: Option<(i32, usize)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let items = match self.tabs.get(tab_idx).and_then(|t| t.notes.clone()) {
                Some(v) => v,
                None => {
                    ui.spinner();
                    ui.label("Loading annotations…");
                    return;
                }
            };
            if items.is_empty() {
                ui.label("No annotations in this document.");
                ui.label("Select text, then Markup.");
                return;
            }
            let shown = items.len().min(MAX_SHOWN);
            if items.len() > shown {
                ui.label(format!(
                    "Showing first {shown} of {} annotations",
                    items.len()
                ));
            }
            for item in items.iter().take(shown) {
                ui.horizontal(|ui| {
                    let title = if item.text.is_empty() {
                        format!("p.{} — {}", item.page + 1, item.label)
                    } else {
                        format!(
                            "p.{} — {}: {}",
                            item.page + 1,
                            item.label,
                            note_snippet(&item.text)
                        )
                    };
                    let resp = ui.selectable_label(false, title);
                    if resp
                        .on_hover_text(if item.text.is_empty() {
                            format!("Go to page {}", item.page + 1)
                        } else {
                            item.text.clone()
                        })
                        .clicked()
                    {
                        jump_to = Some(item.page);
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Delete this annotation (saves file)")
                        .clicked()
                    {
                        delete = Some((item.page, item.index));
                    }
                });
            }
        });
        if let Some(p) = jump_to {
            self.goto(tab_idx, p);
        }
        if let Some((page, index)) = delete {
            self.delete_note(tab_idx, page, index);
        }
    }

    /// Document outline list: indented entries, click to jump (Preview Contents).
    fn outline_view(&mut self, ui: &mut egui::Ui, tab_idx: usize) {
        let mut jump_to: Option<i32> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let items = match self.tabs.get(tab_idx).and_then(|t| t.outline.clone()) {
                Some(v) => v,
                None => {
                    ui.spinner();
                    ui.label("Loading outline…");
                    return;
                }
            };
            if items.is_empty() {
                ui.label("No outline in this document.");
                return;
            }
            for item in &items {
                ui.horizontal(|ui| {
                    ui.add_space((item.depth.min(6) as f32) * 12.0);
                    let text: String = item.title.split_whitespace().collect::<Vec<_>>().join(" ");
                    let label = if text.is_empty() {
                        "(untitled)".to_owned()
                    } else {
                        text
                    };
                    let tip = match item.page {
                        Some(p) => format!("Go to page {}", p + 1),
                        None => "No page target".to_owned(),
                    };
                    if ui
                        .selectable_label(false, label)
                        .on_hover_text(tip)
                        .clicked()
                    {
                        if let Some(p) = item.page {
                            jump_to = Some(p);
                        }
                    }
                });
            }
        });
        if let Some(p) = jump_to {
            self.goto(tab_idx, p);
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
        const MAX_SHOWN: usize = 1000;
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
            let shown = total.min(MAX_SHOWN);
            if total > shown {
                ui.label(format!("Showing first {shown} of {total} matches"));
            }
            for j in 0..shown {
                let (text, page) = match self.tabs.get(tab_idx) {
                    Some(t) => {
                        let m = &t.search_matches[j];
                        let s = t
                            .search_snippets
                            .get(j)
                            .cloned()
                            .unwrap_or_else(|| Self::match_snippet(t, m));
                        (s, m.page)
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
        // NOTE (egui 0.34): Ctrl+wheel never reaches smooth_scroll_delta.
        // The input layer diverts it into the zoom factor (its built-in UI
        // zoom gesture) and zeroes the scroll delta, so the factor below is
        // the only signal. Nothing else consumes it (egui never auto-applies
        // it to the global UI zoom), so no reset is needed. The smooth branch
        // stays as a fallback for platforms whose modifiers don't match
        // egui's zoom_modifier.
        let zoom_factor = ui.input(|i| i.zoom_delta());
        if zoom_factor != 1.0 {
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                let z = snap_zoom(tab.zoom * zoom_factor);
                if (z - tab.zoom).abs() > f32::EPSILON {
                    tab.zoom = z;
                    tab.evict_page_tex();
                    tab.evict_stale_zoom(render_width(z));
                }
            }
        }
        let (dy, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl));
        if ctrl && dy != 0.0 {
            let f = if dy > 0.0 { 1.1 } else { 1.0 / 1.1 };
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                let z = snap_zoom(tab.zoom * f);
                if (z - tab.zoom).abs() > f32::EPSILON {
                    tab.zoom = z;
                    tab.evict_page_tex();
                    tab.evict_stale_zoom(render_width(z));
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
            let render_w = render_width(zoom) as i32;
            for i in 0..pages {
                let (pw, ph) = self
                    .tabs
                    .get(tab_idx)
                    .and_then(|t| t.aspects.get(&i).copied())
                    .unwrap_or((1.0, std::f32::consts::SQRT_2));
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
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    let _ = ui.allocate_rect(img_rect, egui::Sense::hover());
                    if visible && first_visible.is_none() {
                        first_visible = Some(i);
                    }
                    let scroll_target = self.tabs.get(tab_idx).and_then(|t| t.scroll_target);
                    if scroll_target == Some(i) && !scrolled {
                        // Search jumps center the current match; plain jumps center the page.
                        let mut dest = img_rect;
                        if let Some(tab) = self.tabs.get(tab_idx) {
                            if tab.scroll_to_match {
                                if let Some(m) = tab
                                    .search_matches
                                    .get(tab.search_cursor)
                                    .filter(|m| m.page == i)
                                {
                                    if let Some(chars) = tab.text_cache.get(&i) {
                                        let last = chars.len().saturating_sub(1);
                                        let mut union: Option<egui::Rect> = None;
                                        for k in m.start..=m.end.min(last) {
                                            if let Some(r) =
                                                pdf_rect_to_screen(&chars[k], &img_rect, pw, ph)
                                            {
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
                    if visible {
                        self.ensure_page_layers(tab_idx, i);
                    }
                    // paint search matches: yellow, current one orange
                    if let Some(tab) = self.tabs.get(tab_idx) {
                        if let Some(ids) = tab.search_by_page.get(&i) {
                            if let Some(chars) = tab.text_cache.get(&i) {
                                let painter = ui.painter();
                                let last = chars.len().saturating_sub(1);
                                for &mi in ids {
                                    let m = &tab.search_matches[mi];
                                    let col = if mi == tab.search_cursor {
                                        egui::Color32::from_rgba_unmultiplied(255, 140, 0, 110)
                                    } else {
                                        egui::Color32::from_rgba_unmultiplied(255, 235, 0, 90)
                                    };
                                    // one merged fill per line instead of per glyph
                                    let raw: Vec<(f32, f32, f32, f32)> = (m.start
                                        ..=m.end.min(last))
                                        .filter_map(|k| {
                                            pdf_rect_to_screen(&chars[k], &img_rect, pw, ph)
                                                .map(|r| (r.min.x, r.min.y, r.max.x, r.max.y))
                                        })
                                        .collect();
                                    for (l, b, r, t) in merge_lines(&raw) {
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(l, b),
                                                egui::pos2(r, t),
                                            ),
                                            1.0,
                                            col,
                                        );
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
                                    let sel_col = preview_selection(ui.visuals().dark_mode);
                                    // merge per-char boxes into line boxes: a
                                    // whole-line fill is one draw, not one per glyph
                                    let raw: Vec<(f32, f32, f32, f32)> = (sel.start
                                        ..=sel.end.min(chars.len().saturating_sub(1)))
                                        .filter_map(|k| {
                                            let c = &chars[k];
                                            pdf_rect_to_screen_raw(
                                                c.left, c.bottom, c.right, c.top, &img_rect, pw, ph,
                                            )
                                            .map(|r| (r.min.x, r.min.y, r.max.x, r.max.y))
                                        })
                                        .collect();
                                    for (l, b, r, t) in merge_lines(&raw) {
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(l, b),
                                                egui::pos2(r, t),
                                            ),
                                            1.0,
                                            sel_col,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    // ---- links: underline, hover highlight, tooltip ----
                    // All slice-derived values are computed inside this scope so the
                    // immutable borrow of self.tabs ends before the context menu and
                    // the secondary-click handler take it mutably.
                    let mut inter;
                    let menu_link: Option<PageLink>;
                    let menu_img: Option<PdfImage>;
                    let frame_rect: Option<egui::Rect>;
                    let pin_op: Option<(i32, PdfImage)>;
                    let hover_link: bool;
                    {
                        let page_links: &[PageLink] = self
                            .tabs
                            .get(tab_idx)
                            .and_then(|t| t.link_cache.get(&i).map(|v| v.as_slice()))
                            .unwrap_or(&[]);
                        let page_images: &[PdfImage] = self
                            .tabs
                            .get(tab_idx)
                            .and_then(|t| t.image_cache.get(&i).map(|v| v.as_slice()))
                            .unwrap_or(&[]);
                        let hover_pdf = ui
                            .ctx()
                            .pointer_hover_pos()
                            .filter(|p| img_rect.contains(*p))
                            .and_then(|p| screen_to_pdf(p, &img_rect, pw, ph));
                        let hover_idx: Option<usize> = hover_pdf.and_then(|(px, py)| {
                            page_links.iter().position(|l| l.contains(px, py))
                        });
                        // smallest image under the pointer wins (most specific hit)
                        let hover_img: Option<usize> = hover_pdf.and_then(|(px, py)| {
                            page_images
                                .iter()
                                .enumerate()
                                .filter(|(_, im)| im.contains(px, py))
                                .min_by(|(_, a), (_, b)| a.area().total_cmp(&b.area()))
                                .map(|(k, _)| k)
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
                        let sid = egui::Id::new(format!("page-sel-{tab_idx}-{i}"));
                        inter = ui.interact(img_rect, sid, egui::Sense::click_and_drag());
                        if let Some(l) = hover_idx.and_then(|k| page_links.get(k)) {
                            inter = inter.on_hover_text(l.label());
                        }
                        menu_link = hover_idx.and_then(|k| page_links.get(k).cloned());
                        hover_link = menu_link.is_some();
                        // while the context menu is open the pointer sits over the
                        // floating menu, not the page — hover is dead by then, so
                        // serve the image pinned at right-click time
                        let pinned_img = self
                            .tabs
                            .get(tab_idx)
                            .and_then(|t| t.context_image.clone())
                            .filter(|(p, _)| *p == i)
                            .map(|(_, im)| im);
                        menu_img = hover_img
                            .and_then(|k| page_images.get(k).cloned())
                            .or(pinned_img);
                        pin_op = hover_img.map(|k| (i, page_images[k].clone()));
                        // hover frame, or the pinned image while the menu is open
                        let pinned = self
                            .tabs
                            .get(tab_idx)
                            .and_then(|t| t.context_image.as_ref())
                            .filter(|(p, _)| *p == i)
                            .map(|(_, im)| im);
                        let frame_img = hover_img.and_then(|k| page_images.get(k)).or(pinned);
                        frame_rect = frame_img.and_then(|im| {
                            pdf_rect_to_screen_raw(
                                im.left, im.bottom, im.right, im.top, &img_rect, pw, ph,
                            )
                        });
                    }
                    if let Some(r) = frame_rect {
                        ui.painter().rect_stroke(
                            r,
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 140, 255)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    // right-click pins the hovered image so the context menu can
                    // outlive hover (pointer moves onto the floating menu).
                    // A left click elsewhere clears the stale pin. NOTE: a right
                    // click also reports `clicked()`, so check the button too.
                    if inter.secondary_clicked() {
                        // Capture the click position NOW: once the pointer moves
                        // onto the floating menu, interact_pointer_pos() no
                        // longer reflects the page click.
                        let point = inter
                            .interact_pointer_pos()
                            .and_then(|pos| screen_to_pdf(pos, &img_rect, pw, ph))
                            .map(|(x, y)| (i, x, y));
                        if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            tab.context_image = pin_op;
                            tab.context_point = point;
                        }
                    } else if inter.clicked() && !ui.input(|i| i.pointer.secondary_down()) {
                        if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            tab.context_image = None;
                            tab.context_point = None;
                        }
                    }
                    // right-click menu: copy / select all / link actions
                    {
                        inter.context_menu(|ui| {
                            let has_sel =
                                self.tabs.get(tab_idx).and_then(|t| t.selection).is_some();
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
                            if let Some(im) = &menu_img {
                                ui.separator();
                                let page = i;
                                let obj = im.obj;
                                if ui.button("Copy image").clicked() {
                                    let im = im.clone();
                                    self.copy_image(tab_idx, page, &im);
                                    ui.close();
                                }
                                if ui.button("Save image as…").clicked() {
                                    let im = im.clone();
                                    self.save_image(tab_idx, page, &im);
                                    ui.close();
                                }
                                let _ = obj;
                            }
                            // Preview-style sticky note at the pinned
                            // right-click point (captured at click time).
                            let click_pdf: Option<(i32, f32, f32)> = self
                                .tabs
                                .get(tab_idx)
                                .and_then(|t| t.context_point);
                            if ui.button("Add note here").clicked() {
                                match click_pdf {
                                    Some((pg, x, y)) => {
                                        self.note_draft = Some(NoteDraft {
                                            tab: tab_idx,
                                            page: pg,
                                            x,
                                            y,
                                            text: String::new(),
                                            fresh: true,
                                        });
                                        if let Some(tab) = self.tabs.get_mut(tab_idx) {
                                            tab.context_point = None;
                                        }
                                    }
                                    None => {
                                        self.status =
                                            "Could not locate click position".to_owned();
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
                                        if ui.button(format!("Go to page {}", dest + 1)).clicked() {
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
                        if let Some(l) = &menu_link {
                            self.open_link(tab_idx, l);
                        } else if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            if tab.selection.map(|s| s.page) == Some(i) {
                                tab.selection = None;
                            }
                        }
                    }
                    // hand cursor over links, text beam elsewhere on the page
                    if hover_link {
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
                    let is_target = self.tabs.get(tab_idx).and_then(|t| t.scroll_target) == Some(i);
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
                tab.evict_layers();
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
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::W))
            && !self.tabs.is_empty()
        {
            let idx = self.active;
            self.close_tab(idx);
        }
        // Ctrl+Tab / Ctrl+Shift+Tab to cycle tabs
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab))
            && self.tabs.len() > 1
        {
            let shift = ui.input(|i| i.modifiers.shift);
            if shift {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
            } else {
                self.active = (self.active + 1) % self.tabs.len();
            }
        }
        // Ctrl+C copies current selection — but only steals the key when a PDF
        // selection actually exists, so focused text fields keep their own copy.
        // NOTE: some IMEs/filters swallow key-down events (only releases arrive),
        // so a Ctrl-held release counts as a press too. Copy is idempotent,
        // firing twice on normal systems is harmless.
        let copy_pressed = ui.input(|i| {
            (i.key_pressed(egui::Key::C)
                || i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::Key {
                            key: egui::Key::C,
                            pressed: false,
                            ..
                        }
                    )
                }))
                && i.modifiers.ctrl
        });
        if copy_pressed && self.active_tab().and_then(|t| t.selection).is_some() {
            ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::C));
            let ctx = ui.ctx().clone();
            self.copy_selection(&ctx);
        }
        // Ctrl+P opens the print dialog
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P))
            && !self.tabs.is_empty()
        {
            let idx = self.active;
            self.start_print(idx);
        }
        // Ctrl+S saves the active tab
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S))
            && !self.tabs.is_empty()
        {
            let idx = self.active;
            self.save_now(idx);
        }
        // Ctrl+F focuses the search field
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F))
            && !self.tabs.is_empty()
        {
            self.focus_search = true;
        }
        // Ctrl+D toggles dark mode
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::D)) {
            let ctx = ui.ctx().clone();
            self.toggle_dark_mode(&ctx);
        }
        if ui.input(|i| i.key_pressed(egui::Key::F9)) {
            self.show_sidebar = !self.show_sidebar;
        }
        // F11 / Ctrl+L toggles fullscreen, Esc exits it
        let f11 = ui.input(|i| i.key_pressed(egui::Key::F11));
        let ctrl_l = ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::L));
        if f11 || ctrl_l {
            let ctx = ui.ctx().clone();
            self.toggle_fullscreen(&ctx);
        } else if self.fullscreen && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            let ctx = ui.ctx().clone();
            self.toggle_fullscreen(&ctx);
        }

        // process one print page per frame (keeps UI responsive + shows progress)
        self.process_print_job();

        // Poll background file-open results.
        let ctx_clone = ui.ctx().clone();
        self.poll_open(&ctx_clone);
        self.poll_search(&ctx_clone);
        self.poll_render(&ctx_clone);
        self.poll_layers(&ctx_clone);
        self.poll_outline(&ctx_clone);
        self.poll_notes(&ctx_clone);
        // Debounced reading-position autosave: snapshot open tabs each frame,
        // flush to disk at most every 2 s so a crash/exit never loses much.
        {
            let snap: Vec<(PathBuf, i32, f32)> = self
                .tabs
                .iter()
                .map(|t| (t.path.clone(), t.cur, t.zoom))
                .collect();
            if snap != self.resume_snapshot {
                self.resume_snapshot = snap;
                if self.last_resume_save.elapsed() > Duration::from_secs(2) {
                    let marks: Vec<(PathBuf, FilePlace)> = self
                        .tabs
                        .iter()
                        .map(|t| {
                            (
                                t.path.clone(),
                                FilePlace {
                                    page: t.cur,
                                    zoom: t.zoom,
                                },
                            )
                        })
                        .collect();
                    for (p, place) in marks {
                        self.places.insert(p, place);
                    }
                    self.persist();
                }
            }
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
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Title(title));
            }
        }

        // Fullscreen = content only: no toolbar, no sidebar, no status bar.
        let fs = self.fullscreen;
        let dark = ui.visuals().dark_mode;
        let (toolbar_fill, sidebar_fill, status_fill) = preview_chrome(dark);
        if !fs {
            let top_out = egui::Panel::top("toolbar")
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
            let side_out = egui::Panel::left("sidebar")
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
                    let hint_col = if ui.visuals().dark_mode {
                        egui::Color32::from_gray(155)
                    } else {
                        egui::Color32::from_rgb(99, 99, 102)
                    };
                    ui.small(
                        egui::RichText::new("Drag to select text • Right-click for menu • Links are clickable • Ctrl+F search")
                            .color(hint_col),
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
                            .small_button("Exit fullscreen (F11 / Ctrl+L)")
                            .on_hover_text("Exit fullscreen (F11 / Ctrl+L or Esc)")
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.toggle_fullscreen(&ctx);
                        }
                    });
            }
        } else if !self.opening.is_empty() {
            let names: Vec<String> = self
                .opening
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.to_string_lossy().to_string())
                })
                .collect();
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.heading("Opening…");
                        for n in &names {
                            ui.label(n);
                        }
                    });
                });
            });
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
            let bot_out = egui::Panel::bottom("status")
                .frame(egui::Frame::side_top_panel(ui.style()).fill(status_fill))
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(&self.status);
                        if let Some(d) = self.active_tab() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.monospace(d.path.to_string_lossy().to_string());
                                },
                            );
                        }
                    });
                });
            paint_groove(ui, bot_out.response.rect, 1);
        }
        self.note_dialog(ui);
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
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(fam)
            .or_default()
            .push("cjk".to_owned());
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
    if let Ok(img) = image::load_from_memory(include_bytes!("../assets/logo-256.png").as_slice()) {
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
            // Apply initial theme (light by default, respects app.dark_mode field).
            MiniPdf::apply_theme(&cc.egui_ctx, app.dark_mode);
            // Pre-decode UI icons to textures (synchronous; never shows ⚠).
            app.tex_logo = load_embedded_tex(
                &cc.egui_ctx,
                "minipdf-logo",
                include_bytes!("../assets/logo-256.png"),
            );
            // Lucide toolbar icons (white glyph + alpha; tinted per theme at paint time).
            const TOOL_ICONS: &[(&str, &[u8])] = &[
                (
                    "chevron-left",
                    include_bytes!("../assets/icon-chevron-left.png"),
                ),
                (
                    "chevron-right",
                    include_bytes!("../assets/icon-chevron-right.png"),
                ),
                ("minus", include_bytes!("../assets/icon-minus.png")),
                ("plus", include_bytes!("../assets/icon-plus.png")),
                (
                    "panel-left",
                    include_bytes!("../assets/icon-panel-left.png"),
                ),
                ("sun", include_bytes!("../assets/icon-sun.png")),
                ("moon", include_bytes!("../assets/icon-moon.png")),
                ("maximize", include_bytes!("../assets/icon-maximize.png")),
                ("minimize", include_bytes!("../assets/icon-minimize.png")),
                ("printer", include_bytes!("../assets/icon-printer.png")),
                (
                    "highlighter",
                    include_bytes!("../assets/icon-highlighter.png"),
                ),
                ("search", include_bytes!("../assets/icon-search.png")),
                ("file", include_bytes!("../assets/icon-file.png")),
            ];
            for &(name, bytes) in TOOL_ICONS {
                if let Some(tex) =
                    load_embedded_tex(&cc.egui_ctx, &format!("minipdf-icon-{name}"), bytes)
                {
                    app.tool_icons.insert(name, tex);
                }
            }
            setup_cjk_ui_font(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_row(text: &str) -> Vec<CharInfo> {
        text.chars()
            .enumerate()
            .map(|(i, ch)| CharInfo {
                ch,
                left: i as f32 * 10.0,
                bottom: 0.0,
                right: i as f32 * 10.0 + 9.0,
                top: 10.0,
            })
            .collect()
    }

    #[test]
    fn render_width_is_clamped() {
        assert_eq!(render_width(1.0), 1100);
        assert_eq!(render_width(10.0), MAX_RENDER_WIDTH as u32);
        assert_eq!(render_width(0.01), 11);
    }

    #[test]
    fn search_query_normalization_keeps_char_alignment() {
        assert_eq!(normalized_query("  HeLLo "), "hello");
        assert_eq!(normalized_query(""), "");
    }

    #[test]
    fn search_page_finds_non_overlapping_matches_with_snippets() {
        let chars = char_row("hello hello");
        let query: Vec<char> = "hello".chars().collect();
        let cancel = AtomicBool::new(false);
        let mut out = SearchOutput::default();
        search_page(&mut out, 3, &chars, &query, &cancel);
        assert_eq!(out.matches.len(), 2);
        assert_eq!(out.snippets.len(), 2);
        assert_eq!(out.matches[0].page, 3);
        assert_eq!(out.matches[0].start, 0);
        assert_eq!(out.matches[0].end, 4);
        assert_eq!(out.matches[1].start, 6);
    }

    #[test]
    fn search_page_respects_cancel() {
        let chars = char_row("hello hello");
        let query: Vec<char> = "hello".chars().collect();
        let cancel = AtomicBool::new(true);
        let mut out = SearchOutput::default();
        search_page(&mut out, 0, &chars, &query, &cancel);
        assert!(out.matches.is_empty());
    }

    #[test]
    fn markup_undo_validates_annotation_count() {
        let undo = MarkupUndo {
            page: 2,
            before_len: 5,
            count: 3,
        };
        assert!(undo.valid(8));
        assert!(!undo.valid(7));
        assert!(!MarkupUndo {
            page: 2,
            before_len: 5,
            count: 0
        }
        .valid(5));
    }

    #[test]
    fn stale_render_width_is_rejected() {
        let current = render_width(1.0);
        assert_ne!(current, render_width(4.0));
        assert_eq!(render_width(4.0), MAX_RENDER_WIDTH as u32);
    }

    #[test]
    fn merge_lines_groups_same_row() {
        let rows = vec![
            (0.0, 0.0, 10.0, 10.0),
            (11.0, 0.5, 20.0, 10.5),
            (0.0, 30.0, 10.0, 40.0),
        ];
        let merged = merge_lines(&rows);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn rotation_steps_cycle() {
        use PdfPageRenderRotation as R;
        // single steps
        assert_eq!(step_rotation(R::None, false), R::Degrees90);
        assert_eq!(step_rotation(R::None, true), R::Degrees270);
        assert_eq!(step_rotation(R::Degrees90, true), R::None);
        assert_eq!(step_rotation(R::Degrees270, false), R::None);
        // four steps either way return to start
        for start in [R::None, R::Degrees90, R::Degrees180, R::Degrees270] {
            let mut r = start;
            for _ in 0..4 {
                r = step_rotation(r, false);
            }
            assert_eq!(r, start);
            for _ in 0..4 {
                r = step_rotation(r, true);
            }
            assert_eq!(r, start);
        }
        // left then right is identity
        for start in [R::None, R::Degrees90, R::Degrees180, R::Degrees270] {
            assert_eq!(step_rotation(step_rotation(start, true), false), start);
        }
    }

    #[test]
    fn page_ops_and_notes_round_trip() {
        use PdfPageRenderRotation as R;
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        assert!(src.exists(), "missing {}", src.display());
        let dir = std::env::temp_dir().join(format!("minipdf-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pdf = dir.join("ops.pdf");
        std::fs::copy(&src, &pdf).expect("copy test pdf");
        let pdfium = minipdf::ensure_pdfium().expect("pdfium");
        // make it 2 pages by appending a copy of itself.
        // NOTE: the appended doc is loaded from bytes (not the same file
        // handle) and every document is dropped before overwriting the file:
        // Pdfium aborts when a save target is still open.
        let n0 = pdfium
            .load_pdf_from_file(&pdf, None)
            .expect("load")
            .pages()
            .len();
        assert_eq!(n0, 1);
        {
            let bytes = std::fs::read(&pdf).expect("read");
            let mut doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let other = pdfium.load_pdf_from_byte_vec(bytes, None).expect("load");
            doc.pages_mut().append(&other).expect("append");
            assert_eq!(doc.pages().len(), 2);
            // Pdfium cannot save over its own open source: tmp + rename,
            // same as the app's instant-persistence pattern.
            let tmp = dir.join("ops-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(other);
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        // rotate page 0 right, add text box + sticky note
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page 0");
            pg.set_rotation(step_rotation(pg.rotation().expect("rot"), false));
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            let mut ft = annots
                .create_free_text_annotation("hello box")
                .expect("freetext");
            ft.set_bounds(rect_pt(10.0, 10.0, 230.0, 100.0))
                .expect("bounds");
            let mut nt = annots.create_text_annotation("sticky").expect("text");
            nt.set_bounds(rect_pt(10.0, 120.0, 30.0, 140.0))
                .expect("bounds");
            assert_eq!(annots.len(), before_len + 2);
            let tmp = dir.join("ops-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        // reload: rotation + notes visible
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            assert_eq!(doc.pages().len(), 2);
            let pg = doc.pages().get(0).expect("page 0");
            assert_eq!(pg.rotation().expect("rot"), R::Degrees90);
        }
        let notes = fetch_notes(&pdf).expect("notes");
        assert!(notes.iter().any(|n| n.label == "Text box" && n.page == 0));
        assert!(notes.iter().any(|n| n.label == "Note" && n.page == 0));
        // export render produces bytes
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let pg = doc.pages().get(0).expect("page 0");
            let bmp = pg
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(200)
                        .set_maximum_height(600),
                )
                .expect("render");
            let rgba = bmp.as_image().expect("image").to_rgba8();
            assert!(rgba.width() > 0 && rgba.height() > 0);
            assert!(!rgba.as_raw().is_empty());
        }
        // delete page 0 -> 1 page left
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            doc.pages().get(0).expect("page 0").delete().expect("delete");
            let tmp = dir.join("ops-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            assert_eq!(doc.pages().len(), 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: creating notes must not spawn hidden sibling annotations,
    /// otherwise MarkupUndo::valid() would reject the undo.
    #[test]
    fn note_annotations_keep_stable_counts() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        let dir = std::env::temp_dir().join(format!("minipdf-undo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pdf = dir.join("undo.pdf");
        std::fs::copy(&src, &pdf).expect("copy");
        let pdfium = minipdf::ensure_pdfium().expect("pdfium");
        let count = |path: &PathBuf| -> Vec<String> {
            let doc = pdfium.load_pdf_from_file(path, None).expect("load");
            let pg = doc.pages().get(0).expect("page");
            let annots = pg.annotations();
            annots
                .as_range()
                .map(|idx| {
                    format!(
                        "{idx}:{:?}",
                        annots.get(idx).expect("annot").annotation_type()
                    )
                })
                .collect()
        };
        eprintln!("TEST: before={:?}", count(&pdf));
        // one freetext, tmp+rename like the app
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page");
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            let mut ft = annots
                .create_free_text_annotation("hello box")
                .expect("freetext");
            ft.set_bounds(rect_pt(10.0, 10.0, 230.0, 100.0))
                .expect("bounds");
            eprintln!("TEST: after create len={} (before={before_len})", annots.len());
            let tmp = dir.join("undo-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        let after_ft = count(&pdf);
        eprintln!("TEST: after freetext reload={after_ft:?}");
        // one text note
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page");
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            let mut nt = annots.create_text_annotation("sticky").expect("text");
            nt.set_bounds(rect_pt(10.0, 120.0, 30.0, 140.0))
                .expect("bounds");
            eprintln!("TEST: after create len={} (before={before_len})", annots.len());
            let tmp = dir.join("undo-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        let after_nt = count(&pdf);
        eprintln!("TEST: after text reload={after_nt:?}");
        // simulate app undo: valid(before+1)? then delete last
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page");
            let undo = MarkupUndo {
                page: 0,
                before_len: after_ft.len(),
                count: 1,
            };
            let annots = pg.annotations_mut();
            eprintln!("TEST: undo valid={} current={}", undo.valid(annots.len()), annots.len());
            let tmp = dir.join("undo-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            let _ = std::fs::remove_file(&tmp);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: an API-created FreeText box must actually paint pixels
    /// (this Pdfium build draws broken appearances for markup quads, so
    /// any annotation type we ship needs a render check).
    #[test]
    fn free_text_box_renders_visibly() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        let dir = std::env::temp_dir().join(format!("minipdf-vis-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pdf = dir.join("vis.pdf");
        std::fs::copy(&src, &pdf).expect("copy");
        let pdfium = minipdf::ensure_pdfium().expect("pdfium");
        let render = |path: &PathBuf| -> Vec<u8> {
            let doc = pdfium.load_pdf_from_file(path, None).expect("load");
            let pg = doc.pages().get(0).expect("page");
            let bmp = pg
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(400)
                        .set_maximum_height(1200),
                )
                .expect("render");
            bmp.as_image().expect("image").to_rgba8().as_raw().to_vec()
        };
        let base = render(&pdf);
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page");
            let annots = pg.annotations_mut();
            let mut ft = annots
                .create_free_text_annotation("hello box")
                .expect("freetext");
            ft.set_bounds(rect_pt(10.0, 10.0, 230.0, 100.0))
                .expect("bounds");
            ft.set_fill_color(PdfColor::new(255, 255, 180, 255))
                .expect("fill");
            ft.set_stroke_color(PdfColor::new(110, 110, 110, 255))
                .expect("stroke");
            let mut nt = annots.create_text_annotation("sticky").expect("text");
            nt.set_bounds(rect_pt(10.0, 120.0, 30.0, 140.0))
                .expect("bounds");
            let tmp = dir.join("vis-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        let after = render(&pdf);
        assert_eq!(base.len(), after.len());
        let diff = base
            .chunks_exact(4)
            .zip(after.chunks_exact(4))
            .filter(|(a, b)| {
                (a[0] as i32 - b[0] as i32).abs()
                    + (a[1] as i32 - b[1] as i32).abs()
                    + (a[2] as i32 - b[2] as i32).abs()
                    > 30
            })
            .count();
        eprintln!("TEST: pixels={} differing={}", base.len() / 4, diff);
        // bounding box of differing pixels + dark-pixel (text) census
        let w = 400usize;
        let h = base.len() / 4 / w;
        let mut minx = w;
        let mut miny = h;
        let mut maxx = 0usize;
        let mut maxy = 0usize;
        let mut dark_base = 0usize;
        let mut dark_after = 0usize;
        for (idx, (a, b)) in base
            .chunks_exact(4)
            .zip(after.chunks_exact(4))
            .enumerate()
        {
            let d = (a[0] as i32 - b[0] as i32).abs()
                + (a[1] as i32 - b[1] as i32).abs()
                + (a[2] as i32 - b[2] as i32).abs();
            if d > 30 {
                let (x, y) = (idx % w, idx / w);
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
            }
            if a[0] < 80 && a[1] < 80 && a[2] < 80 {
                dark_base += 1;
            }
            if b[0] < 80 && b[1] < 80 && b[2] < 80 {
                dark_after += 1;
            }
        }
        eprintln!("TEST: diff-bbox x{minx}..{maxx} y{miny}..{maxy} (img {w}x{h})");
        eprintln!("TEST: dark base={dark_base} after={dark_after}");
        // the 220x90 pt box must paint a solid area, not just edges
        assert!(diff > 1000, "free-text box painted only {diff} pixels");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: the app's sticky note (square marker + Contents) must come
    /// back from fetch_notes labeled "Note" with its text intact.
    #[test]
    fn square_note_round_trip() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        let dir = std::env::temp_dir().join(format!("minipdf-sqnote-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pdf = dir.join("sqnote.pdf");
        std::fs::copy(&src, &pdf).expect("copy");
        let pdfium = minipdf::ensure_pdfium().expect("pdfium");
        {
            let doc = pdfium.load_pdf_from_file(&pdf, None).expect("load");
            let mut pg = doc.pages().get(0).expect("page");
            let annots = pg.annotations_mut();
            let before_len = annots.len();
            let mut a = annots.create_square_annotation().expect("square");
            a.set_bounds(rect_pt(10.0, 10.0, 34.0, 34.0))
                .expect("bounds");
            a.set_fill_color(PdfColor::new(255, 235, 0, 255))
                .expect("fill");
            a.set_contents("remember this").expect("contents");
            assert_eq!(annots.len(), before_len + 1);
            let tmp = dir.join("sqnote-tmp.pdf");
            doc.save_to_file(&tmp).expect("save");
            drop(doc);
            std::fs::rename(&tmp, &pdf).expect("rename");
        }
        let notes = fetch_notes(&pdf).expect("notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].label, "Note");
        assert_eq!(notes[0].text, "remember this");
        assert_eq!(note_snippet("  hello   world  "), "hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outline_of_plain_pdf_is_empty() {
        let pdf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        let items = fetch_outline(&pdf).expect("outline fetch");
        assert!(items.is_empty());
    }

    #[test]
    fn notes_of_plain_pdf_is_empty() {
        let pdf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        let items = fetch_notes(&pdf).expect("notes fetch");
        assert!(items.is_empty());
    }

    #[test]
    fn recent_list_dedupes_and_caps() {
        let mut recent = Vec::new();
        for i in 0..20 {
            push_recent(&mut recent, PathBuf::from(format!("f{i}.pdf")));
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], PathBuf::from("f19.pdf"));
        push_recent(&mut recent, PathBuf::from("f5.pdf"));
        assert_eq!(recent[0], PathBuf::from("f5.pdf"));
        assert_eq!(recent.len(), MAX_RECENT);
    }

    #[test]
    fn app_state_round_trips() {
        let mut state = AppStateFile::default();
        push_recent(&mut state.recent, PathBuf::from("a.pdf"));
        state
            .places
            .insert(PathBuf::from("a.pdf"), FilePlace { page: 3, zoom: 1.5 });
        let bytes = serde_json::to_vec(&state).expect("serialize");
        let back: AppStateFile = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back.recent, vec![PathBuf::from("a.pdf")]);
        let place = back.places.get(&PathBuf::from("a.pdf")).expect("place");
        assert_eq!((place.page, place.zoom), (3, 1.5));
    }

    #[test]
    fn copy_selection_emits_clipboard_command() {
        let mut app = MiniPdf::default();
        let mut tab = DocTab::new(PathBuf::from("t.pdf"), 1, HashMap::new());
        tab.text_cache.insert(0, char_row("Hi"));
        tab.selection = Some(TextSelection::new(0, 0, 1));
        app.tabs.push(tab);
        let ctx = egui::Context::default();
        app.copy_selection(&ctx);
        assert!(app.status.starts_with("Copied 2 chars"));
        let cmds = ctx.output_mut(|o| std::mem::take(&mut o.commands));
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                egui::OutputCommand::CopyText(s) if s == "Hi"
            )),
            "expected CopyText(Hi), got {cmds:?}"
        );
    }

    #[test]
    fn copy_without_selection_reports_gracefully() {
        let mut app = MiniPdf::default();
        app.tabs
            .push(DocTab::new(PathBuf::from("t.pdf"), 1, HashMap::new()));
        let ctx = egui::Context::default();
        app.copy_selection(&ctx);
        assert!(app.status.contains("No text selected"));
        let cmds = ctx.output_mut(|o| std::mem::take(&mut o.commands));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(_))),
            "no copy expected, got {cmds:?}"
        );
    }

    #[test]
    fn headless_open_and_render() {
        let pdf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mark_test.pdf");
        assert!(pdf.exists(), "missing {}", pdf.display());
        let mut app = MiniPdf::default();
        app.open_file(pdf);
        let ctx = egui::Context::default();
        let mut opened = false;
        for _ in 0..200 {
            app.poll_open(&ctx);
            if !app.tabs.is_empty() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(opened, "open never completed, status={}", app.status);
        assert!(app.tabs[0].pages > 0);
        let w = render_width(1.0) as i32;
        let mut got = false;
        for _ in 0..400 {
            let tex = app.render_tex(0, &ctx, 0, w, true);
            app.poll_render(&ctx);
            if tex.is_some() {
                got = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            got,
            "render never completed, status={} failed={:?}",
            app.status, app.render_failed
        );
        app.ensure_page_layers(0, 0);
        for _ in 0..200 {
            app.poll_layers(&ctx);
            if app.tabs[0].text_cache.contains_key(&0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            app.tabs[0].text_cache.contains_key(&0),
            "layers never completed, status={}",
            app.status
        );
    }
}
