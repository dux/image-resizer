// Spinner and error placeholders (byte-for-byte the Go markup) and SVG source sniffing.

fn placeholder_size(width: i32, height: i32) -> (i32, i32) {
    match (width, height) {
        (0, 0) => (400, 300),
        (0, h) => (h, h),
        (w, 0) => (w, w),
        (w, h) => (w, h),
    }
}

/// White box, #ddd border, rotating arc; the browser re-fetches after 10s.
pub fn spinner(width: i32, height: i32) -> String {
    let (width, height) = placeholder_size(width, height);
    let (cx, cy) = (width / 2, height / 2);
    let r = (width.min(height) / 6).clamp(6, 20);
    // Go's literal, kept so the dash pattern stays byte-identical
    #[allow(clippy::approx_constant)]
    let circumference = 2.0 * 3.14159 * f64::from(r);
    let (dash, gap) = (circumference * 0.25, circumference * 0.75);
    format!(
        r##"<svg width="{width}" height="{height}" xmlns="http://www.w3.org/2000/svg">
  <rect x="0.5" y="0.5" width="{w1}" height="{h1}" fill="white" stroke="#ddd" stroke-width="1" rx="6" ry="6"/>
  <circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="#eee" stroke-width="2.5"/>
  <circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="#999" stroke-width="2.5" stroke-dasharray="{dash:.1} {gap:.1}" stroke-linecap="round">
    <animateTransform attributeName="transform" type="rotate" dur="0.75s" repeatCount="indefinite" from="0 {cx} {cy}" to="360 {cx} {cy}"/>
  </circle>
</svg>"##,
        w1 = width - 1,
        h1 = height - 1,
    )
}

/// Light red box with an exclamation mark.
pub fn error(width: i32, height: i32) -> String {
    let (width, height) = placeholder_size(width, height);
    let (cx, cy) = (width / 2, height / 2);
    let r = (width.min(height) / 5).clamp(8, 24);
    format!(
        r##"<svg width="{width}" height="{height}" xmlns="http://www.w3.org/2000/svg">
  <rect x="0.5" y="0.5" width="{w1}" height="{h1}" fill="#fff8f8" stroke="#f0c0c0" stroke-width="1" rx="6" ry="6"/>
  <circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="#daa" stroke-width="1.5"/>
  <line x1="{cx}" y1="{y1}" x2="{cx}" y2="{y2}" stroke="#daa" stroke-width="2" stroke-linecap="round"/>
  <circle cx="{cx}" cy="{y3}" r="1.5" fill="#daa"/>
</svg>"##,
        w1 = width - 1,
        h1 = height - 1,
        y1 = cy - r * 2 / 3,
        y2 = cy + r / 6,
        y3 = cy + r / 3 + 2,
    )
}

/// SVG files are passed through. Raster magic bytes win over the URL and Content-Type,
/// so a Commons thumb like `Flag.svg.png` is resized.
pub fn is_svg_source(content_type: &str, src: &str, body: &[u8]) -> bool {
    if is_raster_magic(body) {
        return false;
    }
    if is_svg_bytes(body) {
        return true;
    }
    let ct = content_type.to_lowercase();
    if ct.contains("svg") && !ct.contains("png") && !ct.contains("jpeg") && !ct.contains("jpg") {
        return true;
    }
    url_path_is_svg(src)
}

pub fn is_raster_magic(b: &[u8]) -> bool {
    if b.len() < 12 {
        return false;
    }
    b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(b"\x89PNG")
        || b.starts_with(b"GIF87a")
        || b.starts_with(b"GIF89a")
        || (b.starts_with(b"RIFF") && &b[8..12] == b"WEBP")
        || b.starts_with(b"BM")
        || &b[4..8] == b"ftyp"
}

fn is_svg_bytes(b: &[u8]) -> bool {
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    let text = String::from_utf8_lossy(b);
    let lower = text.trim().to_lowercase();
    lower.starts_with("<svg") || (lower.starts_with("<?xml") && lower.contains("<svg"))
}

fn url_path_is_svg(src: &str) -> bool {
    let path = match url::Url::parse(src) {
        Ok(u) if !u.path().is_empty() => u.path().to_string(),
        _ => src.split(['?', '#']).next().unwrap_or_default().to_string(),
    };
    path.to_lowercase().ends_with(".svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_markup_matches_go() {
        assert_eq!(
            spinner(100, 0),
            r##"<svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
  <rect x="0.5" y="0.5" width="99" height="99" fill="white" stroke="#ddd" stroke-width="1" rx="6" ry="6"/>
  <circle cx="50" cy="50" r="16" fill="none" stroke="#eee" stroke-width="2.5"/>
  <circle cx="50" cy="50" r="16" fill="none" stroke="#999" stroke-width="2.5" stroke-dasharray="25.1 75.4" stroke-linecap="round">
    <animateTransform attributeName="transform" type="rotate" dur="0.75s" repeatCount="indefinite" from="0 50 50" to="360 50 50"/>
  </circle>
</svg>"##
        );
        assert!(spinner(0, 0).starts_with(r#"<svg width="400" height="300""#));
        assert!(spinner(10, 10).contains(r#"r="6""#));
        assert!(spinner(1000, 1000).contains(r#"r="20""#));
    }

    #[test]
    fn error_markup_matches_go() {
        assert_eq!(
            error(200, 100),
            r##"<svg width="200" height="100" xmlns="http://www.w3.org/2000/svg">
  <rect x="0.5" y="0.5" width="199" height="99" fill="#fff8f8" stroke="#f0c0c0" stroke-width="1" rx="6" ry="6"/>
  <circle cx="100" cy="50" r="20" fill="none" stroke="#daa" stroke-width="1.5"/>
  <line x1="100" y1="37" x2="100" y2="53" stroke="#daa" stroke-width="2" stroke-linecap="round"/>
  <circle cx="100" cy="58" r="1.5" fill="#daa"/>
</svg>"##
        );
    }

    #[test]
    fn svg_detection() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        assert!(!is_svg_source(
            "image/svg+xml",
            "https://x.com/Flag.svg.png",
            png
        ));
        assert!(!is_svg_source("", "https://x.com/a.svg", png));
        assert!(is_svg_source(
            "",
            "https://x.com/a",
            b"<svg xmlns=\"\"></svg>"
        ));
        assert!(is_svg_source(
            "",
            "https://x.com/a",
            b"\xEF\xBB\xBF  <?xml version=\"1.0\"?><svg></svg>"
        ));
        assert!(is_svg_source(
            "image/svg+xml",
            "https://x.com/a",
            b"garbage-but-long"
        ));
        assert!(!is_svg_source(
            "image/svg+xml; png",
            "https://x.com/a",
            b"garbage-but-long"
        ));
        assert!(is_svg_source(
            "",
            "https://x.com/logo.SVG?x=1",
            b"garbage-but-long"
        ));
        assert!(!is_svg_source(
            "text/html",
            "https://x.com/a.svg.png",
            b"garbage-but-long"
        ));
    }
}
