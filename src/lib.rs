//! MiniPDF 共享库:Pdfium 加载 + 中文字体回退。
//!
//! 为解决两类中文方块问题:
//! 1. 未嵌入 CJK 字体的 PDF(尤其是 AdobeSongStd/STSong 等专有名字)——
//!    用微软雅黑数据做自定义字体回退;
//! 2. 其它非标准西文字体——用 Segoe UI 数据兜底。14 种 PDF 标准字体走
//!    Pdfium 内建数据,不受影响。

use std::path::PathBuf;

use pdfium_render::prelude::*;

/// exe 同目录的 pdfium.dll 优先,其次当前目录,最后系统库。
fn dll_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join(Pdfium::pdfium_platform_library_name()));
        }
    }
    v.push(PathBuf::from(Pdfium::pdfium_platform_library_name()));
    v
}

/// 中文字体文件(cjk.ttf)查找顺序:exe 同目录 > 当前目录 > 开发期 assets。
pub fn find_cjk_font_path() -> Option<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("cjk.ttf"));
        }
    }
    v.push(PathBuf::from("cjk.ttf"));
    v.push(PathBuf::from("assets/cjk.ttf"));
    v.into_iter().find(|p| p.exists())
}

fn find_latin_font_bytes() -> Option<Vec<u8>> {
    // Segoe UI 覆盖拉丁/希腊/西里尔/阿拉伯/希伯来/泰文/越南文等,体积小。
    for p in [
        PathBuf::from("C:/Windows/Fonts/segoeui.ttf"),
        PathBuf::from("C:/Windows/Fonts/arial.ttf"),
    ] {
        if let Ok(b) = std::fs::read(&p) {
            return Some(b);
        }
    }
    None
}

/// PDF 14 种标准字体:走 Pdfium 内建数据,provider 直接放行(None)。
fn is_standard_14(face: &str) -> bool {
    let n: String = face
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    const STD: &[&str] = &[
        "courier",
        "courierbold",
        "courieroblique",
        "courierboldoblique",
        "helvetica",
        "helveticabold",
        "helveticaoblique",
        "helveticaboldoblique",
        "timesroman",
        "timesbold",
        "timesitalic",
        "timesbolditalic",
        "symbol",
        "zapfdingbats",
    ];
    STD.contains(&n.as_str())
}

fn is_adobe_cjk_name(face: &str) -> bool {
    let l = face.to_lowercase();
    [
        "adobesongstd",
        "adobemyungjostd",
        "adobemingstd",
        "kozmin",
        "kozgo",
        "stsong",
        "stson",
        "msungstd",
        "msunglight",
        "song",
        "simsun",
        "nsimsun",
        "simhei",
        "kaiti",
        "fangsong",
        "yahei",
        "microsoftyahei",
        "pingfang",
        "hiragino",
        "meiryo",
        "msgothic",
        "malgungothic",
        "notosanscjk",
        "notoserifcjk",
        "sourcehanserif",
        "sourcehansans",
    ]
    .iter()
    .any(|k| l.contains(k))
}

pub struct CjkFallbackProvider {
    next_id: u64,
    cjk: Vec<u8>,
    latin: Option<Vec<u8>>,
}

impl CjkFallbackProvider {
    pub fn new(cjk: Vec<u8>, latin: Option<Vec<u8>>) -> Self {
        Self { next_id: 1, cjk, latin }
    }
}

impl PdfiumCustomFontProvider for CjkFallbackProvider {
    fn provide(
        &mut self,
        request: PdfiumCustomFontProviderRequest,
    ) -> Option<PdfiumCustomFontProviderResponse> {
        if is_standard_14(&request.font_face) {
            return None; // 内建数据,不干预
        }
        let cjk = matches!(
            request.character_set,
            PdfFontCharacterSet::ChineseGb2312
                | PdfFontCharacterSet::ChineseBig5
                | PdfFontCharacterSet::JapaneseShiftJis
                | PdfFontCharacterSet::KoreanHangul
        ) || is_adobe_cjk_name(&request.font_face);
        let data = if cjk {
            Some(self.cjk.clone())
        } else {
            self.latin.clone()
        };
        data.map(|d| {
            let id = self.next_id;
            self.next_id += 1;
            PdfiumCustomFontProviderResponse {
                id,
                font_face: request.font_face.clone(),
                character_set: request.character_set,
                data: d,
            }
        })
    }
}

/// 绑定 Pdfium 并装好字体回退。cjk.ttf 缺失时退化为系统默认 provider。
pub fn ensure_pdfium() -> Result<Pdfium, String> {
    if let Ok(p) = std::env::var("PDFIUM_LIB_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            let dir = p
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            return finish_bind(
                Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))
                    .map_err(|e| format!("Failed to bind {}: {e}", p.display()))?,
            );
        }
    }
    for p in dll_candidates() {
        if p.exists() {
            let dir = p
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            match Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)) {
                Ok(b) => return finish_bind(b),
                Err(e) => return Err(format!("Failed to bind {}: {e}", p.display())),
            }
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(b) => finish_bind(b),
        Err(_) => Err("pdfium.dll not found. Put it next to minipdf.exe.".to_owned()),
    }
}

fn finish_bind(
    bindings: Box<dyn PdfiumLibraryBindings>,
) -> Result<Pdfium, String> {
    let mut pdfium = Pdfium::new(bindings);
    match find_cjk_font_path()
        .and_then(|p| std::fs::read(p).ok())
    {
        Some(cjk) => {
            let latin = find_latin_font_bytes();
            pdfium.set_custom_font_provider(Box::new(CjkFallbackProvider::new(cjk, latin)));
            Ok(pdfium)
        }
        None => {
            // 没有 cjk.ttf:至少打开系统 provider,能显示已安装的中文字体。
            let _ = pdfium.use_platform_default_font_provider();
            Ok(pdfium)
        }
    }
}
