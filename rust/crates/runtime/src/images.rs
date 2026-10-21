//! Images the model sees (host mode, DE0CH fork): `CLAW_IMAGES=1` turns it on; without it
//! nothing here is used and `read_file` stays text-only.
//!
//! - `read_file` on an image (png, jpeg, webp, gif; heic/heif, bmp, tiff, svg converted to PNG)
//!   or a PDF page (`page`, rendered with `pdftoppm`) returns the image as a tool-result image.
//! - `CLAW_PROMPT_IMAGES` = a JSON array of workdir-relative paths: attached as images to the
//!   prompt's user message, before its text.
//! - Images are stored in the session as references ([`ImageRef`]: path, sha256 of the file,
//!   media type, page), never as bytes. The request builder rehydrates them from the workdir
//!   (the cwd) on every request; a file that changed or went away becomes a text note.
//! - Size rules (as Claude Code's Read): long edge at most 1568 px and at most 5 MB once
//!   base64-encoded; a file already within them is sent unchanged. Processing is deterministic
//!   (same bytes on every request, so prompt caching keeps working) and cached per process.
//! - At most [`MAX_IMAGES_PER_RUN`] new images per claw run; at most
//!   [`MAX_IMAGES_PER_REQUEST`] image blocks per request (older ones become a text note).
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageFormat, ImageReader};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::session::{ContentBlock, ConversationMessage, ImageRef, MessageRole};

/// Long edge limit in pixels.
pub const MAX_LONG_EDGE: u32 = 1568;
/// Limit on an image's base64 encoding in bytes.
pub const MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
/// New images (prompt + `read_file`) one claw run may add.
pub const MAX_IMAGES_PER_RUN: usize = 20;
/// Image blocks one request may carry (the most recent ones).
pub const MAX_IMAGES_PER_REQUEST: usize = 40;
/// Files larger than this are not read as images.
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
/// Marks a tool output that carries image references (first line), see [`split_tool_output`].
const MARKER: &str = "\u{1}claw-images:";

static FORCE_ENABLED: AtomicBool = AtomicBool::new(false);
static USED: AtomicUsize = AtomicUsize::new(0);
static PROMPT_TAKEN: AtomicBool = AtomicBool::new(false);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

type CacheKey = (PathBuf, String, Option<u32>);

fn cache() -> &'static Mutex<HashMap<CacheKey, Arc<Processed>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<Processed>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `CLAW_IMAGES` turns images on.
#[must_use]
pub fn enabled() -> bool {
    FORCE_ENABLED.load(Ordering::SeqCst)
        || std::env::var("CLAW_IMAGES")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
}

/// Test support: turn images on without the environment.
pub fn force_enable_for_tests() {
    FORCE_ENABLED.store(true, Ordering::SeqCst);
}

/// Test support: forget the per-run count and the prompt images.
pub fn reset_for_tests() {
    USED.store(0, Ordering::SeqCst);
    PROMPT_TAKEN.store(false, Ordering::SeqCst);
    if let Ok(mut cache) = cache().lock() {
        cache.clear();
    }
}

/// An image ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub media_type: String,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Size of the source image (after rendering a PDF page / converting).
    pub source_width: u32,
    pub source_height: u32,
    /// Pages of a PDF.
    pub page_count: Option<u32>,
}

impl Processed {
    /// The image's bytes in base64 (what the API's `source.data` takes).
    #[must_use]
    pub fn base64(&self) -> String {
        base64_encode(&self.data)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Sent as is when within the limits.
    Native,
    /// Decoded by the `image` crate (bmp, tiff).
    Decodable,
    Heic,
    Svg,
    Pdf,
}

fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" => Kind::Native,
        "bmp" | "tif" | "tiff" => Kind::Decodable,
        "heic" | "heif" => Kind::Heic,
        "svg" => Kind::Svg,
        "pdf" => Kind::Pdf,
        _ => return None,
    })
}

