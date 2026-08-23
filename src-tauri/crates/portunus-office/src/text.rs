//! Flat text extraction (for content indexing).
//!
//! Extraction is **per section**, and a section is the same thing the renderer
//! calls one: a worksheet, a slide, or — for a text document, which the renderer
//! draws as one continuous column — the whole body. The index stores each
//! section's text separately so a preview can open on the section that matched
//! rather than on the first one (`content_index::best_section`).
//!
//! That makes section *order* load-bearing: index 3 has to be the same slide the
//! renderer draws for `section = 3`, or the preview opens on the wrong one. Part
//! names carry no order — the second sheet of a workbook is routinely stored as
//! `sheet1.xml` — so each format's extractor reuses its renderer's own
//! enumeration (`xl/workbook.xml`'s `<sheets>`, `p:sldIdLst`, document order for
//! ODF) rather than sorting the archive. `extract_office_sections` and
//! `office::render` are checked against each other by test.

use super::{docx, odf, pptx, xlsx};
use std::path::Path;

/// Separator between sections in the flat text, and the character
/// `content_index::doc_sections` splits on. Form feed, matching the page
/// separator `pdftotext` already emits — the index treats a PDF page and an
/// office section as the same thing.
pub const SECTION_SEP: char = '\u{000C}';

/// Chars kept per section, and per document. Extraction input is already bounded
/// (`content.max_file_bytes`, then `pkg::Budget`), but decompressed text is not:
/// a 200×200 grid of long strings would otherwise hand FTS5 a multi-megabyte row.
/// Past the cap the text is cut, never the section — section indices must stay
/// aligned with the renderer's.
const MAX_SECTION_CHARS: usize = 512 * 1024;
const MAX_DOC_CHARS: usize = 8 * 1024 * 1024;

/// One indexable section: the renderer's name for it, plus its flat text.
pub struct Section {
    /// Sheet name or slide title; empty for a text document, which has one
    /// unnamed section.
    pub name: String,
    pub text: String,
}

impl Section {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Section {
        let mut s = Section {
            name: name.into(),
            text: text.into(),
        };
        // A form feed inside a section's own text would split it in two at index
        // time and desync every later section, so it is folded to a newline here
        // rather than trusted not to occur.
        if s.text.contains(SECTION_SEP) {
            s.text = s.text.replace(SECTION_SEP, "\n");
        }
        if s.name.contains(SECTION_SEP) {
            s.name = s.name.replace(SECTION_SEP, " ");
        }
        clip(&mut s.text, MAX_SECTION_CHARS);
        s
    }

    /// The section as the index stores it: the name is searchable text too, so a
    /// query matching a sheet name lands on that sheet.
    fn indexed(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            self.text.clone()
        } else if self.text.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name}\n{}", self.text)
        }
    }
}

/// Every section of an office document, in the renderer's section order.
pub fn extract_office_sections(path: &str) -> Result<Vec<Section>, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "docx" => docx::extract_sections(path),
        "pptx" => pptx::extract_sections(path),
        "xlsx" => xlsx::extract_sections(path),
        // One entry point for the three ODF classes, as in `office::render`: the
        // package's `office:body` decides which extractor runs, not the extension.
        "odt" | "ods" | "odp" => odf::extract_sections(path),
        other => Err(format!("unsupported office extension: {other}")),
    }
}

/// The whole document as flat text, sections separated by [`SECTION_SEP`].
pub fn extract_office_text(path: &str) -> Result<String, String> {
    let sections = extract_office_sections(path)?;
    let mut out = String::new();
    for s in &sections {
        if !out.is_empty() {
            out.push(SECTION_SEP);
        }
        out.push_str(&s.indexed());
        if out.chars().count() >= MAX_DOC_CHARS {
            clip(&mut out, MAX_DOC_CHARS);
            break;
        }
    }
    Ok(out)
}

/// Truncates `s` to `max` chars, on a char boundary.
fn clip(s: &mut String, max: usize) {
    if let Some((i, _)) = s.char_indices().nth(max) {
        s.truncate(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_puts_the_section_name_in_the_searchable_text() {
        let s = Section::new("Widget prices", "café 12");
        assert_eq!(s.indexed(), "Widget prices\ncafé 12");
        // A named but empty section is still findable by its name alone.
        assert_eq!(Section::new("Sheet1", "  ").indexed(), "Sheet1");
        // An unnamed section (a text document) contributes only its body.
        assert_eq!(Section::new("", "naïve").indexed(), "naïve");
    }

    #[test]
    fn a_form_feed_in_a_section_cannot_split_it() {
        let s = Section::new("a\u{000C}b", "one\u{000C}two");
        assert_eq!(s.name, "a b");
        assert_eq!(s.text, "one\ntwo");
    }

    #[test]
    fn section_text_is_clipped_on_a_char_boundary() {
        let s = Section::new("", "é".repeat(MAX_SECTION_CHARS + 10));
        assert_eq!(s.text.chars().count(), MAX_SECTION_CHARS);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        assert!(extract_office_sections("/tmp/x.pages").is_err());
    }
}
