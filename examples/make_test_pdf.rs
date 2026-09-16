// Writes a minimal one-page test PDF (Helvetica, no embedded font needed).
// Run: cargo run --example make_test_pdf
// Output: target/mark_test.pdf

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let content = "BT /F1 24 Tf 72 720 Td 16 TL (Hello world, this is a highlight test.) Tj ET\n\
                   BT /F1 18 Tf 72 680 Td 14 TL (Second line for underline and strikeout.) Tj ET\n";
    let mut pdf: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let mut obj = |pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &str| {
        offsets.push(pdf.len());
        pdf.extend_from_slice(body.as_bytes());
    };
    pdf.extend_from_slice(b"%PDF-1.4\n");
    obj(&mut pdf, &mut offsets, "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    obj(&mut pdf, &mut offsets, "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    obj(
        &mut pdf,
        &mut offsets,
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
    );
    offsets.push(pdf.len());
    let s4 = format!(
        "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        content.len(),
        content
    );
    pdf.extend_from_slice(s4.as_bytes());
    obj(
        &mut pdf,
        &mut offsets,
        "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    );
    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    std::fs::create_dir_all("target")?;
    std::fs::write("target/mark_test.pdf", &pdf)?;
    println!("wrote target/mark_test.pdf ({} bytes)", pdf.len());
    Ok(())
}
