// libvips work: building the cached source, rendering variants, encoders. Everything here blocks.

use rs_vips::voption::{Setter, VOption};
use rs_vips::{Vips, VipsImage};

use super::params::ResizeParams;
use super::plan::{Plan, fit_max, plan};
use crate::jobs::JobError;

/// Decompression bomb guard, checked from the header before any pixel is decoded.
const MAX_PIXELS: i64 = 100_000_000;
/// WebP quality for re-encoded lossy sources: they are an intermediate, keep them close to the original.
const SOURCE_QUALITY: i32 = 95;
/// `keep` flags for savers: drop all metadata.
const KEEP_NONE: i32 = 0;

pub fn init() -> Result<(), String> {
    Vips::init("image-resizer").map_err(|e| e.to_string())?;
    // results are cached by us; the libvips operation cache would only pin source buffers
    Vips::cache_set_max(0);
    Ok(())
}

fn vips_err(prefix: &str, error: impl std::fmt::Display) -> JobError {
    JobError::new(format!("{prefix}; {}", error.to_string().trim()))
}

/// Friendly format name from the loader that opened the image; AVIF and HEIF both surface as `avif`.
pub fn format_of(image: &VipsImage) -> &'static str {
    let loader = image.get_string("vips-loader").unwrap_or_default();
    match loader
        .trim_end_matches("_buffer")
        .trim_end_matches("_source")
    {
        "pngload" => "png",
        "gifload" => "gif",
        "webpload" => "webp",
        "heifload" => "avif",
        "svgload" => "svg",
        "tiffload" => "tiff",
        "magickload" | "bmpload" => "bmp",
        // jpegload and anything unknown
        _ => "jpeg",
    }
}

pub fn mime_of(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        _ => "image/jpeg",
    }
}

fn is_avif_brand(data: &[u8]) -> bool {
    data.len() >= 12 && &data[4..8] == b"ftyp" && matches!(&data[8..12], b"avif" | b"avis")
}

fn header(data: &[u8]) -> Result<VipsImage, JobError> {
    let image = VipsImage::new_from_buffer(data, "").map_err(|e| vips_err("decode-failed", e))?;
    if i64::from(image.get_width()) * i64::from(image.get_height()) > MAX_PIXELS {
        return Err(JobError::new("decode-failed; image has too many pixels"));
    }
    Ok(image)
}

/// The cached form of a downloaded raster source.
pub struct BuiltSource {
    pub data: Vec<u8>,
    pub format: &'static str,
    pub origin: &'static str,
}

/// Originals within `max_size` that need no rotation are stored as they came; everything else is
/// scaled down (shrink-on-load) and stored as near-lossless WebP. GIF keeps its bytes, as before.
pub fn build_source(data: Vec<u8>, max_size: i32) -> Result<BuiltSource, JobError> {
    let image = header(&data)?;
    let origin = format_of(&image);
    if origin == "gif" {
        return Ok(BuiltSource {
            data,
            format: "gif",
            origin,
        });
    }
    let fits = fit_max(image.get_width(), image.get_height(), max_size).is_none();
    let upright = image.get_orientation() <= 1;
    let storable = match origin {
        "jpeg" | "png" | "webp" => true,
        "avif" => is_avif_brand(&data),
        _ => false,
    };
    if fits && upright && storable {
        return Ok(BuiltSource {
            data,
            format: origin,
            origin,
        });
    }

    let thumb = VipsImage::thumbnail_buffer_with_opts(
        &data,
        max_size,
        VOption::new().set("height", max_size).set("size", "down"),
    )
    .map_err(|e| vips_err("resize-failed", e))?;
    let lossy = matches!(origin, "jpeg" | "webp" | "avif");
    let opts = if lossy {
        VOption::new()
            .set("Q", SOURCE_QUALITY)
            .set("keep", KEEP_NONE)
    } else {
        VOption::new().set("lossless", true).set("keep", KEEP_NONE)
    };
    let data = thumb
        .webpsave_buffer_with_opts(opts)
        .map_err(|e| vips_err("source-encode-failed", e))?;
    Ok(BuiltSource {
        data,
        format: "webp",
        origin,
    })
}

/// A rendered STEP snapshot as the cached source: clamped, margins trimmed, lossless WebP
/// (CAD renders are flat-colour line art where lossy artifacts show).
pub fn build_step_source(
    png: &[u8],
    transparent: bool,
    max_size: i32,
) -> Result<Vec<u8>, JobError> {
    let image = VipsImage::thumbnail_buffer_with_opts(
        png,
        max_size,
        VOption::new().set("height", max_size).set("size", "down"),
    )
    .and_then(|i| i.copy_memory())
    .map_err(|e| vips_err("render-decode-failed", e))?;
    let image = trim_margins(image, transparent).map_err(|e| vips_err("render-trim-failed", e))?;
    image
        .webpsave_buffer_with_opts(VOption::new().set("lossless", true).set("keep", KEEP_NONE))
        .map_err(|e| vips_err("render-encode-failed", e))
}

