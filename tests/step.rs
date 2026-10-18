// STEP renders and GLB conversion; skipped when f3d / DRAWEXE are not installed.

mod common;

use std::time::Duration;

use common::{AVIF, dims, env_with};

fn have(tool: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn renders_snapshots_per_camera_and_background() {
    if !have("f3d") {
        eprintln!("f3d not installed, skipping");
        return;
    }
    // f3d takes seconds per render; wait for it instead of taking the spinner
    let env = env_with(|c| c.wait_timeout = Duration::from_secs(60)).await;
    let url = format!("{}/box.step", env.origin);

    let r = env.resize("/r/w600.png", "/box.step", AVIF).await;
    assert_eq!(
        (r.status.as_u16(), r.header("content-type")),
        (200, "image/png"),
        "{}",
        r.header("x-info")
    );
    let (w, h) = dims(&r.body);
    assert!(
        w == 600 && h > 100,
        "trimmed render keeps the model aspect, got {w}x{h}"
    );
    env.wait_cached(&url, "step").await;
    env.wait_cached(&url, "source_cam-iso_bg-transparent").await;
    env.wait_cached(&url, "cam-iso_bg-transparent_w_600_png")
        .await;

    let top = env
        .resize("/r/w300&cam=top&bg=white", "/box.step", AVIF)
        .await;
    assert_eq!(
        top.header("content-type"),
        "image/avif",
        "{}",
        top.header("x-info")
    );
    env.wait_cached(&url, "source_cam-top_bg-white").await;
    env.wait_cached(&url, "cam-top_bg-white_w_300_avif").await;

    let bad = env.resize("/r/w300&cam=0,0,0", "/box.step", AVIF).await;
    assert_eq!(bad.status, 400);

    // STEP served without a .step extension is sniffed and rendered with the default view
    let sniffed = env.resize("/r/w200.png", "/model", AVIF).await;
    assert_eq!(
        sniffed.header("content-type"),
        "image/png",
        "{}",
        sniffed.header("x-info")
    );
}

#[tokio::test]
async fn converts_to_glb() {
    if !have("DRAWEXE") && !have("occt-draw") {
        eprintln!("DRAWEXE not installed, skipping");
        return;
    }
    // f3d takes seconds per render; wait for it instead of taking the spinner
    let env = env_with(|c| c.wait_timeout = Duration::from_secs(60)).await;
    let r = env.resize("/r.glb", "/box.step", "*/*").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), "model/gltf-binary");
    assert_eq!(r.header("access-control-allow-origin"), "*");
    assert_eq!(&r.body[..4], b"glTF");
    image_resizer::step::validate_glb(&r.body).unwrap();
    env.wait_cached(&format!("{}/box.step", env.origin), "glb")
        .await;

    let hit = env.resize("/r/to=glb", "/box.step", "*/*").await;
    assert_eq!(hit.header("x-cache"), "HIT");
    assert_eq!(hit.header("access-control-allow-origin"), "*");

    let not_step = env.resize("/r.glb", "/test.jpeg", "*/*").await;
    assert_eq!(not_step.status, 422);
    assert!(not_step.text().contains("not-a-step-file"));
}
