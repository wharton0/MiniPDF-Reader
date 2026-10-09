//! Core domain types and data structures for MiniPDF.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc};
use eframe::egui;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocVersion {
    pub id: TabId,
    pub revision: u64,
}

pub type RenderKey = (DocVersion, i32, u32);
pub type LayerKey = (DocVersion, i32);
pub type RenderData = (Vec<u8>, usize, usize);
pub type LayerData = (Vec<CharInfo>, Vec<PageLink>, Vec<PdfImage>, (f32, f32));
pub type RenderMessage = (RenderKey, Result<RenderData, String>);
pub type LayerMessage = (LayerKey, Result<LayerData, String>);
pub type SearchMessage = (DocVersion, u64, Result<SearchOutput, String>);
pub type OutlineMessage = (DocVersion, Result<Vec<OutlineItem>, String>);
pub type NotesMessage = (DocVersion, Result<Vec<NoteItem>, String>);

#[derive(Default)]
pub struct SearchOutput {
    pub matches: Vec<SearchMatch>,
    pub snippets: Vec<String>,
    pub truncated: bool,
}

/// One document-outline (bookmark) entry: Preview's Contents sidebar.
#[derive(Clone)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<i32>,
    pub depth: usize,
}

/// One annotation entry for the Notes sidebar (Preview Highlights & Notes).
#[derive(Clone)]
pub struct NoteItem {
    pub page: i32,
    pub index: usize,
    pub label: &'static str,
    pub text: String,
}

#[derive(Clone, Copy)]
pub struct MarkupUndo {
    pub page: i32,
    pub before_len: usize,
    pub count: usize,
}

impl MarkupUndo {
    pub fn valid(&self, current_len: usize) -> bool {
        self.count > 0 && current_len == self.before_len + self.count
    }
}

/// Per-file reading position persisted across launches.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Default)]
pub struct FilePlace {
    #[serde(default)]
    pub page: i32,
    #[serde(default)]
    pub zoom: f32,
}

/// Whole-app persisted state: recent files + reading positions.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct AppStateFile {
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    #[serde(default)]
    pub places: HashMap<PathBuf, FilePlace>,
}

#[derive(Clone, Copy)]
pub struct CharInfo {
    pub ch: char,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

impl CharInfo {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
    pub fn center(&self) -> (f32, f32) {
        (
            (self.left + self.right) * 0.5,
            (self.bottom + self.top) * 0.5,
        )
    }
}

#[derive(Clone, Copy)]
pub struct TextSelection {
    pub page: i32,
    pub start: usize,
    pub end: usize, // inclusive, normalized start <= end
}

impl TextSelection {
    pub fn new(page: i32, a: usize, b: usize) -> Self {
        Self {
            page,
            start: a.min(b),
            end: a.max(b),
        }
    }
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }
}

/// Text markup kind (mirrors Preview: highlight / underline / strike out).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MarkupKind {
    Highlight,
    Underline,
    Strikeout,
}

impl MarkupKind {
    pub fn name(self) -> &'static str {
        match self {
            MarkupKind::Highlight => "Highlight",
            MarkupKind::Underline => "Underline",
            MarkupKind::Strikeout => "Strikethrough",
        }
    }
}

/// Pending sticky-note creation (Preview: note).
pub struct NoteDraft {
    pub tab: usize,
    pub page: i32,
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub fresh: bool,
}

/// Clickable link on a page: bounds in PDF points + target.
#[derive(Clone)]
pub enum LinkTarget {
    Url(String),
    Page(i32),
}

#[derive(Clone)]
pub struct PageLink {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
    pub target: LinkTarget,
}

impl PageLink {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
    pub fn label(&self) -> String {
        match &self.target {
            LinkTarget::Url(u) => u.clone(),
            LinkTarget::Page(p) => format!("Go to page {}", p + 1),
        }
    }
}

/// One occurrence of the search query: char range [start, end] on a page.
#[derive(Clone, Copy)]
pub struct SearchMatch {
    pub page: i32,
    pub start: usize,
    pub end: usize,
}

/// One embedded raster image on a page: object index + PDF-space bounds.
#[derive(Clone)]
pub struct PdfImage {
    pub obj: usize,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

impl PdfImage {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.bottom && y <= self.top
    }
    pub fn area(&self) -> f32 {
        (self.right - self.left) * (self.top - self.bottom)
    }
}

