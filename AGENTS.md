# AGENTS.md

A Windows-only minimal PDF viewer built in Rust with **Pdfium** (rendering) and **egui** (GUI). Single binary, no installer, no network, no background services, no registry writes.

## Commands

```sh
cargo build                 # debug build (uses build.rs to embed app icon via rc.exe if available)
cargo build --release       # release build (opt-level 2, stripped)
cargo run -- <file.pdf>     # launch viewer with one or more PDFs pre-opened as tabs
cargo test                  # 19 unit tests (mod tests in src/main.rs: search, geometry, outline/notes, render)
```

There is **no linter config, no CI**. `cargo test` covers the pure helpers; full verification is still manual: build, launch, interact.

## Runtime prerequisites

- `pdfium.dll` must be found at runtime. Lookup order (`lib.rs::ensure_pdfium`):
  1. `PDFIUM_LIB_PATH` env var (point at the dll or its directory)
  2. `pdfium.dll` next to `minipdf.exe`
  3. `pdfium.dll` in the working directory
  4. system library search
- `cjk.ttf` (in `assets/` during dev, next to the exe when deployed) is needed for CJK font fallback. If absent, Pdfium falls back to the platform default font provider (Chinese may render as boxes).
- Logs go to `%TEMP%\minipdf.log` via `log_line()` — check there for panics and startup diagnostics.

## Architecture

### Module split

- `src/lib.rs` — Pdfium binding + **custom font fallback provider** (`CjkFallbackProvider`).
- `src/main.rs` — GUI application (~4800 lines incl. `mod tests`): `MiniPdf` app state, frame layout, and background-worker orchestration (`poll_*` channel drains each frame).
- `src/types.rs` — domain types: `TabId`/`DocVersion`, channel message aliases (`RenderMessage`/`LayerMessage`/`SearchMessage`/`OutlineMessage`/`NotesMessage`), `DocTab` + cache eviction, `CharInfo`/`TextSelection`/`PageLink`/`PdfImage`/`SearchMatch`, `MarkupKind`/`MarkupUndo`, `OutlineItem`/`NoteItem`, persisted `AppStateFile`/`FilePlace`, cache-limit constants.
- `src/theme.rs` — palettes (`preview_chrome`/`preview_canvas`/`preview_selection`/`preview_link` with real dark variants), groove/separator painting, toolbar `icon_paint` + hand-drawn vector icon fallbacks.
- `src/search.rs` — `lower1`/`normalized_query`/`search_page` (cancellable, snippet-building).
- `src/pdf_util.rs` — zoom/render constants, PDF↔screen coordinate mapping, rotation stepping, outline/notes fetching.
- `src/platform.rs` — Windows FFI: print dialog + GDI printing, `shell_verb`, `shell_openas_com`, with `#[cfg(not(windows))]` stubs.

### Core types

- `MiniPdf` (`eframe::App`) — top-level app state. Holds the shared `Pdfium` instance, a `Vec<DocTab>`, sidebar mode, status string, preloaded UI icon textures, and five mpsc channel pairs (`render`/`layer`/`search`/`outline`/`notes`) plus `render_in_flight`/`render_failed` sets that cap concurrent background renders (`MAX_RENDER_THREADS`).
- `DocTab` — one open PDF: path, current page, zoom, page/aspect map, and **per-tab caches** keyed by page number:
  - `page_tex: HashMap<(i32, u32), TextureHandle>` — rendered page bitmaps keyed by `(page, width_px)`. Width is snapped to a zoom grid so re-visiting a zoom level reuses the texture.
  - `thumb_tex: HashMap<i32, TextureHandle>` — sidebar thumbnails.
  - `text_cache` / `link_cache` / `image_cache` — extracted text chars, hyperlinks, and embedded raster images per page.
  - `outline: Option<Vec<OutlineItem>>` / `notes: Option<Vec<NoteItem>>` — bookmark tree and annotation list, fetched on background threads.
  - `search_matches` / `search_by_page` / `search_hits` / `search_snippets` / `search_cursor` — search results.
  - `markup_stack: Vec<MarkupUndo>` — undo records (`page` + `before_len` + `count`).
  - `version: DocVersion` (stable `TabId` + revision) plus `search_generation` / `search_cancel` — guards so the frame loop drops stale background results.
