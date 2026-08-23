//! Embedded media parts (`word/media/*`, `ppt/media/*`, ODF `Pictures/*`) turned
//! into something the preview frame can display.
//!
//! Everything leaves here as a `data:` URI. That is a constraint, not a
//! preference: the preview document is loaded into a sandboxed iframe with an
//! **opaque origin**, so a `blob:` URL minted by the parent document is not
//! fetchable from inside it (an opaque origin is not the blob's origin, and there
//! is no HTTP origin serving the document either). `data:` is the only channel
//! left for binary media, so we accept base64's ~33% size tax and spend the
//! effort on keeping the payloads small instead: downscale to the display size
//! before encoding, and cap bytes per image and per document.

#![allow(dead_code)] // Consumed by the later-stage renderers.

use super::pkg::{read_entry_bytes, Budget, Zip};
use base64::Engine;
use image::{DynamicImage, GenericImageView, ImageEncoder, ImageFormat};
use roxmltree::Node;
use std::collections::HashMap;
use std::rc::Rc;

// ── caps ─────────────────────────────────────────────────────────────────────

/// Per-image cap on the emitted `data:` URI — i.e. on the bytes that actually
/// land in the preview HTML, base64 inflation included.
const MAX_IMAGE_URI_BYTES: usize = 2 * 1024 * 1024;

/// Whole-document cap on emitted image bytes. WebKit has to parse and hold every
/// one of them, so the sum matters more than any single image.
const MAX_TOTAL_URI_BYTES: usize = 24 * 1024 * 1024;

/// Widest image we will encode. `want_px` comes from the document's own drawing
/// extents, which are untrusted: a shape can declare a 100000px width.
const MAX_TARGET_PX: u32 = 2400;

/// Pixel-count ceiling before decoding. Mirrors `preview.rs`'s
/// `MAX_OCR_MEGAPIXELS`: dimensions come out of the container header, so a
/// 64000×64000 declaration is cheap to read and ruinous to decode (4 bytes per
/// pixel of RGBA). Rejected from the header, before any pixel buffer is reserved.
const MAX_MEGAPIXELS: u64 = 40;

/// JPEG quality for re-encoded rasters. 82 is the usual "no visible artefacts on
/// photos at 1:1" point, and document images are displayed at or below their
/// encoded size, which hides the rest.
const JPEG_QUALITY: u8 = 82;

/// Alpha below this counts as real transparency rather than rounding noise left
/// by an earlier encode or resize.
const ALPHA_OPAQUE: u8 = 250;

/// A GIF at or under this size is passed through unchanged (see `encode`).
const GIF_PASSTHROUGH_BYTES: usize = 512 * 1024;

// ── kinds ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Tiff,
    Webp,
    Svg,
    Emf,
    Wmf,
    Unknown,
}

impl MediaKind {
    /// Sniff by magic bytes, NOT by part name: a document controls its own part
    /// names, so `image1.png` holding a JPEG (or an EMF) is ordinary, and the
    /// extension is untrusted metadata. Binary magics are tested before the SVG
    /// text sniff so a binary payload that happens to contain `<svg` cannot be
    /// mistaken for markup.
    pub fn sniff(bytes: &[u8]) -> MediaKind {
        let b = bytes;
        if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            return MediaKind::Png;
        }
        if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return MediaKind::Jpeg;
        }
        if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            return MediaKind::Gif;
        }
        if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
            return MediaKind::Webp;
        }
        // "BM" alone is a weak magic, so require room for the whole file header.
        if b.len() >= 14 && b.starts_with(b"BM") {
            return MediaKind::Bmp;
        }
        if b.starts_with(b"II\x2A\x00") || b.starts_with(b"MM\x00\x2A") {
            return MediaKind::Tiff;
        }
        // EMF: an EMR_HEADER record (type 1) carrying the " EMF" signature at
        // offset 40. The record type on its own collides with too much.
        if b.len() >= 44 && b.starts_with(&[0x01, 0x00, 0x00, 0x00]) && &b[40..44] == b" EMF" {
            return MediaKind::Emf;
        }
        // WMF: the Aldus placeable header, or a bare METAFILE header with
        // mtType 1/2 and the fixed mtHeaderSize of 9 words.
        if b.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A])
            || b.starts_with(&[0x01, 0x00, 0x09, 0x00])
            || b.starts_with(&[0x02, 0x00, 0x09, 0x00])
        {
            return MediaKind::Wmf;
        }
        if sniff_svg(b) {
            return MediaKind::Svg;
        }
        MediaKind::Unknown
    }

    /// True when an `<img>` can render the bytes as they are. TIFF is excluded:
    /// WebKitGTK does not decode it, so a TIFF part has to be re-encoded.
    pub fn browser_ok(self) -> bool {
        matches!(
            self,
            MediaKind::Png
                | MediaKind::Jpeg
                | MediaKind::Gif
                | MediaKind::Bmp
                | MediaKind::Webp
                | MediaKind::Svg
        )
    }

    fn image_format(self) -> Option<ImageFormat> {
        Some(match self {
            MediaKind::Png => ImageFormat::Png,
            MediaKind::Jpeg => ImageFormat::Jpeg,
            MediaKind::Gif => ImageFormat::Gif,
            MediaKind::Bmp => ImageFormat::Bmp,
            MediaKind::Tiff => ImageFormat::Tiff,
            MediaKind::Webp => ImageFormat::WebP,
            _ => return None,
        })
    }
}

