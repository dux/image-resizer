// STEP (ISO 10303) CAD support: f3d snapshot renders and GLB conversion through scripts/step2glb.
// Renders are cached per camera/background under `source_cam-<cam>_bg-<bg>`, raw STEP bytes once under `step`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::jobs::JobError;

/// Bounds a single f3d render or GLB conversion.
const TOOL_TIMEOUT: Duration = Duration::from_secs(45);

/// f3d `--camera-direction` vectors (the direction the camera looks along, Z-up).
const CAM_PRESETS: [(&str, &str); 7] = [
    ("iso", "-1,1,-1"),
    ("front", "0,1,0"),
    ("back", "0,-1,0"),
    ("left", "1,0,0"),
    ("right", "-1,0,0"),
    ("top", "0,0,-1"),
    ("bottom", "0,0,1"),
];

pub fn is_step_source(src: &str) -> bool {
    url::Url::parse(src).is_ok_and(|u| {
        let ext = std::path::Path::new(u.path())
            .extension()
            .map(std::ffi::OsStr::to_ascii_lowercase);
        ext.is_some_and(|e| e == "step" || e == "stp")
    })
}

/// STEP files start with `ISO-10303-21;`, possibly after whitespace or a BOM.
pub fn is_step_data(data: &[u8]) -> bool {
    let head = &data[..data.len().min(64)];
    head.windows(12).any(|w| w == b"ISO-10303-21")
}

/// Validates `cam` and returns the f3d direction plus a token safe for cache keys; empty means iso.
pub fn parse_cam(value: &str) -> Result<(String, String), String> {
    let mut s = value.trim().to_lowercase();
    if s.is_empty() {
        s = "iso".into();
    }
    if let Some((_, dir)) = CAM_PRESETS.iter().find(|(name, _)| *name == s) {
        return Ok(((*dir).to_string(), s));
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return Err(format!(
            "invalid cam '{s}', use a preset (iso, front, top, ...) or x,y,z"
        ));
    }
    let mut non_zero = false;
    for part in &parts {
        let f: f64 = part
            .parse()
            .map_err(|_| format!("invalid cam vector '{s}'"))?;
        if !f.is_finite() {
            return Err(format!("invalid cam vector '{s}'"));
        }
        non_zero |= f != 0.0;
    }
    if !non_zero {
        return Err("cam vector must be non-zero".into());
    }
    Ok((s.clone(), s))
}

/// Empty, `transparent` and `none` render with alpha (the default); `white` is opaque white.
pub fn parse_bg(value: &str) -> Result<(bool, String), String> {
    match value.trim().to_lowercase().as_str() {
        "" | "transparent" | "none" => Ok((true, "transparent".into())),
        "white" => Ok((false, "white".into())),
        other => Err(format!(
            "invalid bg '{other}', use transparent (default), none, or white"
        )),
    }
}

/// `iso_bg-transparent`, shared by the source and variant cache keys.
pub fn cam_bg_token(cam_key: &str, bg_key: &str) -> String {
    let cam = if cam_key.is_empty() { "iso" } else { cam_key };
    let bg = if bg_key.is_empty() {
        "transparent"
    } else {
        bg_key
    };
    format!("{cam}_bg-{bg}")
}

pub fn source_key(cam_key: &str, bg_key: &str) -> String {
    format!("source_cam-{}", cam_bg_token(cam_key, bg_key))
}

/// Runs a tool on the STEP bytes in a scratch dir and returns the output file.
async fn run_tool(
    step: &[u8],
    out_name: &str,
    argv: impl FnOnce(&Path, &Path) -> Vec<String>,
) -> Result<Vec<u8>, JobError> {
    let dir = tempfile::Builder::new()
        .prefix("step-")
        .tempdir()
        .map_err(JobError::internal)?;
    let input = dir.path().join("model.step");
    let output = dir.path().join(out_name);
    tokio::fs::write(&input, step)
        .await
        .map_err(JobError::internal)?;

    let argv = argv(&input, &output);
    let tool = Path::new(&argv[0])
        .file_name()
        .map_or_else(|| argv[0].clone(), |n| n.to_string_lossy().into_owned());
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| JobError::new(format!("{tool}-failed; {e}")))?;
    let result = tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| JobError::retry(format!("{tool}-failed; timeout")))?
        .map_err(|e| JobError::new(format!("{tool}-failed; {e}")))?;
    if !result.status.success() {
        let mut msg = String::from_utf8_lossy(&result.stdout).into_owned();
        msg.push_str(&String::from_utf8_lossy(&result.stderr));
        let msg: String = msg.trim().chars().take(300).collect();
        return Err(JobError::new(format!(
            "{tool}-failed; {}; {msg}",
            result.status
        )));
    }
    tokio::fs::read(&output)
        .await
        .map_err(|_| JobError::new(format!("{tool}-no-output; tool produced no result")))
}