- `CharInfo` / `TextSelection` / `PageLink` (`LinkTarget::Url`/`Page`) / `PdfImage` / `SearchMatch` / `OutlineItem` / `NoteItem` — value types for page content layers.

### Data flow

1. `main()` builds `MiniPdf`, preloads icon textures and the CJK UI font, hands off to `eframe::run_native`.
2. Each frame: `eframe::App::ui` consumes keyboard shortcuts (Ctrl+O/W/S/P/F/C, F9/F11/Esc, Ctrl+Tab, F3), processes drag-and-dropped PDFs, then lays out the **tab bar → toolbar → sidebar → page view → status bar**.
3. Page rendering goes through `MiniPdf::render_tex`: cache hit returns the texture, cache miss spawns a background worker (`spawn_worker("minipdf-render", …)`, capped by `MAX_RENDER_THREADS`, tracked in `render_in_flight`) and returns `None` so the UI shows a Loading placeholder. The worker opens the PDF itself (`ensure_pdfium` → `load_pdf_from_file` → `render_with_config` → RGBA bytes) and posts a `RenderMessage`; `poll_render` turns it into a `ColorImage` → `ctx.load_texture` on the next frame.

### Critical pattern: `with_doc`

**Every** Pdfium operation (render, extract text, save, markup, search) goes through `MiniPdf::with_doc(tab_idx, |pdfium, path| ...)`. The closure re-opens the PDF from disk each call — `pdfium-render` does not keep a long-lived `PdfDocument` in app state. This is by design: a `PdfDocument` borrows the `Pdfium` bindings and would conflict with egui's borrow-checker across frames. The tradeoff is repeated file I/O; the texture/layer caches mask this. `with_doc` uses the shared `self.pdfium` handle; background workers (`spawn_worker`) instead call `ensure_pdfium()` themselves and post typed messages back over the channels.

### Caching strategy

- Texture cache eviction is **distance-based LRU**: `evict_page_tex` keeps the `MAX_PAGE_TEX_PAGES` (16) pages nearest to `cur`, dropping all other zoom widths for those pages. Layer caches (`evict_layers`) keep `MAX_LAYER_PAGES` (32) nearest pages.
- Zoom is snapped to a 0.05 grid (`snap_zoom`) so the same `(page, width)` key hits the cache after scrolling back.
- `MAX_RENDER_WIDTH` (2600px) caps texture memory — a single page at this width is ~38 MB RGBA.

### Markup / annotation

Text markup (highlight / underline / strikeout) is implemented as **square annotations**, not native PDF markup quads. The code comment at `add_markup` explains why: this Pdfium build draws broken appearances for API-created markup quads (only the right edge renders). Squares render correctly. Highlight = translucent fill; underline/strikeout = thin opaque bar positioned at the baseline or midline.

- Markup is applied then the file is **immediately saved** (Preview-style instant persistence) via a `.minipdf-tmp` file + atomic rename.
- After markup, the affected page's texture/thumb are evicted so the next frame re-renders with the annotation visible.
- Undo removes the **last annotation on the page** (not necessarily the last one created) — `markup_stack` only tracks which pages to touch.

### Search

`run_search` bumps `search_generation`, swaps in a fresh cancellable `AtomicBool`, and walks every page's text on a background worker (`search::search_page`), posting a `SearchMessage`. Matching is case-insensitive via a **first-char-only lowercase** (`lower1`) to keep character indices aligned with the source text (full `to_lowercase` can change string length and break index mapping). Each hit also records a one-line snippet (`search_snippets`). `poll_search` applies results only if the generation still matches, so typing a new query discards the stale run. Results are capped at 500/page and 5000 total (`truncated` flag).

### Save / Save As