/// Looks for an `<svg` element start in the head of the part, tolerating a BOM,
/// an XML declaration, a doctype and comments in front of it.
fn sniff_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let head = head.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(head);
    let lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    lower.windows(4).any(|w| w == b"<svg")
}

// ── budget ───────────────────────────────────────────────────────────────────

// Notes are `&'static str` so de-duplication is a plain compare with no
// allocation, and a document with 300 metafiles still produces one line of
// explanation rather than 300.
const NOTE_TOO_LARGE: &str = "Some images omitted — size limit";
pub(super) const NOTE_BUDGET: &str = "Some images omitted — size limit";
const NOTE_VECTOR: &str = "Vector images not drawn";
const NOTE_UNREADABLE: &str = "Some images could not be decoded";
const NOTE_UNSUPPORTED: &str = "Some image formats unsupported";

/// Per-document image budget: how many encoded bytes the preview has already
/// committed, plus the notes explaining the placeholders it had to emit.
pub struct MediaBudget {
    per_image: usize,
    total: usize,
    spent: usize,
    omitted: usize,
    notes: Vec<&'static str>,
}

impl MediaBudget {
    pub fn new() -> Self {
        MediaBudget::with_caps(MAX_IMAGE_URI_BYTES, MAX_TOTAL_URI_BYTES)
    }

    /// The caps are parameters only so tests can reach the refusal paths without
    /// generating 24 MB of images; renderers use `new`.
    pub fn with_caps(per_image: usize, total: usize) -> Self {
        MediaBudget {
            per_image,
            total,
            spent: 0,
            omitted: 0,
            notes: Vec::new(),
        }
    }

    pub fn spent(&self) -> usize {
        self.spent
    }

    /// How many images were replaced by a placeholder, for any reason.
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn notes(&self) -> &[&'static str] {
        &self.notes
    }

    fn note(&mut self, note: &'static str) {
        self.omitted += 1;
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Charges `n` emitted bytes, or returns the placeholder reason it refused
    /// for. A refusal consumes no budget, so a later smaller image still fits.
    fn take(&mut self, n: usize) -> Result<(), &'static str> {
        if n > self.per_image {
            self.note(NOTE_TOO_LARGE);
            return Err("image too large");
        }
        if self.spent.saturating_add(n) > self.total {
            self.note(NOTE_BUDGET);
            return Err("image budget reached");
        }
        self.spent += n;
        Ok(())
    }
}

impl Default for MediaBudget {
    fn default() -> Self {
        MediaBudget::new()
    }
}

// ── results ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Media {
    /// Shared because one media part is usually referenced from several places (a
    /// logo on every slide) and the URI is by far the largest string in the
    /// output.
    DataUri(Rc<String>),
    /// Caller emits a placeholder box at the correct geometry.
    Placeholder(&'static str /* short reason for the UI */),
}

// ── cache ────────────────────────────────────────────────────────────────────