/// True when `read_file` shows this path as an image (by its extension).
#[must_use]
pub fn is_image_path(path: &str) -> bool {
    kind_of(Path::new(path)).is_some()
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(base64_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[must_use]
pub fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

fn media_type_of(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

/// Fits `bytes` (any format the `image` crate reads) into the limits: unchanged when a PNG,
/// JPEG, GIF or WebP already within them, else downscaled and re-encoded (PNG, or JPEG when the
/// PNG is too big).
pub fn fit(bytes: &[u8]) -> Result<Processed, String> {
    let format =
        image::guess_format(bytes).map_err(|_| "not an image format I can read".to_string())?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|error| format!("the image could not be read: {error}"))?;
    if let Some(media_type) = media_type_of(format) {
        if width.max(height) <= MAX_LONG_EDGE && base64_len(bytes.len()) <= MAX_BASE64_BYTES {
            return Ok(Processed {
                media_type: media_type.to_string(),
                data: bytes.to_vec(),
                width,
                height,
                source_width: width,
                source_height: height,
                page_count: None,
            });
        }
    }
    let image = ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .map_err(|error| format!("the image could not be decoded: {error}"))?;
    fit_decoded(&image)
}

fn fit_decoded(image: &DynamicImage) -> Result<Processed, String> {
    let (source_width, source_height) = (image.width(), image.height());
    let mut target = MAX_LONG_EDGE.min(source_width.max(source_height)).max(1);
    for _ in 0..8 {
        let scaled = if source_width.max(source_height) > target {
            image.resize(target, target, FilterType::Lanczos3)
        } else {
            image.clone()
        };
        let png = encode_png(&scaled)?;
        if base64_len(png.len()) <= MAX_BASE64_BYTES {
            return Ok(done("image/png", png, &scaled, source_width, source_height));
        }
        for quality in [85_u8, 70, 55] {
            let jpeg = encode_jpeg(&scaled, quality)?;
            if base64_len(jpeg.len()) <= MAX_BASE64_BYTES {
                return Ok(done(
                    "image/jpeg",
                    jpeg,
                    &scaled,
                    source_width,
                    source_height,
                ));
            }
        }
        target = (target * 3 / 4).max(1);
    }
    Err("the image could not be made small enough".to_string())
}

fn done(media_type: &str, data: Vec<u8>, image: &DynamicImage, sw: u32, sh: u32) -> Processed {
    Processed {
        media_type: media_type.to_string(),
        data,
        width: image.width(),
        height: image.height(),
        source_width: sw,
        source_height: sh,
        page_count: None,
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|error| format!("PNG encoding failed: {error}"))?;
    Ok(out)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let rgb = image.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("JPEG encoding failed: {error}"))?;
    Ok(out)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "claw-img-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the first of `commands` that exists; Ok(()) when one succeeded.
fn run_first(commands: &[(&str, Vec<String>)], what: &str) -> Result<(), String> {
    let mut last = format!("no converter for {what} is installed");
    for (program, args) in commands {
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                last = format!(
                    "{program} failed: {}",
                    stderr.lines().last().unwrap_or("").trim()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => last = format!("{program}: {error}"),
        }
    }
    Err(last)
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn convert_heic(source: &Path) -> Result<Vec<u8>, String> {
    let dir = TempDir::new()?;
    let out = dir.0.join("out.png");
    let (src, dst) = (path_arg(source), path_arg(&out));
    run_first(
        &[
            ("heif-dec", vec![src.clone(), dst.clone()]),
            ("heif-convert", vec![src.clone(), dst.clone()]),
            ("magick", vec![format!("{src}[0]"), format!("png:{dst}")]),
            ("convert", vec![format!("{src}[0]"), format!("png:{dst}")]),
        ],
        "HEIC",
    )?;
    std::fs::read(&out).map_err(|_| "the HEIC converter wrote no image".to_string())
}

fn convert_svg(source: &Path) -> Result<Vec<u8>, String> {
    let dir = TempDir::new()?;
    let out = dir.0.join("out.png");
    let (src, dst) = (path_arg(source), path_arg(&out));
    run_first(
        &[
            (
                "rsvg-convert",
                vec!["-o".to_string(), dst.clone(), src.clone()],
            ),
            ("magick", vec![src.clone(), format!("png:{dst}")]),
            ("convert", vec![src.clone(), format!("png:{dst}")]),
        ],
        "SVG",
    )?;
    std::fs::read(&out).map_err(|_| "the SVG converter wrote no image".to_string())
}

fn pdf_page_count(source: &Path) -> Result<u32, String> {
    let output = Command::new("pdfinfo")
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "no PDF renderer (pdfinfo/pdftoppm) is installed".to_string()
            } else {
                format!("pdfinfo: {error}")
            }
        })?;
    if !output.status.success() {
        return Err("the PDF could not be opened".to_string());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Pages:").map(|n| n.trim().parse::<u32>()))
        .and_then(Result::ok)
        .ok_or_else(|| "the PDF's page count could not be read".to_string())
}

fn render_pdf_page(source: &Path, page: u32) -> Result<Vec<u8>, String> {
    let dir = TempDir::new()?;
    let root = dir.0.join("page");
    let page_arg = page.to_string();
    run_first(
        &[(
            "pdftoppm",
            vec![
                "-f".to_string(),
                page_arg.clone(),
                "-l".to_string(),
                page_arg,
                "-png".to_string(),
                "-singlefile".to_string(),
                "-scale-to".to_string(),
                MAX_LONG_EDGE.to_string(),
                path_arg(source),
                path_arg(&root),
            ],
        )],
        "PDF",
    )?;
    std::fs::read(root.with_extension("png"))
        .map_err(|_| "the PDF page could not be rendered".to_string())
}

/// Turns a file's bytes into an image to send (converting and fitting as needed).
fn process_bytes(path: &Path, bytes: &[u8], page: Option<u32>) -> Result<Processed, String> {
    let kind = kind_of(path).ok_or_else(|| "not an image file".to_string())?;
    match kind {
        Kind::Native | Kind::Decodable => fit(bytes),
        Kind::Heic => fit(&convert_heic(path)?),
        Kind::Svg => fit(&convert_svg(path)?),
        Kind::Pdf => {
            let count = pdf_page_count(path)?;
            let page = page.unwrap_or(1);
            if page == 0 || page > count {
                return Err(format!(
                    "the PDF has {count} page(s); page {page} does not exist"
                ));
            }
            let mut processed = fit(&render_pdf_page(path, page)?)?;
            processed.page_count = Some(count);
            Ok(processed)
        }
    }
}

/// Reads and processes a file, through the per-process cache. Returns its sha256 too.
fn load(
    absolute: &Path,
    page: Option<u32>,
    expect_sha: Option<&str>,
) -> Result<(String, Arc<Processed>), String> {
    let metadata =
        std::fs::metadata(absolute).map_err(|_| "the file does not exist".to_string())?;
    if !metadata.is_file() {
        return Err("not a file".to_string());
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "the file is too large ({} MB)",
            metadata.len() / 1_048_576
        ));
    }
    let bytes = std::fs::read(absolute).map_err(|error| error.to_string())?;
    let sha = sha256_hex(&bytes);
    if let Some(expected) = expect_sha {
        if expected != sha {
            return Err("the file changed".to_string());
        }
    }
    let page = if kind_of(absolute) == Some(Kind::Pdf) {
        Some(page.unwrap_or(1))
    } else {
        None
    };
    let key = (absolute.to_path_buf(), sha.clone(), page);
    if let Some(hit) = cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned())
    {
        return Ok((sha, hit));
    }
    let processed = Arc::new(process_bytes(absolute, &bytes, page)?);
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key, Arc::clone(&processed));
    }
    Ok((sha, processed))
}