/// Renders STEP bytes to a PNG snapshot.
pub async fn render_png(
    f3d: &str,
    step: &[u8],
    cam_dir: &str,
    transparent: bool,
    size: i32,
) -> Result<Vec<u8>, JobError> {
    let cam_dir = if cam_dir.is_empty() {
        CAM_PRESETS[0].1
    } else {
        cam_dir
    }
    .to_string();
    let f3d = f3d.to_string();
    run_tool(step, "render.png", move |input, output| {
        let mut args = vec![
            f3d,
            input.to_string_lossy().into_owned(),
            // ignore user/system config: no grid, filename or axis chrome
            "--no-config".into(),
            // input is sniff-verified STEP; auto-detection rejects some real exports (CATIA V5 AP203)
            "--force-reader=STEP".into(),
            "--verbose=quiet".into(),
            "--output".into(),
            output.to_string_lossy().into_owned(),
            "--resolution".into(),
            format!("{size},{size}"),
            "--camera-direction".into(),
            cam_dir,
            "--up".into(),
            "+Z".into(),
            "--anti-aliasing".into(),
            "--ambient-occlusion".into(),
        ];
        if transparent {
            args.push("--no-background".into());
        } else {
            args.extend(["--background-color".into(), "1,1,1".into()]);
        }
        args
    })
    .await
}

pub async fn convert_glb(step2glb: &str, step: &[u8]) -> Result<Vec<u8>, JobError> {
    let bin = step2glb.to_string();
    let data = run_tool(step, "model.glb", move |input, output| {
        vec![
            bin,
            input.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
        ]
    })
    .await?;
    validate_glb(&data)?;
    Ok(data)
}

/// A STEP file without solid geometry converts into an empty scene; that is an error, not a cache entry.
pub fn validate_glb(data: &[u8]) -> Result<(), JobError> {
    if data.len() < 20 || &data[..4] != b"glTF" {
        return Err(JobError::new(
            "glb-invalid; converter produced non-GLB output",
        ));
    }
    let json_len = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
    if json_len == 0 || 20 + json_len > data.len() {
        return Err(JobError::new("glb-invalid; bad JSON chunk length"));
    }
    if !data[20..20 + json_len]
        .windows(8)
        .any(|w| w == br#""meshes""#)
    {
        return Err(JobError::new(
            "glb-empty; STEP file contains no solid geometry",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_source_by_extension() {
        assert!(is_step_source("https://x.com/part.step"));
        assert!(is_step_source("https://x.com/part.STP?v=1"));
        assert!(!is_step_source("https://x.com/part.png"));
        assert!(!is_step_source("https://x.com/a?f=part.step"));
    }

    #[test]
    fn step_data_sniff() {
        assert!(is_step_data(b"ISO-10303-21;\nHEADER;"));
        assert!(is_step_data(b"\xEF\xBB\xBF  ISO-10303-21;"));
        assert!(!is_step_data(b"\x89PNG...."));
        assert!(!is_step_data(
            &[b' '; 70]
                .iter()
                .chain(b"ISO-10303-21")
                .copied()
                .collect::<Vec<_>>()
        ));
    }

    #[test]
    fn cams() {
        assert_eq!(parse_cam("").unwrap(), ("-1,1,-1".into(), "iso".into()));
        assert_eq!(parse_cam(" TOP ").unwrap(), ("0,0,-1".into(), "top".into()));
        assert_eq!(
            parse_cam("1,2,3").unwrap(),
            ("1,2,3".into(), "1,2,3".into())
        );
        assert_eq!(parse_cam("-1,1,-0.5").unwrap().1, "-1,1,-0.5");
        assert!(parse_cam("0,0,0").unwrap_err().contains("non-zero"));
        assert!(parse_cam("1,2").unwrap_err().contains("use a preset"));
        assert!(
            parse_cam("a,b,c")
                .unwrap_err()
                .contains("invalid cam vector")
        );
    }

    #[test]
    fn backgrounds() {
        assert_eq!(parse_bg("").unwrap(), (true, "transparent".into()));
        assert_eq!(parse_bg("none").unwrap(), (true, "transparent".into()));
        assert_eq!(parse_bg("White").unwrap(), (false, "white".into()));
        assert!(parse_bg("red").is_err());
    }

    #[test]
    fn keys() {
        assert_eq!(source_key("", ""), "source_cam-iso_bg-transparent");
        assert_eq!(source_key("top", "white"), "source_cam-top_bg-white");
        assert_eq!(cam_bg_token("1,2,3", "transparent"), "1,2,3_bg-transparent");
    }

    #[test]
    fn glb_validation() {
        let glb = |json: &str| {
            let mut d = b"glTF".to_vec();
            d.extend(2u32.to_le_bytes());
            d.extend(0u32.to_le_bytes());
            d.extend((json.len() as u32).to_le_bytes());
            d.extend(b"JSON");
            d.extend(json.as_bytes());
            d
        };
        assert!(validate_glb(&glb(r#"{"meshes":[{}]}"#)).is_ok());
        assert!(
            validate_glb(&glb(r#"{"nodes":[]}"#))
                .unwrap_err()
                .to_string()
                .starts_with("glb-empty")
        );
        assert!(
            validate_glb(b"nope")
                .unwrap_err()
                .to_string()
                .starts_with("glb-invalid")
        );
        let mut bad = glb("{}");
        bad[12] = 200;
        assert!(
            validate_glb(&bad)
                .unwrap_err()
                .to_string()
                .contains("bad JSON chunk")
        );
    }
}
