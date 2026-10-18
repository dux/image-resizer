// Resize URL grammar, kept identical to the Go service: /r/{params}[.ext]?{src}, /r.{ext}?{src}, /resize?src=..

use std::collections::HashMap;

use crate::step;

/// Output formats requestable via a path extension or `f=`; `glb` applies to STEP sources only.
pub const FORCED_FORMATS: [&str; 7] = ["png", "jpg", "jpeg", "webp", "avif", "gif", "glb"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResizeParams {
    pub width: i32,
    pub height: i32,
    pub crop: bool,
    /// Size part of the cache key, e.g. `w_300`, `c_300x200`; empty when no size was asked.
    pub cache_key: String,
    /// Forced output format, empty means Accept negotiation; `glb` is STEP to GLB.
    pub format: String,
    pub cam_dir: String,
    pub cam_key: String,
    pub bg_transparent: bool,
    pub bg_key: String,
}

/// Strips a trailing `.ext` when ext is a forced format, so dotted cam vectors pass untouched.
pub fn split_format_ext(path: &str) -> (&str, Option<String>) {
    if let Some(idx) = path.rfind('.') {
        let ext = path[idx + 1..].to_lowercase();
        if FORCED_FORMATS.contains(&ext.as_str()) {
            return (&path[..idx], Some(ext));
        }
    }
    (path, None)
}

fn atoi(value: &str) -> Option<i32> {
    value.parse().ok()
}

fn dimensions(
    value: &str,
    format_err: &str,
    width_err: &str,
    height_err: &str,
) -> Result<(i32, i32), String> {
    let parts: Vec<&str> = value.split('x').collect();
    if parts.len() != 2 {
        return Err(format_err.into());
    }
    let width = atoi(parts[0]).filter(|w| *w > 0).ok_or(width_err)?;
    let height = atoi(parts[1]).filter(|h| *h > 0).ok_or(height_err)?;
    Ok((width, height))
}

/// Parses resize values from a lookup that answers like Go's `url.Values.Get` (missing and empty are the same).
pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<ResizeParams, String> {
    let get = |key: &str| get(key).filter(|v| !v.is_empty());
    let mut params = ResizeParams::default();

    if let Some(f) = get("f").map(|f| f.to_lowercase()) {
        if !FORCED_FORMATS.contains(&f.as_str()) {
            return Err(format!("invalid f parameter '{f}'"));
        }
        params.format = if f == "jpeg" { "jpg".into() } else { f };
    }
    if let Some(to) = get("to") {
        if to != "glb" {
            return Err(format!(
                "invalid to parameter '{to}', only 'glb' is supported"
            ));
        }
        params.format = "glb".into();
    }
    if let Some(cam) = get("cam") {
        let (dir, key) = step::parse_cam(&cam)?;
        params.cam_dir = dir;
        params.cam_key = key;
    }
    let (transparent, bg_key) = step::parse_bg(&get("bg").unwrap_or_default())?;
    params.bg_transparent = transparent;
    params.bg_key = bg_key;

    if let Some(crop) = get("c").or_else(|| get("crop")) {
        params.crop = true;
        if crop.contains('x') {
            let (w, h) = dimensions(
                &crop,
                "invalid crop format, use c=100 or c=100x100",
                "invalid crop width",
                "invalid crop height",
            )?;
            params.width = w;
            params.height = h;
        } else {
            let size = atoi(&crop).filter(|s| *s > 0).ok_or("invalid crop size")?;
            params.width = size;
            params.height = size;
        }
        params.cache_key = format!("c_{}x{}", params.width, params.height);
        return Ok(params);
    }

    if let Some(width) = get("w").or_else(|| get("width")) {
        if width.contains('x') {
            let (w, h) = dimensions(
                &width,
                "invalid dimension format, use w=100x100",
                "invalid width",
                "invalid height",
            )?;
            params.width = w;
            params.height = h;
            params.cache_key = format!("w_{w}x{h}");
        } else {
            let w = atoi(&width)
                .filter(|w| *w > 0)
                .ok_or("invalid width parameter")?;
            params.width = w;
            params.cache_key = format!("w_{w}");
        }
    }
    if let Some(height) = get("h").or_else(|| get("height")) {
        let h = atoi(&height)
            .filter(|h| *h > 0)
            .ok_or("invalid height parameter")?;
        params.height = h;
        params.cache_key = if params.width > 0 {
            format!("w_{}x{h}", params.width)
        } else {
            format!("h_{h}")
        };
    }
    Ok(params)
}

/// First value of `key` in a raw query, decoded like Go's `url.Values.Get`.
pub fn query_get(raw: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(raw.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Path form `w300&c=200x100&cam=top`: `k=v` pairs, or a `w`/`h`/`c` letter followed by an optional `_` and the value.
fn path_values(segment: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for param in segment.split('&').filter(|p| !p.is_empty()) {
        if let Some((key, value)) = param.split_once('=') {
            values.insert(key.to_string(), value.to_string());
        } else if param.len() > 1 && param.is_char_boundary(1) {
            let (kind, value) = param.split_at(1);
            let value = value.strip_prefix('_').unwrap_or(value);
            if matches!(kind, "w" | "h" | "c") {
                values.insert(kind.to_string(), value.to_string());
            }
        }
    }
    values
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub params: ResizeParams,
    pub src: String,
}

/// Turns a resize request into params and a normalized source URL; `Err` is the 400 body text.
/// `path` is the decoded URL path, `raw_query` the query as sent.
pub fn parse_request(path: &str, raw_query: &str) -> Result<Parsed, String> {
    let raw_query = raw_query.replace("&amp;", "&");

    let mut segment = "";
    let mut forced: Option<String> = None;
    let mut path_form = false;
    if let Some(ext) = path.strip_prefix("/r.") {
        forced = Some(ext.to_lowercase());
        path_form = true;
    } else if let Some(rest) = path
        .strip_prefix("/r/")
        .filter(|p| !p.is_empty() && *p != "r")
    {
        path_form = true;
        (segment, forced) = split_format_ext(rest);
    }

    let mut params = if !segment.is_empty() {
        let values = path_values(segment.split('?').next().unwrap_or_default());
        parse(|key| values.get(key).cloned())
    } else if path_form {
        Ok(ResizeParams::default())
    } else {
        parse(|key| query_get(&raw_query, key))
    }
    .map_err(|e| format!("Invalid parameters: {e}"))?;

    if let Some(ext) = forced {
        params.format = if ext == "jpeg" { "jpg".into() } else { ext };
    }

    let src = if path_form && !raw_query.is_empty() {
        raw_query.clone()
    } else {
        query_get(&raw_query, "src").unwrap_or_default()
    };
    if src.is_empty() {
        return Err("Missing src parameter".into());
    }
    let src = query_unescape(&src).map_err(|e| format!("Invalid src URL: {e}"))?;
    Ok(Parsed {
        params,
        src: normalize_src(src),
    })
}

/// Go's `url.QueryUnescape`: `+` is a space, `%XX` decodes, a bad escape is an error.
pub fn query_unescape(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit));
                let Some(hex) = hex else {
                    let end = (i + 3).min(bytes.len());
                    return Err(format!(
                        "invalid URL escape \"{}\"",
                        String::from_utf8_lossy(&bytes[i..end])
                    ));
                };
                out.push(
                    u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("00"), 16).unwrap_or(0),
                );
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Repairs `https:/` and `http:/` and adds `https://` to a scheme-less source with a dot in it.
/// Go ran the repair after the prefixing, so `https:/x.com` became `https://https:/x.com`.
pub fn normalize_src(mut src: String) -> String {
    for scheme in ["https:/", "http:/"] {
        if src.starts_with(scheme) && !src.starts_with(&format!("{scheme}/")) {
            src = src.replacen(scheme, &format!("{scheme}/"), 1);
        }
    }
    if !src.starts_with("http://")
        && !src.starts_with("https://")
        && !src.starts_with("//")
        && src.contains('.')
    {
        src = format!("https://{src}");
    }
    src
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &str, query: &str) -> Parsed {
        parse_request(path, query).unwrap()
    }

    #[test]
    fn split_ext_only_strips_known_formats() {
        assert_eq!(split_format_ext("w300.png"), ("w300", Some("png".into())));
        assert_eq!(split_format_ext("w300.JPEG"), ("w300", Some("jpeg".into())));
        assert_eq!(split_format_ext("cam=-1,1,-0.5"), ("cam=-1,1,-0.5", None));
        assert_eq!(
            split_format_ext("cam=-1,1,-0.5.glb"),
            ("cam=-1,1,-0.5", Some("glb".into()))
        );
        assert_eq!(split_format_ext("w300"), ("w300", None));
        assert_eq!(split_format_ext("w300.bmp"), ("w300.bmp", None));
    }

    #[test]
    fn path_forms() {
        let p = parsed("/r/w300", "example.com/a.jpg");
        assert_eq!(
            (p.params.width, p.params.height, p.params.cache_key.as_str()),
            (300, 0, "w_300")
        );
        assert_eq!(p.src, "https://example.com/a.jpg");
        assert_eq!(parsed("/r/w_300", "x.com/a").params.cache_key, "w_300");
        assert_eq!(parsed("/r/w=300", "x.com/a").params.cache_key, "w_300");
        assert_eq!(parsed("/r/h200", "x.com/a").params.cache_key, "h_200");
        assert_eq!(
            parsed("/r/w300x200", "x.com/a").params.cache_key,
            "w_300x200"
        );
        assert_eq!(
            parsed("/r/w300&h=200", "x.com/a").params.cache_key,
            "w_300x200"
        );
        let c = parsed("/r/c300", "x.com/a").params;
        assert!(c.crop);
        assert_eq!(
            (c.width, c.height, c.cache_key.as_str()),
            (300, 300, "c_300x300")
        );
        assert_eq!(
            parsed("/r/c300x200", "x.com/a").params.cache_key,
            "c_300x200"
        );
        assert_eq!(parsed("/r/w300.png", "x.com/a").params.format, "png");
        assert_eq!(parsed("/r/w300.jpeg", "x.com/a").params.format, "jpg");
        assert_eq!(parsed("/r/w300&f=webp", "x.com/a").params.format, "webp");
        assert_eq!(
            parsed("/r.png", "x.com/a").params,
            ResizeParams {
                format: "png".into(),
                ..Default::default()
            }
        );
        assert_eq!(parsed("/r.glb", "x.com/a.step").params.format, "glb");
        assert_eq!(parsed("/r/to=glb", "x.com/a.step").params.format, "glb");
    }

    #[test]
    fn step_params() {
        let p = parsed("/r/w600&cam=top&bg=white.png", "x.com/a.step").params;
        assert_eq!(
            (p.cam_dir.as_str(), p.cam_key.as_str(), p.bg_key.as_str()),
            ("0,0,-1", "top", "white")
        );
        assert!(!p.bg_transparent);
        assert_eq!(p.format, "png");
        let v = parsed("/r/c300&cam=-1,1,-0.5", "x.com/a.step").params;
        assert_eq!(v.cam_key, "-1,1,-0.5");
        assert!(v.bg_transparent);
        assert_eq!(v.bg_key, "transparent");
    }

    #[test]
    fn legacy_query_form() {
        let p = parsed("/resize", "src=https://example.com/a.jpg&w=200");
        assert_eq!(p.params.cache_key, "w_200");
        assert_eq!(p.src, "https://example.com/a.jpg");
        assert_eq!(
            parsed("/resize", "src=x.com/a&width=10&height=20")
                .params
                .cache_key,
            "w_10x20"
        );
        assert_eq!(
            parsed("/resize", "src=x.com/a&crop=50").params.cache_key,
            "c_50x50"
        );
        assert_eq!(
            parsed("/resize", "src=x.com/a&w=5&amp;h=6")
                .params
                .cache_key,
            "w_5x6"
        );
        assert_eq!(parsed("/r/", "src=x.com/a&w=5").params.cache_key, "w_5");
    }

    #[test]
    fn errors() {
        assert_eq!(
            parse_request("/r/w0", "x.com/a").unwrap_err(),
            "Invalid parameters: invalid width parameter"
        );
        assert_eq!(
            parse_request("/r/wabc", "x.com/a").unwrap_err(),
            "Invalid parameters: invalid width parameter"
        );
        assert_eq!(
            parse_request("/r/c1x2x3", "x.com/a").unwrap_err(),
            "Invalid parameters: invalid crop format, use c=100 or c=100x100"
        );
        assert_eq!(
            parse_request("/r/w300", "").unwrap_err(),
            "Missing src parameter"
        );
        assert_eq!(
            parse_request("/resize", "w=3").unwrap_err(),
            "Missing src parameter"
        );
        assert_eq!(
            parse_request("/r/f=bmp", "x.com/a").unwrap_err(),
            "Invalid parameters: invalid f parameter 'bmp'"
        );
        assert_eq!(
            parse_request("/r/to=obj", "x.com/a").unwrap_err(),
            "Invalid parameters: invalid to parameter 'obj', only 'glb' is supported"
        );
        assert_eq!(
            parse_request("/r/w3", "x.com/%zz").unwrap_err(),
            "Invalid src URL: invalid URL escape \"%zz\""
        );
    }

    #[test]
    fn src_normalization() {
        assert_eq!(
            parsed("/r/w3", "https%3A%2F%2Fx.com%2Fa.jpg").src,
            "https://x.com/a.jpg"
        );
        assert_eq!(
            parsed("/r/w3", "https:/x.com/a.jpg").src,
            "https://x.com/a.jpg"
        );
        assert_eq!(
            parsed("/r/w3", "http:/x.com/a.jpg").src,
            "http://x.com/a.jpg"
        );
        assert_eq!(
            parsed("/r/w3", "http://x.com/a.jpg").src,
            "http://x.com/a.jpg"
        );
        assert_eq!(parsed("/r/w3", "localhost/a").src, "localhost/a");
        assert_eq!(
            parsed("/r/w3", "x.com/a?b=1&c=2").src,
            "https://x.com/a?b=1&c=2"
        );
    }
}
