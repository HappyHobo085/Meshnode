//! Inline-SVG QR codes: one `<path>`, no scripts, no external references.

use qrcode::{Color, EcLevel, QrCode};

const QUIET: usize = 4;

/// Render `text` as an SVG QR code (error correction M, quiet zone 4 modules).
/// Returns an empty string if the text cannot be encoded.
pub fn qr_svg(text: &str) -> String {
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) else {
        return String::new();
    };
    let w = code.width();
    let colors = code.to_colors();
    let size = w + 2 * QUIET;
    let mut d = String::new();
    for y in 0..w {
        let mut x = 0;
        while x < w {
            if colors[y * w + x] == Color::Dark {
                let start = x;
                while x < w && colors[y * w + x] == Color::Dark {
                    x += 1;
                }
                let run = x - start;
                d.push_str(&format!("M{} {}h{run}v1h-{run}z", start + QUIET, y + QUIET));
            } else {
                x += 1;
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\"><path d=\"{d}\" fill=\"currentColor\"/></svg>"
    )
}