/// `path` (relative to `root` or absolute) inside `root`: (absolute canonical path, path as stored).
fn resolve(root: &Path, path: &str) -> Result<(PathBuf, String), String> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|_| format!("{path} does not exist"))?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let relative = canonical
        .strip_prefix(&root)
        .map_err(|_| format!("{path} is outside the workspace"))?;
    Ok((
        canonical.clone(),
        relative.to_string_lossy().replace('\\', "/"),
    ))
}

fn take_slot() -> bool {
    let mut used = USED.load(Ordering::SeqCst);
    while used < MAX_IMAGES_PER_RUN {
        match USED.compare_exchange(used, used + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => used = now,
        }
    }
    false
}

fn give_back_slot() {
    let mut used = USED.load(Ordering::SeqCst);
    while used > 0 {
        match USED.compare_exchange(used, used - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(now) => used = now,
        }
    }
}

fn limit_note() -> String {
    format!(
        "Image limit for this message reached ({MAX_IMAGES_PER_RUN} images); the image was not shown. \
         Describe what you have, or continue in the next message."
    )
}

fn image_ref(stored: String, sha: String, processed: &Processed, page: Option<u32>) -> ImageRef {
    ImageRef {
        path: stored,
        sha256: sha,
        media_type: processed.media_type.clone(),
        page,
        width: processed.width,
        height: processed.height,
    }
}