- `save_now`: render to `.pdf.minipdf-tmp`, then `std::fs::rename` over the original (atomic on same volume).
- `save_as`: write to chosen path, then switch the tab's `path` to the new file so subsequent saves target it.

### Windows shell integration (`src/platform.rs`)

- `shell_verb(path, "print")` / `"open"` — `ShellExecuteW` FFI for system print and default-app open.
- `shell_openas_com(path)` — "Open with" picker via `SHOpenWithDialog` (shell32), run on a dedicated **STA thread** because the dialog is modal and needs its own message pump without re-entering the egui loop. The thread is joined (blocking) so the UI freezes until the user closes the dialog.
- All Windows FFI is `#[cfg(windows)]` with `#[cfg(not(windows))]` stubs returning errors.

## Conventions and gotchas

- **Windows-only by design.** Non-Windows builds compile (via cfg stubs) but cannot render or print.
- **Dark mode toggle, light by default.** `MiniPdf::dark_mode` (toolbar sun/moon button → `toggle_dark_mode`/`apply_theme`) switches between the Catalina v2 light palette and the real dark variants in `theme.rs` (`preview_chrome`, `preview_canvas`, `preview_selection`, `preview_link` all take a `dark` param). Groove/separator painting adapts via `ui.visuals().dark_mode`. It never follows the OS setting — it's a manual in-app switch.
- **`#![windows_subsystem = "windows"]`** at the crate root — no console window on launch. This means `println!` output is invisible; use `log_line()` or `eprintln!` (only visible if launched from a terminal).
- **Borrow checker workaround in event handling.** When a keyboard shortcut needs to call a `&mut self` method, the code often clones the `egui::Context` first (`let ctx = ui.ctx().clone()`) so the borrow on `ui` is released before the mutable call. Follow this pattern when adding new shortcuts that call `&mut self` methods.
- **`eframe::App::ui` is used instead of `update`.** The app implements `fn ui(&mut self, ui: &mut egui::Ui, _frame)` rather than the standard `fn update(&mut self, ctx, frame)`. Layout is done with `Panel::top` / `SidePanel::left` / `TopBottomPanel::bottom` inside `ui`.
- **Icon embedding** (`build.rs`) probes for `rc.exe` in three locations (PATH, Windows SDK env vars, `C:\Program Files (x86)\Windows Kits\10\bin`). If not found, the build succeeds without an embedded icon (just a `cargo:warning`). The `cargo:rustc-link-arg` value is printed **without quotes** — embedded quotes become part of the filename and cause LNK1104.
- **Embedded assets** are loaded via `include_bytes!` and decoded synchronously with the `image` crate into `egui::TextureHandle` (`load_embedded_tex`). This avoids egui's async bytes-loader, which would briefly show the ⚠ placeholder.
- **CJK UI font** is loaded at startup from `cjk.ttf` (`setup_cjk_ui_font`) and registered for both `Proportional` and `Monospace` families. If missing, the egui UI itself can't render CJK text (independent of the Pdfium font fallback in `lib.rs`).

## Key constants

| Constant | Value | Purpose |
|---|---|---|
| `THUMB_WIDTH` | 132 | Sidebar thumbnail width in px |
| `RENDER_BASE_WIDTH` | 1100 | Base render width at zoom 1.0 (`pdf_util.rs`) |
| `MAX_RENDER_WIDTH` | 2600 | Hard cap on texture width (~38 MB RGBA per page, `pdf_util.rs`) |
| `MAX_PAGE_TEX_PAGES` | 16 | Max pages kept in the texture cache (`types.rs`) |
| `MAX_LAYER_PAGES` | 32 | Max pages kept in text/link/image caches (`types.rs`) |
| `ZOOM_STEP` | 0.05 | Zoom grid step (range 0.2–4.0, `pdf_util.rs`) |
| `MAX_MARK_CHARS` | 3000 | Max chars in a single markup annotation (`main.rs::add_markup`) |
| Search caps | 500/page, 5000 total | Hard limits on search results (`search.rs`) |
