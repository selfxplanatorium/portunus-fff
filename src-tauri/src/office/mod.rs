//! Office document reading (docx/pptx/xlsx and the ODF equivalents): flat text
//! for the content index, and styled HTML for the preview.

mod cellstyle;
mod docshape;
mod docx;
mod drawingml;
mod emit;
mod fonts;
mod highlight;
mod html;
mod link;
mod listnum;
mod media;
mod model;
mod numfmt;
mod odf;
mod opc;
mod pkg;
mod pptx;
mod sheet;
mod sheetmodel;
mod slideshape;
mod text;
mod xlsx;
mod xml;

pub use text::{extract_office_text, SECTION_SEP};

pub const OFFICE_EXTENSIONS: &[&str] = &["docx", "pptx", "xlsx", "odt", "ods", "odp"];

pub fn is_office_ext(ext: &str) -> bool {
    OFFICE_EXTENSIONS.contains(&ext)
}

// ── rendered preview ─────────────────────────────────────────────────────────

/// Which renderer produced a document, and therefore how the frontend presents
/// it: a flowing page column, a scrolling sheet, or a fixed-geometry slide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    Doc,
    Sheet,
    Slide,
}

/// One rendered section of an office document.
///
/// `html` is a body fragment, escaped by construction — it is never parsed from
/// document markup, only built by `html::Writer`. The frontend drops it into a
/// sandboxed iframe, so it must be self-contained apart from the theme custom
/// properties the host injects.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficeDoc {
    pub html: String,
    pub shape: Shape,
    /// Sheet names / slide titles. Length is the section count.
    pub sections: Vec<String>,
    /// Which section `html` covers.
    pub section: u32,
    /// Slide canvas size in CSS px at 96dpi; `None` for docs and sheets.
    pub natural: Option<(f32, f32)>,
    /// docx page width and padding in CSS px; `None` for sheets and slides.
    pub page: Option<(f32, f32, f32)>,
    /// Id of the mark the frontend should scroll to. Marks carry stable ids and
    /// the best cluster is only known once the whole section is rendered, so the
    /// winner is reported here rather than as an `id="pmatch"` in the markup.
    pub best_mark_id: Option<String>,
    pub truncated: bool,
    /// Degradation notes for a muted footer — charts without a raster fallback,
    /// clipped rows, missing parts. Surfaced rather than hidden.
    pub notes: Vec<String>,
}

/// Render one section of an office document to HTML.
pub fn render(path: &str, section: Option<u32>, terms: &[String]) -> Result<OfficeDoc, String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "xlsx" => xlsx::render(path, section, terms),
        "pptx" => pptx::render(path, section, terms),
        "docx" => docx::render(path, section, terms),
        // One entry point for the three ODF classes: the package's own
        // `office:body` decides which renderer runs, not the extension, so a
        // renamed file renders as what it is (see `odf::pkg::Class`).
        "odt" | "ods" | "odp" => odf::render(path, section, terms),
        other => Err(format!("unsupported office extension: {other}")),
    }
}