/// `read_file` on an image or PDF (in `root`, the cwd): the tool output, carrying the image as a
/// reference that the conversation loop turns into an image block ([`split_tool_output`]).
pub fn read_image_tool(root: &Path, path: &str, page: Option<u32>) -> Result<String, String> {
    let (absolute, stored) = resolve(root, path)?;
    if !take_slot() {
        return Ok(limit_note());
    }
    let result = load(&absolute, page, None);
    let (sha, processed) = match result {
        Ok(value) => value,
        Err(error) => {
            give_back_slot();
            return Err(format!("{path}: {error}"));
        }
    };
    let is_pdf = processed.page_count.is_some();
    let page = is_pdf.then(|| page.unwrap_or(1));
    let size = format!("{}x{} px", processed.width, processed.height);
    let scaled = if (processed.width, processed.height)
        == (processed.source_width, processed.source_height)
    {
        String::new()
    } else {
        format!(
            ", downscaled from {}x{}",
            processed.source_width, processed.source_height
        )
    };
    let text = if let (Some(count), Some(page)) = (processed.page_count, page) {
        format!(
            "PDF {stored}: page {page} of {count}, shown to you below as an image ({size}). \
             read_file with page=K shows another page; use pdftotext in bash for the text."
        )
    } else {
        format!(
            "Image {stored}: {size}{scaled} ({}), shown to you below.",
            processed.media_type
        )
    };
    let reference = image_ref(stored, sha, &processed, page);
    Ok(format!(
        "{MARKER}{}\n{text}",
        Value::Array(vec![ref_json(&reference)])
    ))
}

/// Splits a tool output into its text and the image references `read_image_tool` put in it.
#[must_use]
pub fn split_tool_output(output: String) -> (String, Vec<ImageRef>) {
    let Some(rest) = output.strip_prefix(MARKER) else {
        return (output, Vec::new());
    };
    let (head, text) = rest.split_once('\n').unwrap_or((rest, ""));
    let refs = serde_json::from_str::<Vec<Value>>(head)
        .unwrap_or_default()
        .iter()
        .filter_map(ref_from_json)
        .collect();
    (text.to_string(), refs)
}

/// The tool message with its images after the result (what the session stores).
#[must_use]
pub fn with_images(mut message: ConversationMessage, images: Vec<ImageRef>) -> ConversationMessage {
    message.blocks.extend(
        images
            .into_iter()
            .map(|image| ContentBlock::Image { image }),
    );
    message
}

/// An image reference as JSON (the event stream's `images` entries).
#[must_use]
pub fn ref_json(image: &ImageRef) -> Value {
    let mut value = json!({
        "path": image.path,
        "sha256": image.sha256,
        "media_type": image.media_type,
        "width": image.width,
        "height": image.height,
    });
    if let Some(page) = image.page {
        value["page"] = json!(page);
    }
    value
}

fn ref_from_json(value: &Value) -> Option<ImageRef> {
    let number = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(ImageRef {
        path: value.get("path")?.as_str()?.to_string(),
        sha256: value.get("sha256")?.as_str()?.to_string(),
        media_type: value.get("media_type")?.as_str()?.to_string(),
        page: number("page"),
        width: number("width").unwrap_or(0),
        height: number("height").unwrap_or(0),
    })
}