/// Crops f3d's auto-fit padding: on the alpha band for transparent renders, white otherwise.
fn trim_margins(image: VipsImage, transparent: bool) -> rs_vips::Result<VipsImage> {
    let (left, top, width, height) = if transparent {
        if !image.hasalpha() {
            return Ok(image);
        }
        let alpha = image.extract_band(image.get_bands() - 1)?;
        alpha.find_trim_with_opts(
            VOption::new()
                .set("threshold", 10.0)
                .set("background", &[0.0][..]),
        )?
    } else {
        image.find_trim_with_opts(
            VOption::new()
                .set("threshold", 10.0)
                .set("background", &[255.0, 255.0, 255.0][..]),
        )?
    };
    if width > 0 && height > 0 && (width < image.get_width() || height < image.get_height()) {
        return image.extract_area(left, top, width, height);
    }
    Ok(image)
}

pub struct Encoded {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub format: &'static str,
}

/// What the client accepts when the format is negotiated.
#[derive(Clone, Copy, Debug, Default)]
pub struct Accepts {
    pub avif: bool,
    pub webp: bool,
}

/// Resizes the cached source per `params` and encodes it: forced format, else GIF for GIF origins,
/// else AVIF > WebP > original (JPEG/PNG) with fallbacks when an encoder fails.
pub fn render_variant(
    source: &[u8],
    origin: &str,
    params: &ResizeParams,
    accepts: Accepts,
    quality: i32,
    max_size: i32,
) -> Result<Encoded, JobError> {
    let head = header(source).map_err(|e| {
        JobError::new(
            e.message
                .replacen("decode-failed", "source-decode-failed", 1),
        )
    })?;
    let image = match plan(head.get_width(), head.get_height(), params, max_size) {
        Plan::Keep => head.copy_memory(),
        Plan::Scale { width, height } => scale(source, width, height),
        Plan::Cover {
            width,
            height,
            crop,
        } => scale(source, width, height).and_then(|i| {
            let (left, top, w, h) = crop;
            let (w, h) = (w.min(i.get_width() - left), h.min(i.get_height() - top));
            i.extract_area(left, top, w, h)
        }),
    }
    .map_err(|e| vips_err("resize-failed", e))?;

    if !params.format.is_empty() {
        return encode_forced(&image, &params.format, quality)
            .map_err(|e| vips_err("encode-failed", e));
    }
    if origin == "gif" {
        return encode(&image, "gif", quality).map_err(|e| vips_err("gif-encode-failed", e));
    }
    if accepts.avif {
        match encode(&image, "avif", quality) {
            Ok(out) => return Ok(out),
            Err(e) => tracing::warn!(error = %e, "AVIF encode failed, falling back"),
        }
    }
    if accepts.webp {
        match encode(&image, "webp", quality) {
            Ok(out) => return Ok(out),
            Err(e) => tracing::warn!(error = %e, "WebP encode failed, falling back"),
        }
    }
    let fallback = if origin == "png" { "png" } else { "jpeg" };
    encode(&image, fallback, quality).map_err(|e| vips_err("encode-failed", e))
}

/// Exact-size resample with shrink-on-load; thumbnail premultiplies alpha around the resample.
fn scale(source: &[u8], width: i32, height: i32) -> rs_vips::Result<VipsImage> {
    VipsImage::thumbnail_buffer_with_opts(
        source,
        width,
        VOption::new().set("height", height).set("size", "force"),
    )?
    .copy_memory()
}

fn encode_forced(image: &VipsImage, format: &str, quality: i32) -> rs_vips::Result<Encoded> {
    match format {
        "jpg" | "jpeg" => encode(image, "jpeg", quality),
        other => encode(image, other, quality),
    }
}

pub fn encode(image: &VipsImage, format: &str, quality: i32) -> rs_vips::Result<Encoded> {
    let (data, format) = match format {
        "avif" => (
            image.heifsave_buffer_with_opts(
                VOption::new()
                    .set("Q", quality)
                    .set("effort", 1)
                    .set("compression", "av1")
                    .set("keep", KEEP_NONE),
            )?,
            "avif",
        ),
        "webp" => (
            image.webpsave_buffer_with_opts(
                VOption::new().set("Q", quality).set("keep", KEEP_NONE),
            )?,
            "webp",
        ),
        "png" => (
            image.pngsave_buffer_with_opts(VOption::new().set("keep", KEEP_NONE))?,
            "png",
        ),
        "gif" => (image.gifsave_buffer()?, "gif"),
        _ => {
            // JPEG has no alpha; vips would composite onto black
            let flat;
            let image = if image.hasalpha() {
                flat = image.flatten_with_opts(
                    VOption::new().set("background", &[255.0, 255.0, 255.0][..]),
                )?;
                &flat
            } else {
                image
            };
            (
                image.jpegsave_buffer_with_opts(
                    VOption::new().set("Q", quality).set("keep", KEEP_NONE),
                )?,
                "jpeg",
            )
        }
    };
    Ok(Encoded {
        data,
        mime: mime_of(format),
        format,
    })
}