/// Encoded media keyed by `(part_path, want_px)`, plus the parts that failed for
/// a reason which cannot change between calls.
///
/// The failure side is not only a speed optimization: every `get` that reaches
/// the zip charges the package's inflated-size `Budget`, so an EMF reused on 40
/// slides would exhaust the 64 MB package budget and truncate the rest of the
/// document. Only *want_px-independent* verdicts are remembered there (wrong
/// format, corrupt bytes, decode bomb, missing part); size refusals depend on
/// `want_px` and on how much budget is left, so they are re-evaluated each time.
#[derive(Default)]
pub struct MediaCache {
    uris: HashMap<(String, u32), Rc<String>>,
    hard_fail: HashMap<String, &'static str>,
}

impl MediaCache {
    pub fn new() -> Self {
        MediaCache::default()
    }

    /// Encoded media for `part`, sized for a `want_px`-wide display box. Never
    /// panics: a malformed part degrades to a `Placeholder`.
    ///
    /// The cache is self-bounding — every entry in it was charged to `mb`, so
    /// `MAX_TOTAL_URI_BYTES` caps its memory too.
    pub fn get(
        &mut self,
        zip: &mut Zip,
        budget: &mut Budget,
        mb: &mut MediaBudget,
        part: &str,
        want_px: u32,
    ) -> Media {
        let want_px = want_px.clamp(1, MAX_TARGET_PX);
        let key = (part.to_string(), want_px);
        if let Some(uri) = self.uris.get(&key) {
            return Media::DataUri(Rc::clone(uri));
        }
        if let Some(reason) = self.hard_fail.get(part) {
            return Media::Placeholder(reason);
        }

        let bytes = match read_entry_bytes(zip, part, budget) {
            Ok(Some(b)) if !b.is_empty() => b,
            // Missing, empty, oversized or budget-stopped: nothing to retry.
            _ => return self.fail(mb, part, NOTE_UNREADABLE, "image unavailable"),
        };

        let kind = MediaKind::sniff(&bytes);
        match kind {
            // No pure-Rust EMF/WMF rasterizer exists, and these are everywhere in
            // real documents (pasted Excel charts, equations, Visio drawings,
            // anything round-tripped through the Windows clipboard), so the
            // placeholder is a first-class outcome rather than an edge case.
            MediaKind::Emf | MediaKind::Wmf => {
                return self.fail(mb, part, NOTE_VECTOR, "vector image (EMF/WMF)")
            }
            MediaKind::Unknown => {
                return self.fail(mb, part, NOTE_UNSUPPORTED, "unsupported image")
            }
            _ => {}
        }

        let encoded = match encode(&bytes, kind, want_px) {
            Ok(e) => e,
            Err(reason) => return self.fail(mb, part, NOTE_UNREADABLE, reason),
        };

        // The URI length is what gets charged, not the image length: base64 is
        // what the HTML actually carries.
        let uri = data_uri(encoded.mime, &encoded.bytes);
        if let Err(reason) = mb.take(uri.len()) {
            return Media::Placeholder(reason);
        }
        let uri = Rc::new(uri);
        self.uris.insert(key, Rc::clone(&uri));
        Media::DataUri(uri)
    }

    fn fail(
        &mut self,
        mb: &mut MediaBudget,
        part: &str,
        note: &'static str,
        reason: &'static str,
    ) -> Media {
        mb.note(note);
        self.hard_fail.insert(part.to_string(), reason);
        Media::Placeholder(reason)
    }
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    let mut s = String::with_capacity(mime.len() + bytes.len() * 4 / 3 + 16);
    s.push_str("data:");
    s.push_str(mime);
    s.push_str(";base64,");
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut s);
    s
}

// ── encoding ─────────────────────────────────────────────────────────────────

struct Encoded {
    mime: &'static str,
    bytes: Vec<u8>,
}