/// The prompt's user message with the images `CLAW_PROMPT_IMAGES` names (once per process; None
/// when there are none or images are off).
#[must_use]
pub fn prompt_message(root: &Path, text: &str) -> Option<ConversationMessage> {
    if !enabled() || PROMPT_TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }
    let raw = std::env::var("CLAW_PROMPT_IMAGES").ok()?;
    let paths: Vec<String> = serde_json::from_str(&raw).ok()?;
    if paths.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    let mut shown = Vec::new();
    for path in paths {
        let attached = resolve(root, &path).and_then(|(absolute, stored)| {
            if !take_slot() {
                return Err(format!(
                    "the limit of {MAX_IMAGES_PER_RUN} images per message was reached"
                ));
            }
            match load(&absolute, None, None) {
                Ok((sha, processed)) => {
                    let page = processed.page_count.map(|_| 1);
                    Ok(image_ref(stored, sha, &processed, page))
                }
                Err(error) => {
                    give_back_slot();
                    Err(error)
                }
            }
        });
        match attached {
            Ok(image) => {
                shown.push(ref_json(&image));
                blocks.push(ContentBlock::Image { image });
            }
            Err(reason) => blocks.push(ContentBlock::Text {
                text: format!("[attached image {path} could not be shown: {reason}]"),
            }),
        }
    }
    if !shown.is_empty() {
        crate::event_stream::emit(json!({"type": "prompt_images", "images": shown}));
    }
    blocks.push(ContentBlock::Text {
        text: text.to_string(),
    });
    Some(ConversationMessage {
        role: MessageRole::User,
        blocks,
        usage: None,
    })
}

/// The image's bytes for a request, from the workdir (`root`); Err = the text to send instead.
pub fn rehydrate(root: &Path, image: &ImageRef) -> Result<Arc<Processed>, String> {
    resolve(root, &image.path)
        .and_then(|(absolute, _)| load(&absolute, image.page, Some(&image.sha256)))
        .map(|(_, processed)| processed)
        .map_err(|_| unavailable_note(&image.path))
}

#[must_use]
pub fn unavailable_note(path: &str) -> String {
    format!(
        "[image {path} is no longer available: it changed or was deleted after it was viewed; read it again to see it]"
    )
}

#[must_use]
pub fn omitted_note(path: &str) -> String {
    format!("[earlier image {path} omitted]")
}

