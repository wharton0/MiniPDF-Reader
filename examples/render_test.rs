// 无头验证:绑定 Pdfium(含中文字体回退)并把 PDF 第 1 页渲染成 PNG。
use minipdf::ensure_pdfium;
use pdfium_render::prelude::*;

fn main() {
    let pdfium = ensure_pdfium().expect("pdfium init failed");
    let pdf = std::env::args().nth(1).expect("usage: render_test <file.pdf> [page]");
    let idx: i32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let doc = pdfium.load_pdf_from_file(&pdf, None).expect("open failed");
    let pages = doc.pages();
    println!("pages = {}", pages.len());
    let page = pages.get(idx).expect("page");
    let bmp = page
        .render_with_config(&PdfRenderConfig::new().set_target_width(800))
        .expect("render failed");
    let img = bmp.as_image().expect("as_image failed");
    img.save("render_test_out.png").expect("save failed");
    let t = page.text().expect("text failed");
    println!("text = {:?}", t.all().chars().take(60).collect::<String>());
    println!("RENDER OK");
}
