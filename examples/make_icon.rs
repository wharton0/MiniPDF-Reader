// Generates assets/logo-256.png and assets/minipdf.ico from a procedural design.
// No extra dependencies: drawing is done pixel by pixel with the `image` crate,
// the .ico container is packed by hand (PNG-compressed entries).
//
// Run: cargo run --example make_icon

use image::{ImageFormat, Rgba, RgbaImage};
use std::io::Cursor;

const SS: u32 = 4; // supersample factor for smooth edges

fn over(dst: &mut Rgba<u8>, src: Rgba<u8>) {
    let sa = src[3] as u32;
    if sa == 0 {
        return;
    }
    if sa == 255 {
        *dst = src;
        return;
    }
    let da = dst[3] as u32;
    let out_a = sa + da * (255 - sa) / 255;
    if out_a == 0 {
        *dst = Rgba([0, 0, 0, 0]);
        return;
    }
    for k in 0..3 {
        let v = (src[k] as u32 * sa + dst[k] as u32 * da * (255 - sa) / 255) / out_a;
        dst[k] = v.min(255) as u8;
    }
    dst[3] = out_a.min(255) as u8;
}

fn plot(img: &mut RgbaImage, x: i32, y: i32, c: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        let p = img.get_pixel_mut(x as u32, y as u32);
        over(p, c);
    }
}

/// Filled rounded rect in design pixels (scaled by SS internally).
fn rrect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: Rgba<u8>) {
    let (x0, y0, x1, y1, r) = (x0 * SS as i32, y0 * SS as i32, x1 * SS as i32, y1 * SS as i32, r * SS as i32);
    let r = r.min((x1 - x0) / 2).min((y1 - y0) / 2).max(0);
    let rf = r as f32;
    for y in y0..y1 {
        for x in x0..x1 {
            // distance from point to the inner (unrounded) rect
            let nx = if x < x0 + r {
                x0 + r - x
            } else if x >= x1 - r {
                x - (x1 - 1 - r)
            } else {
                0
            };
            let ny = if y < y0 + r {
                y0 + r - y
            } else if y >= y1 - r {
                y - (y1 - 1 - r)
            } else {
                0
            };
            let inside = if nx == 0 || ny == 0 {
                true
            } else {
                (nx * nx + ny * ny) as f32 <= rf * rf
            };
            if inside {
                plot(img, x, y, c);
            }
        }
    }
}

/// Filled circle in design pixels.
fn circle(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, c: Rgba<u8>) {
    let (cx, cy, r) = (cx * SS as i32, cy * SS as i32, r * SS as i32);
    for y in (cy - r)..(cy + r) {
        for x in (cx - r)..(cx + r) {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy <= r * r {
                plot(img, x, y, c);
            }
        }
    }
}

/// Thick segment drawn as a row of stamps (for the magnifier handle).
fn stamp_segment(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: Rgba<u8>) {
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * SS as i32).max(1);
    for k in 0..=steps {
        let t = k as f32 / steps as f32;
        let x = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
        let y = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
        circle(img, x, y, r, c);
    }
}

/// Three-stop diagonal gradient helper.
fn lerp3(c0: [u8; 3], c1: [u8; 3], c2: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let (a, b, tt) = if t < 0.5 {
        (c0, c1, t * 2.0)
    } else {
        (c1, c2, (t - 0.5) * 2.0)
    };
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * tt) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * tt) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * tt) as u8,
    ]
}

fn draw_logo(px: u32) -> RgbaImage {
    let s = px * SS;
    let mut img = RgbaImage::new(s, s);
    // soft shadow under the card
    for (dy, a) in [(14, 26u8), (10, 30), (6, 36)] {
        rrect(&mut img, 28, 28 + dy, 228, 228 + dy, 52, Rgba([20, 25, 35, a]));
    }
    // photo card: diagonal cyan -> blue -> pink gradient (original art,
    // Preview-vibe background)
    {
        const C0: [u8; 3] = [105, 205, 255];
        const C1: [u8; 3] = [70, 140, 250];
        const C2: [u8; 3] = [248, 150, 215];
        let ss = SS as i32;
        let (x0, y0, x1, y1, rr) = (28 * ss, 28 * ss, 228 * ss, 228 * ss, 52 * ss);
        let span = ((x1 - x0) + (y1 - y0)) as f32;
        for y in y0..y1 {
            for x in x0..x1 {
                let nx = if x < x0 + rr {
                    x0 + rr - x
                } else if x >= x1 - rr {
                    x - (x1 - 1 - rr)
                } else {
                    0
                };
                let ny = if y < y0 + rr {
                    y0 + rr - y
                } else if y >= y1 - rr {
                    y - (y1 - 1 - rr)
                } else {
                    0
                };
                let inside = if nx == 0 || ny == 0 {
                    true
                } else {
                    (nx * nx + ny * ny) as f32 <= (rr * rr) as f32
                };
                if inside {
                    let t = ((x - x0) + (y - y0)) as f32 / span;
                    let c = lerp3(C0, C1, C2, t);
                    plot(&mut img, x, y, Rgba([c[0], c[1], c[2], 255]));
                }
            }
        }
    }
    // soft top-left sheen (fully inside the card: no spill past the corner)
    circle(&mut img, 110, 100, 40, Rgba([255, 255, 255, 40]));
    // Preview-style magnifier over the page (bottom-right)
    const RING: Rgba<u8> = Rgba([35, 40, 48, 255]);
    const GLASS: Rgba<u8> = Rgba([215, 231, 248, 255]);
    const GLASS_DEEP: Rgba<u8> = Rgba([178, 205, 236, 255]);
    circle(&mut img, 168, 178, 56, RING);
    circle(&mut img, 168, 178, 45, GLASS);
    // glass shading: deeper tint at the bottom (supsersampled space)
    {
        let (gcx, gcy, gr) = (168 * SS as i32, 178 * SS as i32, 45 * SS as i32);
        for y in gcy..(gcy + gr) {
            let dy = y - gcy;
            let half = ((gr * gr - dy * dy).max(0) as f32).sqrt() as i32;
            let t = dy as f32 / gr as f32;
            let a = (t * t * 110.0) as u8;
            for x in (gcx - half)..(gcx + half) {
                plot(
                    &mut img,
                    x,
                    y,
                    Rgba([GLASS_DEEP[0], GLASS_DEEP[1], GLASS_DEEP[2], a]),
                );
            }
        }
    }
    // glass highlight
    circle(&mut img, 153, 163, 11, Rgba([255, 255, 255, 235]));
    // handle down-right
    stamp_segment(&mut img, 202, 212, 240, 250, 11, RING);
    circle(&mut img, 240, 250, 11, RING);
    img
}

fn png_bytes(img: &RgbaImage) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img.clone()).write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

fn write_ico(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len() as u32;
    for (size, data) in entries {
        let w = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(w);
        out.push(w);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in entries {
        out.extend_from_slice(data);
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let master = draw_logo(256);
    // downscale supersampled art to crisp 256px
    let small = image::imageops::resize(&master, 256, 256, image::imageops::FilterType::Lanczos3);
    small.save("assets/logo-256.png")?;

    let mut entries = Vec::new();
    for size in [256u32, 64, 48, 32, 16] {
        let img = if size == 256 {
            small.clone()
        } else {
            image::imageops::resize(&small, size, size, image::imageops::FilterType::Lanczos3)
        };
        entries.push((size, png_bytes(&img)?));
    }
    std::fs::write("assets/minipdf.ico", write_ico(&entries))?;
    println!("wrote assets/logo-256.png and assets/minipdf.ico");
    Ok(())
}