/// How many image blocks of `messages` to replace with [`omitted_note`] (the oldest ones), so a
/// request carries at most [`MAX_IMAGES_PER_REQUEST`].
#[must_use]
pub fn images_to_omit(messages: &[ConversationMessage]) -> usize {
    let total = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, ContentBlock::Image { .. }))
        .count();
    total.saturating_sub(MAX_IMAGES_PER_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// Serialises tests that touch the per-run count.
    static LOCK: Mutex<()> = Mutex::new(());

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        encode_png(&DynamicImage::ImageRgb8(image)).unwrap()
    }

    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        let image = RgbImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let [a, b, c, _] = state.to_le_bytes();
            Rgb([a, b, c])
        });
        encode_png(&DynamicImage::ImageRgb8(image)).unwrap()
    }

    fn workdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_len(4), 8);
    }

    #[test]
    fn small_png_is_sent_unchanged() {
        let bytes = png(40, 30);
        let processed = fit(&bytes).unwrap();
        assert_eq!(processed.data, bytes);
        assert_eq!(processed.media_type, "image/png");
        assert_eq!((processed.width, processed.height), (40, 30));
    }

    #[test]
    fn large_image_is_downscaled_to_the_long_edge() {
        let processed = fit(&png(3000, 1000)).unwrap();
        assert_eq!(processed.width, MAX_LONG_EDGE);
        assert_eq!(processed.height, 523);
        assert_eq!(
            (processed.source_width, processed.source_height),
            (3000, 1000)
        );
        // deterministic: the same bytes every time (prompt caching)
        assert_eq!(fit(&png(3000, 1000)).unwrap().data, processed.data);
    }

    #[test]
    fn noisy_image_fits_in_five_megabytes() {
        let processed = fit(&noisy_png(1568, 1568)).unwrap();
        assert!(base64_len(processed.data.len()) <= MAX_BASE64_BYTES);
        assert!(processed.width.max(processed.height) <= MAX_LONG_EDGE);
        assert_eq!(processed.media_type, "image/jpeg");
    }

    #[test]
    fn bmp_is_converted_to_png() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(10, 10, Rgb([1, 2, 3])));
        let mut bmp = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bmp), ImageFormat::Bmp)
            .unwrap();
        let processed = fit(&bmp).unwrap();
        assert_eq!(processed.media_type, "image/png");
    }

    #[test]
    fn read_image_tool_round_trips_through_the_marker_and_rehydrates() {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_tests();
        let dir = workdir();
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        let bytes = png(20, 10);
        std::fs::write(dir.path().join("out/chart.png"), &bytes).unwrap();
        let output = read_image_tool(dir.path(), "out/chart.png", None).unwrap();
        let (text, refs) = split_tool_output(output);
        assert!(text.starts_with("Image out/chart.png: 20x10 px"), "{text}");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "out/chart.png");
        assert_eq!(refs[0].sha256, sha256_hex(&bytes));
        assert_eq!(refs[0].media_type, "image/png");
        assert_eq!(rehydrate(dir.path(), &refs[0]).unwrap().data, bytes);

        // the file changed: a note instead of the image
        std::fs::write(dir.path().join("out/chart.png"), png(21, 10)).unwrap();
        assert_eq!(
            rehydrate(dir.path(), &refs[0]).unwrap_err(),
            unavailable_note("out/chart.png")
        );
        std::fs::remove_file(dir.path().join("out/chart.png")).unwrap();
        assert!(rehydrate(dir.path(), &refs[0]).is_err());
    }

    #[test]
    fn plain_output_has_no_images() {
        let (text, refs) = split_tool_output("hello".to_string());
        assert_eq!(text, "hello");
        assert!(refs.is_empty());
    }

    #[test]
    fn paths_outside_the_workspace_are_refused() {
        let dir = workdir();
        let other = workdir();
        std::fs::write(other.path().join("x.png"), png(2, 2)).unwrap();
        let absolute = other.path().join("x.png").to_string_lossy().into_owned();
        assert!(read_image_tool(dir.path(), &absolute, None)
            .unwrap_err()
            .contains("outside the workspace"));
    }

    #[test]
    fn run_cap_answers_with_a_note() {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_tests();
        let dir = workdir();
        std::fs::write(dir.path().join("a.png"), png(4, 4)).unwrap();
        for _ in 0..MAX_IMAGES_PER_RUN {
            let (_, refs) = split_tool_output(read_image_tool(dir.path(), "a.png", None).unwrap());
            assert_eq!(refs.len(), 1);
        }
        let (text, refs) = split_tool_output(read_image_tool(dir.path(), "a.png", None).unwrap());
        assert!(refs.is_empty());
        assert!(
            text.contains("Image limit for this message reached (20"),
            "{text}"
        );
        reset_for_tests();
    }

    #[test]
    fn omits_the_oldest_images_beyond_the_request_cap() {
        let image = ImageRef {
            path: "a.png".into(),
            sha256: "x".into(),
            media_type: "image/png".into(),
            page: None,
            width: 1,
            height: 1,
        };
        let message = ConversationMessage {
            role: MessageRole::User,
            blocks: (0..30)
                .map(|_| ContentBlock::Image {
                    image: image.clone(),
                })
                .collect(),
            usage: None,
        };
        assert_eq!(images_to_omit(&[message.clone()]), 0);
        assert_eq!(images_to_omit(&[message.clone(), message]), 20);
    }

    #[test]
    fn prompt_images_come_before_the_text_and_note_failures() {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_tests();
        force_enable_for_tests();
        let dir = workdir();
        std::fs::create_dir_all(dir.path().join("uploads")).unwrap();
        std::fs::write(dir.path().join("uploads/photo.jpg"), png(8, 8)).unwrap();
        std::env::set_var(
            "CLAW_PROMPT_IMAGES",
            r#"["uploads/photo.jpg","uploads/missing.png"]"#,
        );
        let message = prompt_message(dir.path(), "what is this?").unwrap();
        std::env::remove_var("CLAW_PROMPT_IMAGES");
        assert_eq!(message.blocks.len(), 3);
        let ContentBlock::Image { image } = &message.blocks[0] else {
            panic!("expected an image first: {:?}", message.blocks[0]);
        };
        // the bytes are a PNG whatever the name says: sent as what they are
        assert_eq!(image.media_type, "image/png");
        assert!(
            matches!(&message.blocks[1], ContentBlock::Text { text } if text.starts_with("[attached image uploads/missing.png could not be shown"))
        );
        assert!(
            matches!(&message.blocks[2], ContentBlock::Text { text } if text == "what is this?")
        );
        // once per process
        assert!(prompt_message(dir.path(), "again").is_none());
        reset_for_tests();
    }
}
