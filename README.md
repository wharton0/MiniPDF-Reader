# MiniPDF-Reader

A minimal PDF viewer written in **Rust** using **Pdfium** and **egui**. Designed for Windows only.

## Philosophy

- **Pure PDF viewing** — open, read, navigate, search, highlight, print, save
- **No background services, no network, no registry writes**
- **Windows-only** — relies on `pdfium.dll`, Windows Subsystem UI
- **Lightweight** — single executable, no installer beyond copying the .exe
- **Catalina-inspired UI** — restrained grays, system blue accent, translucent controls

## Features

- Open PDF files (File → Open or drag-and-drop)
- Multi-tab browsing
- Text selection & copy
- Search with per-occurrence highlighting (Enter/Shift+Enter navigation)
- Print (whole document or current page) via system print dialog
- Fullscreen mode (F11/Esc) hiding toolbar/sidebar/status
- Annotation/markup: highlight, underline, strike-out with color picker (yellow/green/blue/pink)
- Undo/redo per tab for markup
- Save (`Ctrl+S`) and Save As
- Color scheme: forced light theme, Catalina v2 palette
- Window controls: Close (#FF5F57), Minimize (#FFBD2E), Zoom (#28C840)

## Screenshot

![MiniPDF-Reader UI](screenshot.png)

## Build

```powershell
cargo build --release
```

The release executable (`target/release/minipdf.exe`) can be placed in any directory or copied to `C:\Users\<user>\AppData\Local\MiniPDF\` for auto-sync behavior.

## License

MIT OR Apache-2.0