fn encode(bytes: &[u8], kind: MediaKind, want_px: u32) -> Result<Encoded, &'static str> {
    if kind == MediaKind::Svg {
        // Passed through verbatim. An SVG referenced by `<img src="data:…">` is
        // rendered in a restricted mode: no scripts run, no external
        // subresources are fetched, and it cannot touch the embedding document —
        // and the preview frame's CSP is `default-src 'none'; img-src data:` on
        // top of that. Rasterizing it instead would mean shipping an SVG
        // renderer, so the untrusted markup goes straight through on purpose. Do
        // not "fix" this by inlining it as an `<svg>` element, which *would* be
        // scriptable.
        return Ok(Encoded {
            mime: "image/svg+xml",
            bytes: bytes.to_vec(),
        });
    }
    let format = kind.image_format().ok_or("unsupported image")?;

    // Dimensions from the container header first. `into_dimensions` builds the
    // decoder and stops, so nothing pixel-sized is allocated and an absurd
    // declaration costs one header read.
    let (w, h) = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| "unreadable image")?;
    if w == 0 || h == 0 {
        return Err("empty image");
    }
    if (w as u64) * (h as u64) > MAX_MEGAPIXELS * 1_000_000 {
        return Err("image too large");
    }

    // An animated GIF survives only as its own bytes: decoding yields frame 1
    // and re-encoding would silently freeze it. So a GIF that is already small
    // and already no wider than its box goes through untouched.
    if kind == MediaKind::Gif && w <= want_px && bytes.len() <= GIF_PASSTHROUGH_BYTES {
        return Ok(Encoded {
            mime: "image/gif",
            bytes: bytes.to_vec(),
        });
    }

    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    // The megapixel check above is the real guard; `image`'s own allocation limit
    // stays at its default as a second line of defence for formats that lie in
    // their header.
    reader.limits(image::Limits::default());
    let img = reader.decode().map_err(|_| "unreadable image")?;

    // Downscale before encoding — this is where essentially all of the saving
    // is. Office embeds the full-resolution original, so a 4000px photo shown
    // 960px wide is ~17x the pixels (and bytes) the preview can use. Never
    // upscale: that spends bytes for no visible detail.
    let img = if img.width() > want_px {
        let nw = want_px.max(1);
        let nh = ((img.height() as u64 * nw as u64) / img.width().max(1) as u64).max(1) as u32;
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Alpha decides the container, and it is read off the *actual channel* rather
    // than guessed from the input format. A transparent logo must not become a
    // JPEG, which has no alpha and would composite it onto black; conversely a
    // PNG photo whose alpha channel is entirely opaque is just a large JPEG
    // waiting to happen.
    let mut out = Vec::new();
    if has_meaningful_alpha(&img) {
        img.to_rgba8()
            .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|_| "image encode failed")?;
        Ok(Encoded {
            mime: "image/png",
            bytes: out,
        })
    } else {
        let rgb = img.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut out),
            JPEG_QUALITY,
        )
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|_| "image encode failed")?;
        Ok(Encoded {
            mime: "image/jpeg",
            bytes: out,
        })
    }
}

/// True when the image carries an alpha channel *and* actually uses it. Run after
/// downscaling, so the scan is over the smaller buffer.
fn has_meaningful_alpha(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return false;
    }
    img.pixels().any(|(_, _, p)| p[3] < ALPHA_OPAQUE)
}

// ── mc:AlternateContent ──────────────────────────────────────────────────────

/// `mc:AlternateContent` often pairs a metafile `mc:Choice` with a raster
/// `mc:Fallback`. Prefer whichever branch yields a browser-displayable image;
/// returns the chosen `mc:Choice` / `mc:Fallback` child, or `None` when `node` is
/// not an `AlternateContent` or carries no branches.
///
/// Free fidelity when it works: the fallback is usually the PNG Word wrote for
/// consumers that do not understand the `Requires` namespace — exactly our
/// situation — so it beats the placeholder the metafile branch would produce.
pub fn prefer_raster_branch<'a, 'i>(node: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    if node.tag_name().name() != "AlternateContent" {
        return None;
    }
    let mut best: Option<(u8, bool, Node<'a, 'i>)> = None;
    for child in node.children().filter(|c| c.is_element()) {
        let is_fallback = match child.tag_name().name() {
            "Fallback" => true,
            "Choice" => false,
            _ => continue,
        };
        // Rank by picture kind, then prefer Fallback: markup compatibility says a
        // consumer supporting none of the `Requires` namespaces must take the
        // fallback, and we support none of them. Strict `>` keeps document order
        // on a full tie.
        let rank = (image_score(child), is_fallback);
        if best.is_none_or(|(s, f, _)| rank > (s, f)) {
            best = Some((rank.0, rank.1, child));
        }
    }
    best.map(|(_, _, n)| n)
}