pub struct DocTab {
    pub version: DocVersion,
    pub search_generation: u64,
    pub search_cancel: Arc<AtomicBool>,
    pub search_snippets: Vec<String>,
    pub path: PathBuf,
    pub pages: i32,
    pub cur: i32,
    pub zoom: f32,
    pub aspects: HashMap<i32, (f32, f32)>,
    pub scroll_target: Option<i32>,
    pub scroll_to_match: bool,
    pub page_tex: HashMap<(i32, u32), egui::TextureHandle>,
    pub thumb_tex: HashMap<i32, egui::TextureHandle>,
    pub search_text: String,
    pub search_query: String,
    pub search_matches: Vec<SearchMatch>,
    pub search_by_page: HashMap<i32, Vec<usize>>,
    pub search_cursor: usize,
    pub search_hits: Vec<i32>,
    pub text_cache: HashMap<i32, Vec<CharInfo>>,
    pub link_cache: HashMap<i32, Vec<PageLink>>,
    pub image_cache: HashMap<i32, Vec<PdfImage>>,
    pub outline: Option<Vec<OutlineItem>>,
    pub notes: Option<Vec<NoteItem>>,
    pub selection: Option<TextSelection>,
    pub highlight_rgb: (u8, u8, u8),
    pub markup_stack: Vec<MarkupUndo>,
    pub drag_anchor: Option<(i32, usize)>,
    pub page_box: String,
    pub page_box_cur: i32,
    pub context_image: Option<(i32, PdfImage)>,
    pub context_point: Option<(i32, f32, f32)>,
}

pub const MAX_PAGE_TEX_PAGES: usize = 16;
pub const MAX_LAYER_PAGES: usize = 32;

impl DocTab {
    pub fn new(path: PathBuf, pages: i32, aspects: HashMap<i32, (f32, f32)>) -> Self {
        static NEXT_TAB_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            version: DocVersion {
                id: TabId(NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
                revision: 0,
            },
            search_generation: 0,
            search_cancel: Arc::new(AtomicBool::new(false)),
            search_snippets: Vec::new(),
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
            search_by_page: HashMap::new(),
            search_cursor: 0,
            search_hits: Vec::new(),
            text_cache: HashMap::new(),
            link_cache: HashMap::new(),
            image_cache: HashMap::new(),
            outline: None,
            notes: None,
            highlight_rgb: (255, 255, 0),
            markup_stack: Vec::new(),
            selection: None,
            drag_anchor: None,
            page_box: String::new(),
            page_box_cur: -1,
            context_image: None,
            context_point: None,
        }
    }

    pub fn title(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.to_string_lossy().to_string())
    }

    pub fn evict_page_tex(&mut self) {
        if self.page_tex.len() <= MAX_PAGE_TEX_PAGES * 2 {
            return;
        }
        let cur = self.cur;
        let mut dists: Vec<(i32, i32)> = self
            .page_tex
            .keys()
            .map(|(p, _)| ((p - cur).abs(), *p))
            .collect();
        dists.sort_unstable();
        let keep: std::collections::HashSet<i32> = dists
            .into_iter()
            .take(MAX_PAGE_TEX_PAGES)
            .map(|(_, p)| p)
            .collect();
        self.page_tex.retain(|(p, _), _| keep.contains(p));
    }

    pub fn evict_layers(&mut self) {
        fn prune<V>(map: &mut HashMap<i32, V>, cur: i32) {
            if map.len() <= MAX_LAYER_PAGES {
                return;
            }
            let mut dists: Vec<i32> = map.keys().cloned().collect();
            dists.sort_unstable_by_key(|p| (p - cur).abs());
            let keep: std::collections::HashSet<i32> =
                dists.into_iter().take(MAX_LAYER_PAGES).collect();
            map.retain(|p, _| keep.contains(p));
        }
        let cur = self.cur;
        prune(&mut self.text_cache, cur);
        prune(&mut self.link_cache, cur);
        prune(&mut self.image_cache, cur);
    }

    pub fn evict_stale_zoom(&mut self, current_width: u32) {
        self.page_tex.retain(|(_, w), _| *w == current_width);
    }

    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let chars = self.text_cache.get(&sel.page)?;
        if chars.is_empty() || sel.end >= chars.len() {
            return None;
        }
        Some(chars[sel.start..=sel.end].iter().map(|c| c.ch).collect())
    }
}