/// How likely a branch is to yield something displayable: a DrawingML blip (2) is
/// a real raster part, a VML `imagedata` or an embedded object (1) is usually a
/// metafile, anything else (0) holds no picture at all.
fn image_score(branch: Node) -> u8 {
    let mut score = 0u8;
    for d in branch.descendants().filter(|d| d.is_element()) {
        match d.tag_name().name() {
            "blip" => {
                if super::xml::attr_local(d, "embed").is_some()
                    || super::xml::attr_local(d, "link").is_some()
                {
                    return 2;
                }
            }
            "imagedata" | "OLEObject" | "object" => score = score.max(1),
            _ => {}
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::open_zip;
    use image::{ImageBuffer, Rgba};
    use std::io::Write;
    use std::path::PathBuf;

    // ── fixtures ────────────────────────────────────────────────────────────

    /// `w`x`h` RGBA PNG. `alpha` is applied to one corner block, so a
    /// "transparent" fixture really has transparent pixels and an opaque one
    /// really has none.
    fn rgba_png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
            let a = if x < w / 2 && y < h / 2 { alpha } else { 255 };
            Rgba([(x % 256) as u8, (y % 256) as u8, 40, a])
        });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut std::io::Cursor::new(&mut out), 90)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// A BMP file header declaring `w`x`h` with no pixel data behind it — the
    /// decode-bomb shape (cheap header, ruinous buffer). BMP because it has no
    /// checksum to keep consistent.
    fn bmp_header(w: i32, h: i32) -> Vec<u8> {
        let mut b = Vec::with_capacity(54);
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&0u32.to_le_bytes()); // file size (unused)
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        b.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&24u16.to_le_bytes()); // bpp
        b.extend_from_slice(&0u32.to_le_bytes()); // compression: BI_RGB
        b.extend_from_slice(&0u32.to_le_bytes()); // image byte size
        b.extend_from_slice(&[0u8; 16]); // pixels-per-meter + palette counts
        b
    }

    /// Minimal EMF: an EMR_HEADER record with the " EMF" signature at offset 40.
    fn emf_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 88];
        b[0] = 0x01; // iType = EMR_HEADER
        b[4] = 88; // nSize
        b[40..44].copy_from_slice(b" EMF");
        b
    }

    fn wmf_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[0..4].copy_from_slice(&[0xD7, 0xCD, 0xC6, 0x9A]);
        b
    }

    fn zip_file(tag: &str, entries: &[(&str, Vec<u8>)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "portunus-media-test-{tag}-{}.zip",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn uri_of(m: &Media) -> &str {
        match m {
            Media::DataUri(u) => u.as_str(),
            Media::Placeholder(r) => panic!("expected a data URI, got placeholder: {r}"),
        }
    }

    /// Decodes an image `data:` URI back to pixels, so a test can assert on what
    /// the frame would actually paint.
    fn decode_uri(uri: &str) -> DynamicImage {
        let b64 = uri.split_once(";base64,").expect("base64 data URI").1;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        image::load_from_memory(&bytes).expect("decodable image")
    }

    // ── sniffing ────────────────────────────────────────────────────────────

    #[test]
    fn sniff_recognizes_magic_bytes() {
        assert_eq!(MediaKind::sniff(&rgba_png(4, 4, 255)), MediaKind::Png);
        assert_eq!(MediaKind::sniff(&jpeg_bytes(4, 4)), MediaKind::Jpeg);
        assert_eq!(MediaKind::sniff(b"GIF89a\0\0\0\0"), MediaKind::Gif);
        assert_eq!(MediaKind::sniff(b"GIF87a\0\0\0\0"), MediaKind::Gif);
        assert_eq!(MediaKind::sniff(&bmp_header(4, 4)), MediaKind::Bmp);
        assert_eq!(MediaKind::sniff(b"II\x2A\x00rest"), MediaKind::Tiff);
        assert_eq!(MediaKind::sniff(b"RIFF\0\0\0\0WEBPVP8 "), MediaKind::Webp);
        assert_eq!(MediaKind::sniff(&emf_bytes()), MediaKind::Emf);
        assert_eq!(MediaKind::sniff(&wmf_bytes()), MediaKind::Wmf);
        assert_eq!(
            MediaKind::sniff(b"<?xml version=\"1.0\"?>\n<!-- c --><SVG xmlns=\"x\"/>"),
            MediaKind::Svg
        );
        assert!(MediaKind::Png.browser_ok());
        assert!(MediaKind::Svg.browser_ok());
        assert!(!MediaKind::Tiff.browser_ok());
        assert!(!MediaKind::Emf.browser_ok());
        assert!(!MediaKind::Unknown.browser_ok());
    }

    #[test]
    fn sniff_handles_empty_and_truncated_input() {
        assert_eq!(MediaKind::sniff(&[]), MediaKind::Unknown);
        assert_eq!(MediaKind::sniff(&[0x89, 0x50]), MediaKind::Unknown);
        assert_eq!(MediaKind::sniff(b"BM"), MediaKind::Unknown); // header too short
        assert_eq!(MediaKind::sniff(b"GIF8"), MediaKind::Unknown);
        assert_eq!(MediaKind::sniff(b"RIFF\0\0\0\0"), MediaKind::Unknown);
        assert_eq!(MediaKind::sniff(b"Widget"), MediaKind::Unknown);
    }

    #[test]
    fn part_name_is_ignored_in_favour_of_magic_bytes() {
        // A part called image1.png holding JPEG bytes: the name is document-
        // controlled metadata, the magic bytes are the truth.
        let jpeg = jpeg_bytes(40, 20);
        assert_eq!(MediaKind::sniff(&jpeg), MediaKind::Jpeg);
        let path = zip_file("liar", &[("word/media/image1.png", jpeg)]);
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        let m = c.get(&mut zip, &mut b, &mut mb, "word/media/image1.png", 200);
        assert!(
            uri_of(&m).starts_with("data:image/jpeg;base64,"),
            "{}",
            &uri_of(&m)[..32.min(uri_of(&m).len())]
        );
        let _ = std::fs::remove_file(&path);
    }

    // ── placeholders ────────────────────────────────────────────────────────

    #[test]
    fn metafiles_become_placeholders_with_a_note() {
        let path = zip_file(
            "metafile",
            &[
                ("word/media/chart.emf", emf_bytes()),
                ("word/media/eq.wmf", wmf_bytes()),
            ],
        );
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        for part in ["word/media/chart.emf", "word/media/eq.wmf"] {
            match c.get(&mut zip, &mut b, &mut mb, part, 400) {
                Media::Placeholder(r) => assert!(r.contains("EMF/WMF"), "{r}"),
                Media::DataUri(u) => panic!("a metafile must not be emitted: {u}"),
            }
        }
        assert_eq!(mb.omitted(), 2);
        assert_eq!(mb.notes(), &[NOTE_VECTOR]); // one note, not one per image
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_bytes_yield_a_placeholder_without_panicking() {
        // Valid PNG magic, garbage behind it.
        let mut corrupt = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend_from_slice(b"not really a png at all");
        let path = zip_file(
            "corrupt",
            &[
                ("word/media/bad.png", corrupt),
                ("word/media/mystery.dat", b"Widget".to_vec()),
            ],
        );
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        assert!(matches!(
            c.get(&mut zip, &mut b, &mut mb, "word/media/bad.png", 300),
            Media::Placeholder(_)
        ));
        assert!(matches!(
            c.get(&mut zip, &mut b, &mut mb, "word/media/mystery.dat", 300),
            Media::Placeholder(_)
        ));
        // A missing part is a placeholder too, not an error.
        assert!(matches!(
            c.get(&mut zip, &mut b, &mut mb, "word/media/absent.png", 300),
            Media::Placeholder(_)
        ));
        assert_eq!(mb.omitted(), 3);
        assert_eq!(mb.spent(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn absurd_declared_dimensions_are_rejected_from_the_header() {
        // 64000x64000 would be ~16 GB of RGBA. The verdict has to come from the
        // header, so this test also has to finish instantly.
        let path = zip_file("bomb", &[("word/media/bomb.bmp", bmp_header(64000, 64000))]);
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        let t = std::time::Instant::now();
        match c.get(&mut zip, &mut b, &mut mb, "word/media/bomb.bmp", 800) {
            // The pixel-count guard is what must refuse it, not a later decode
            // error — by then the buffer would already have been reserved.
            Media::Placeholder(r) => assert_eq!(r, "image too large"),
            Media::DataUri(_) => panic!("a 64000x64000 declaration must be refused"),
        }
        assert!(
            t.elapsed().as_secs() < 5,
            "the decode bomb was not rejected early"
        );
        assert_eq!(mb.spent(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budget_refuses_past_the_total_cap_and_records_a_note() {
        let path = zip_file("budget", &[("word/media/a.png", rgba_png(300, 300, 255))]);
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut c) = (Budget::new(), MediaCache::new());
        let mut mb = MediaBudget::with_caps(1 << 20, 64); // 64 bytes of document budget
        match c.get(&mut zip, &mut b, &mut mb, "word/media/a.png", 300) {
            Media::Placeholder(r) => assert_eq!(r, "image budget reached"),
            Media::DataUri(u) => panic!("must not fit in 64 bytes: {} bytes", u.len()),
        }
        assert_eq!(mb.notes(), &[NOTE_BUDGET]);
        assert_eq!(mb.omitted(), 1);
        assert_eq!(mb.spent(), 0, "a refusal must not consume budget");

        // Same image, refused by the per-image cap this time. A size refusal is
        // deliberately not cached as a hard failure, so the second `get` really
        // re-encodes.
        let mut mb = MediaBudget::with_caps(64, 1 << 20);
        match c.get(&mut zip, &mut b, &mut mb, "word/media/a.png", 300) {
            Media::Placeholder(r) => assert_eq!(r, "image too large"),
            Media::DataUri(_) => panic!("must not fit the per-image cap"),
        }
        assert_eq!(mb.notes(), &[NOTE_TOO_LARGE]);
        let _ = std::fs::remove_file(&path);
    }

    // ── caching ─────────────────────────────────────────────────────────────

    #[test]
    fn cache_returns_the_same_rc_for_the_same_part_and_size() {
        let path = zip_file("cache", &[("ppt/media/logo.png", rgba_png(200, 100, 255))]);
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        let first = c.get(&mut zip, &mut b, &mut mb, "ppt/media/logo.png", 120);
        let spent_once = mb.spent();
        let second = c.get(&mut zip, &mut b, &mut mb, "ppt/media/logo.png", 120);
        let (Media::DataUri(first), Media::DataUri(second)) = (&first, &second) else {
            panic!("both must be data URIs");
        };
        assert!(Rc::ptr_eq(first, second), "the second get must reuse the encode");
        assert_eq!(
            mb.spent(),
            spent_once,
            "a cache hit must not be charged twice"
        );
        // A different display size is a different encode.
        let bigger = c.get(&mut zip, &mut b, &mut mb, "ppt/media/logo.png", 200);
        let Media::DataUri(bigger) = &bigger else {
            panic!("data URI")
        };
        assert!(!Rc::ptr_eq(first, bigger));
        let _ = std::fs::remove_file(&path);
    }

    // ── encoding decisions ──────────────────────────────────────────────────

    #[test]
    fn transparency_picks_png_and_opacity_picks_jpeg() {
        let path = zip_file(
            "alpha",
            &[
                ("word/media/logo.png", rgba_png(120, 120, 0)),
                ("word/media/photo.png", rgba_png(120, 120, 255)),
            ],
        );
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        let transparent = c.get(&mut zip, &mut b, &mut mb, "word/media/logo.png", 120);
        assert!(
            uri_of(&transparent).starts_with("data:image/png;base64,"),
            "real alpha must stay PNG"
        );
        // Both inputs are PNG, so the choice can only be coming from the alpha
        // channel and not from the input format.
        let opaque = c.get(&mut zip, &mut b, &mut mb, "word/media/photo.png", 120);
        assert!(
            uri_of(&opaque).starts_with("data:image/jpeg;base64,"),
            "an opaque PNG must be re-encoded as JPEG"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn downscales_to_the_display_size_but_never_upscales() {
        let path = zip_file(
            "scale",
            &[
                ("word/media/big.png", rgba_png(400, 200, 255)),
                ("word/media/small.png", rgba_png(50, 25, 255)),
            ],
        );
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());

        let big = c.get(&mut zip, &mut b, &mut mb, "word/media/big.png", 100);
        let img = decode_uri(uri_of(&big));
        assert_eq!((img.width(), img.height()), (100, 50), "aspect ratio kept");

        let small = c.get(&mut zip, &mut b, &mut mb, "word/media/small.png", 400);
        let img = decode_uri(uri_of(&small));
        assert_eq!((img.width(), img.height()), (50, 25), "must not upscale");

        // A document-declared extent cannot ask for an arbitrarily large encode.
        let clamped = c.get(&mut zip, &mut b, &mut mb, "word/media/big.png", 100_000);
        assert_eq!(decode_uri(uri_of(&clamped)).width(), 400);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn small_gifs_pass_through_but_oversized_ones_are_re_encoded() {
        let gif = |w: u32, h: u32| {
            let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_fn(w, h, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, 7, 255]));
            let mut out = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Gif)
                .unwrap();
            out
        };
        let small = gif(20, 10);
        let path = zip_file(
            "gif",
            &[
                ("ppt/media/spin.gif", small.clone()),
                ("ppt/media/wide.gif", gif(400, 200)),
            ],
        );
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());

        // Fits its box: bytes go through untouched, so an animation survives.
        let m = c.get(&mut zip, &mut b, &mut mb, "ppt/media/spin.gif", 100);
        let uri = uri_of(&m);
        assert!(uri.starts_with("data:image/gif;base64,"), "{}", &uri[..24]);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(uri.split_once(";base64,").unwrap().1)
            .unwrap();
        assert_eq!(decoded, small);

        // Needs downscaling, so it is re-encoded (and flattened) like any raster.
        let m = c.get(&mut zip, &mut b, &mut mb, "ppt/media/wide.gif", 100);
        assert!(uri_of(&m).starts_with("data:image/jpeg;base64,"));
        assert_eq!(decode_uri(uri_of(&m)).width(), 100);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn svg_passes_through_as_a_base64_data_uri() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="4" height="4"/></svg>"#;
        let path = zip_file("svg", &[("word/media/shape.svg", svg.to_vec())]);
        let mut zip = open_zip(path.to_str().unwrap()).unwrap();
        let (mut b, mut mb, mut c) = (Budget::new(), MediaBudget::new(), MediaCache::new());
        let m = c.get(&mut zip, &mut b, &mut mb, "word/media/shape.svg", 200);
        let uri = uri_of(&m);
        assert!(uri.starts_with("data:image/svg+xml;base64,"), "{uri}");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(uri.split_once(";base64,").unwrap().1)
            .unwrap();
        assert_eq!(decoded, svg.to_vec(), "markup must pass through unchanged");
        let _ = std::fs::remove_file(&path);
    }

    // ── mc:AlternateContent ─────────────────────────────────────────────────

    fn alternate_content<'a>(doc: &'a roxmltree::Document<'a>) -> Node<'a, 'a> {
        doc.descendants()
            .find(|n| n.tag_name().name() == "AlternateContent")
            .expect("AlternateContent")
    }

    #[test]
    fn prefers_the_branch_holding_a_raster_picture() {
        let xml = r#"<root xmlns:mc="m" xmlns:a="a" xmlns:v="v" xmlns:r="r">
          <mc:AlternateContent>
            <mc:Choice Requires="wps"><v:imagedata r:id="rId9"/></mc:Choice>
            <mc:Fallback><a:blip r:embed="rId4"/></mc:Fallback>
          </mc:AlternateContent>
        </root>"#;
        let doc = crate::xml::parse(xml).unwrap();
        assert_eq!(
            prefer_raster_branch(alternate_content(&doc))
                .unwrap()
                .tag_name()
                .name(),
            "Fallback"
        );

        // Reversed: the raster sits in the Choice this time.
        let xml = r#"<root xmlns:mc="m" xmlns:a="a" xmlns:v="v" xmlns:r="r">
          <mc:AlternateContent>
            <mc:Choice Requires="c"><a:blip r:embed="rId1"/></mc:Choice>
            <mc:Fallback><v:imagedata r:id="rId2"/></mc:Fallback>
          </mc:AlternateContent>
        </root>"#;
        let doc = crate::xml::parse(xml).unwrap();
        assert_eq!(
            prefer_raster_branch(alternate_content(&doc))
                .unwrap()
                .tag_name()
                .name(),
            "Choice"
        );

        // Neither branch holds a picture: markup compatibility says take the
        // fallback, because we support no `Requires` namespace.
        let xml = r#"<root xmlns:mc="m" xmlns:w="w">
          <mc:AlternateContent>
            <mc:Choice Requires="c"><w:t>Widget</w:t></mc:Choice>
            <mc:Fallback><w:t>Sheet1</w:t></mc:Fallback>
          </mc:AlternateContent>
        </root>"#;
        let doc = crate::xml::parse(xml).unwrap();
        assert_eq!(
            prefer_raster_branch(alternate_content(&doc))
                .unwrap()
                .tag_name()
                .name(),
            "Fallback"
        );

        // Not an AlternateContent at all.
        assert!(prefer_raster_branch(doc.root_element()).is_none());
    }
}
