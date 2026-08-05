//! xlsx → styled HTML, one worksheet per call.
//!
//! Sheet identity comes from `xl/workbook.xml` — the `<sheets>` order, resolved
//! through `xl/_rels/workbook.xml.rels` — and never from part names. Part names
//! carry no ordering: the second sheet of a workbook is routinely stored as
//! `sheet1.xml`, so reading a fixed path shows the wrong sheet.

mod sheet;
mod styles;

use super::drawingml::theme::Theme;
use super::emit::{self, Notes};
use super::highlight::{Marker, Terms};
use super::media::{MediaBudget, MediaCache};
use super::pkg::{self, Budget, Zip};
use super::text::Section;
use super::xml::{child, elems, truthy};
use super::{opc, xml, OfficeDoc, Shape};
use styles::Styles;

/// Excel's own grid limits, used to reject nonsense references rather than to
/// bound emission (see `MAX_ROWS`/`MAX_COLS` for that).
pub const MAX_ROW_NUMBER: u32 = 1_048_576;
const MAX_COL_NUMBER: usize = 16_384;

/// Emission bounds. Past these the grid is clipped and a note says so. The
/// `Writer`'s byte cap is the real backstop — these keep the row/column loops
/// from walking a million-row sheet, and keep the DOM small enough that the
/// reader's zoom stays smooth (every cell is a box the frame has to lay out).
///
/// The row cap is deliberately tight. A preview is for recognising a file, not
/// reading it; the honest note beats a grid nobody can scroll.
pub const MAX_ROWS: u32 = 200;
pub const MAX_COLS: usize = 200;

/// Byte cap for the emitted body HTML.
pub const HTML_CAP: usize = 6 * 1024 * 1024;

const MAX_SHEETS: usize = 256;
const MAX_SHARED_STRINGS: usize = 200_000;
/// Shared strings are truncated on the way into the pool, so a workbook whose
/// table is one 32k-character string per entry cannot dominate memory.
const MAX_SST_CHARS: usize = 1024;
const MAX_SHEET_NAME_CHARS: usize = 64;

/// One entry of the workbook's `<sheets>` list.
pub struct SheetRef {
    pub name: String,
    /// The worksheet part, or `None` when the `r:id` did not resolve — the sheet
    /// keeps its place in the section list either way, so section indices stay
    /// aligned with the workbook's own order.
    pub part: Option<String>,
    pub hidden: bool,
}

/// Everything one sheet render needs. Grouped so the helpers can take disjoint
/// mutable borrows of the pieces they touch.
pub struct Ctx<'a> {
    pub zip: &'a mut Zip,
    pub budget: &'a mut Budget,
    pub styles: &'a Styles,
    pub sst: &'a [String],
    pub date1904: bool,
    pub terms: &'a Terms,
    pub marker: &'a mut Marker,
    pub media: &'a mut MediaCache,
    pub mb: &'a mut MediaBudget,
    pub notes: &'a mut Notes,
    /// Byte cap for the emitted HTML. A field rather than a constant so the tests
    /// can reach the truncation path without generating megabytes of fixture.
    pub html_cap: usize,
}

pub fn render(path: &str, section: Option<u32>, terms: &[String]) -> Result<OfficeDoc, String> {
    render_capped(path, section, terms, HTML_CAP)
}

fn render_capped(
    path: &str,
    section: Option<u32>,
    terms: &[String],
    html_cap: usize,
) -> Result<OfficeDoc, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();
    let mut notes = Notes::new();

    let wb_part = opc::root_part(&mut zip, &mut budget, "xl/workbook.xml");
    let wb_xml = pkg::read_entry(&mut zip, &wb_part, &mut budget)?
        .ok_or_else(|| format!("xlsx: missing workbook part ({wb_part})"))?;
    let wb_rels = opc::read_rels(&mut zip, &wb_part, &mut budget)?;
    let (sheets, date1904) = parse_workbook(&wb_xml, &wb_part, &wb_rels)?;

    // A hidden sheet is never the default view, but it stays in the section list so
    // the indices the frontend hands back keep matching the workbook's order.
    let hidden: Vec<&str> = sheets
        .iter()
        .filter(|s| s.hidden)
        .map(|s| s.name.as_str())
        .take(8)
        .collect();
    if !hidden.is_empty() {
        notes.add(&format!("Hidden sheets: {}", hidden.join(", ")));
    }
    let default_idx = sheets.iter().position(|s| !s.hidden).unwrap_or(0) as u32;
    let last = sheets.len().saturating_sub(1) as u32;
    let idx = section.map(|s| s.min(last)).unwrap_or(default_idx);

    let theme = match opc::part_by_kind(&wb_rels, &wb_part, "/theme") {
        Some(p) => match pkg::read_entry(&mut zip, &p, &mut budget)? {
            Some(x) => Theme::parse(&x).unwrap_or_default(),
            None => Theme::default(),
        },
        None => Theme::default(),
    };

    let styles_part = opc::part_by_kind(&wb_rels, &wb_part, "/styles")
        .unwrap_or_else(|| "xl/styles.xml".to_string());
    let styles = match pkg::read_entry(&mut zip, &styles_part, &mut budget)? {
        Some(x) => match Styles::parse(&x, &theme) {
            Ok(s) => s,
            Err(_) => {
                notes.add("Cell styles unreadable");
                Styles::empty()
            }
        },
        None => {
            notes.add("Cell styles unreadable");
            Styles::empty()
        }
    };

    let sst_part = opc::part_by_kind(&wb_rels, &wb_part, "/sharedStrings")
        .unwrap_or_else(|| "xl/sharedStrings.xml".to_string());
    let sst = match pkg::read_entry(&mut zip, &sst_part, &mut budget)? {
        Some(x) => shared_strings(&x).unwrap_or_default(),
        None => Vec::new(),
    };

    let sections: Vec<String> = sheets.iter().map(|s| s.name.clone()).collect();
    let query = Terms::new(terms);
    let mut marker = Marker::new();
    let mut media = MediaCache::new();
    let mut mb = MediaBudget::new();

    let out = {
        let mut ctx = Ctx {
            zip: &mut zip,
            budget: &mut budget,
            styles: &styles,
            sst: &sst,
            date1904,
            terms: &query,
            marker: &mut marker,
            media: &mut media,
            mb: &mut mb,
            notes: &mut notes,
            html_cap,
        };
        let sh = &sheets[idx as usize];
        match sheet::render(&mut ctx, sh) {
            Ok(o) => o,
            Err(e) => {
                let msg = emit::degrade_msg(&e, "sheet");
                ctx.notes.add(&msg);
                sheet::SheetOut {
                    html: super::sheet::error_body(&msg),
                    truncated: true,
                }
            }
        }
    };

    for n in mb.notes() {
        notes.add(n);
    }

    Ok(OfficeDoc {
        html: out.html,
        shape: Shape::Sheet,
        sections,
        section: idx,
        // A sheet has no intrinsic page or canvas size; it is as wide as its
        // columns and the frontend scrolls it.
        natural: None,
        page: None,
        best_mark_id: marker.best_mark_id(),
        truncated: out.truncated,
        notes: notes.into_vec(),
    })
}

// ── content index ────────────────────────────────────────────────────────────

/// Flat text for the content index: one section per worksheet, in `<sheets>`
/// order — the same order [`render`] uses, so section 3 of the index is the sheet
/// the reader opens for `section = 3`. A hidden sheet keeps its place, as it does
/// in the tab strip.
///
/// Cells go through the same [`sheet::cell_text`] the renderer uses, so a date
/// indexes as the date it displays rather than as its serial, a shared string
/// resolves through the pool, and an inline or cached-formula string is read
/// rather than skipped. The row and column caps are the renderer's, so anything
/// found here is something the preview can show and highlight.
pub(super) fn extract_sections(path: &str) -> Result<Vec<Section>, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();

    let wb_part = opc::root_part(&mut zip, &mut budget, "xl/workbook.xml");
    let wb_xml = pkg::read_entry(&mut zip, &wb_part, &mut budget)?
        .ok_or_else(|| format!("xlsx: missing workbook part ({wb_part})"))?;
    let wb_rels = opc::read_rels(&mut zip, &wb_part, &mut budget)?;
    let (sheets, date1904) = parse_workbook(&wb_xml, &wb_part, &wb_rels)?;

    // Number formats only: no theme, so `Styles::parse` resolves theme colours to
    // nothing. Indexing reads `fmt` and never a colour.
    let styles_part = opc::part_by_kind(&wb_rels, &wb_part, "/styles")
        .unwrap_or_else(|| "xl/styles.xml".to_string());
    let styles = match pkg::read_entry(&mut zip, &styles_part, &mut budget) {
        Ok(Some(x)) => Styles::parse(&x, &Theme::default()).unwrap_or_else(|_| Styles::empty()),
        _ => Styles::empty(),
    };

    let sst_part = opc::part_by_kind(&wb_rels, &wb_part, "/sharedStrings")
        .unwrap_or_else(|| "xl/sharedStrings.xml".to_string());
    let sst = match pkg::read_entry(&mut zip, &sst_part, &mut budget) {
        Ok(Some(x)) => shared_strings(&x).unwrap_or_default(),
        _ => Vec::new(),
    };

    let mut out = Vec::with_capacity(sheets.len());
    for sh in &sheets {
        let xml_str = match &sh.part {
            Some(p) => match pkg::read_entry(&mut zip, p, &mut budget) {
                Ok(Some(x)) => Some(x),
                // A sheet that cannot be read keeps its place with empty text:
                // dropping it would shift every later sheet's section index.
                _ => None,
            },
            None => None,
        };
        let text = match &xml_str {
            Some(x) => sheet_text(x, &styles, &sst, date1904).unwrap_or_default(),
            None => String::new(),
        };
        out.push(Section::new(sh.name.clone(), text));
    }
    Ok(out)
}

/// One worksheet's cells as text, rows separated by newlines and cells by tabs.
///
/// Tabs rather than spaces so a numeric grid keeps its cells as separate tokens
/// (`porter unicode61` splits on both, but a tab survives a later change of mind
/// about cell joining, and it is what a copied selection uses).
fn sheet_text(
    xml_str: &str,
    styles: &Styles,
    sst: &[String],
    date1904: bool,
) -> Result<String, String> {
    let doc = xml::parse(xml_str)?;
    let Some(data) = xml::descendant(doc.root_element(), "sheetData") else {
        return Ok(String::new());
    };
    // `cell_text` reports a short shared-string pool through `notes`; the index has
    // no footer to show it in, and the renderer will report it when the sheet is
    // previewed.
    let mut discard = Notes::new();
    let mut out = String::new();
    for row in elems(data).filter(|n| n.tag_name().name() == "row") {
        // Clipped exactly as the renderer clips: a row past `MAX_ROWS` is not
        // drawn, so a hit in it could never be highlighted.
        if xml::attr_u32(row, "r").is_some_and(|r| r > MAX_ROWS) {
            break;
        }
        let mut cells = 0usize;
        for cell in elems(row).filter(|n| n.tag_name().name() == "c") {
            let col = xml::attr_local(cell, "r")
                .map(|r| split_cell_ref(r).0)
                .and_then(col_letter_to_index);
            if col.is_some_and(|c| c >= MAX_COLS) {
                continue;
            }
            let style_id = xml::attr_u32(cell, "s").unwrap_or(0);
            let fmt = &styles.get(style_id).fmt;
            let (text, _) = sheet::cell_text(cell, fmt, sst, date1904, &mut discard);
            if text.is_empty() {
                continue;
            }
            if cells > 0 {
                out.push('\t');
            }
            out.push_str(&text);
            cells += 1;
        }
        if cells > 0 {
            out.push('\n');
        }
    }
    Ok(out)
}

// ── workbook ─────────────────────────────────────────────────────────────────

fn parse_workbook(
    wb_xml: &str,
    wb_part: &str,
    rels: &opc::Rels,
) -> Result<(Vec<SheetRef>, bool), String> {
    let doc = xml::parse(wb_xml)?;
    let root = doc.root_element();
    // The 1904 date system shifts every serial by 1462 days; missing it puts every
    // date in the workbook four years and a day off.
    let date1904 = child(root, "workbookPr")
        .and_then(|n| xml::attr_local(n, "date1904"))
        .map(truthy)
        .unwrap_or(false);

    let mut sheets = Vec::new();
    if let Some(list) = child(root, "sheets") {
        for (i, s) in elems(list)
            .filter(|n| n.tag_name().name() == "sheet")
            .take(MAX_SHEETS)
            .enumerate()
        {
            let raw = xml::attr_local(s, "name").unwrap_or("").trim();
            let name = if raw.is_empty() {
                format!("Sheet{}", i + 1)
            } else {
                clip_chars(raw, MAX_SHEET_NAME_CHARS)
            };
            let state = xml::attr_local(s, "state").unwrap_or("visible");
            let hidden =
                state.eq_ignore_ascii_case("hidden") || state.eq_ignore_ascii_case("veryHidden");
            let part = xml::attr_local(s, "id")
                .and_then(|id| rels.get(id))
                .filter(|r| !r.external)
                .and_then(|r| opc::resolve_target(wb_part, &r.target));
            sheets.push(SheetRef { name, part, hidden });
        }
    }
    if sheets.is_empty() {
        return Err("xlsx: the workbook declares no sheets".to_string());
    }
    Ok((sheets, date1904))
}

/// `xl/sharedStrings.xml` → the string pool, indexed by `<si>` position.
fn shared_strings(sst_xml: &str) -> Result<Vec<String>, String> {
    let doc = xml::parse(sst_xml)?;
    Ok(elems(doc.root_element())
        .filter(|n| n.tag_name().name() == "si")
        .take(MAX_SHARED_STRINGS)
        .map(|si| clip_chars(&sheet::rich_text(si), MAX_SST_CHARS))
        .collect())
}

// ── cell references ──────────────────────────────────────────────────────────

/// Excel column letters → 0-based index ("A"→0, "Z"→25, "AA"→26, "XFD"→16383).
///
/// Bounded on purpose: the letter run comes from document XML, and an unbounded
/// `idx * 26` overflows `usize` on a long one — a debug-build panic on a malformed
/// cell reference. Anything past Excel's last column is not a reference.
pub fn col_letter_to_index(col: &str) -> Option<usize> {
    let mut idx: usize = 0;
    for ch in col.bytes() {
        if !ch.is_ascii_alphabetic() {
            break;
        }
        idx = idx * 26 + (ch.to_ascii_uppercase() - b'A') as usize + 1;
        if idx > MAX_COL_NUMBER {
            return None;
        }
    }
    if idx == 0 {
        None
    } else {
        Some(idx - 1)
    }
}

/// Split a cell reference like "AB12" into its letter prefix and digit suffix.
pub fn split_cell_ref(r: &str) -> (&str, &str) {
    let split = r.find(|c: char| c.is_ascii_digit()).unwrap_or(r.len());
    (&r[..split], &r[split..])
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => {
            let mut out = s[..i].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::sheet::col_letter;
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    // ── fixtures ────────────────────────────────────────────────────────────

    const NS: &str = "xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"";

    /// A `Zip` is `ZipArchive<BufReader<File>>`, so a fixture package has to be
    /// file-backed. Removed on drop.
    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    impl Fixture {
        fn new(tag: &str, entries: &[(&str, String)]) -> Fixture {
            // Tests build a base part list and append overrides, so a repeated
            // name means "replace" - zip itself rejects a duplicate entry.
            // Walk backwards keeping the first sighting (i.e. the last write),
            // then restore document order.
            let mut seen = std::collections::HashSet::new();
            let mut parts: Vec<&(&str, String)> = entries
                .iter()
                .rev()
                .filter(|(name, _)| seen.insert(*name))
                .collect();
            parts.reverse();

            // Tests run in parallel in one process, so the pid alone does not
            // make this unique - a shared tag would have two tests writing the
            // same file.
            static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "portunus-xlsx-{tag}-{}-{n}.xlsx",
                std::process::id()
            ));
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in parts {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
            Fixture(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn render(&self, section: Option<u32>) -> OfficeDoc {
            super::render(self.path(), section, &[]).expect("render")
        }
    }

    /// `(name, rId, state)` per sheet.
    fn workbook(pr: &str, sheets: &[(&str, &str, &str)]) -> String {
        let items: String = sheets
            .iter()
            .enumerate()
            .map(|(i, (name, rid, state))| {
                let st = if state.is_empty() {
                    String::new()
                } else {
                    format!(" state=\"{state}\"")
                };
                format!(
                    "<sheet name=\"{name}\" sheetId=\"{}\" r:id=\"{rid}\"{st}/>",
                    i + 1
                )
            })
            .collect();
        format!("<workbook {NS}>{pr}<sheets>{items}</sheets></workbook>")
    }

    /// `(rId, relationship-kind, target)`.
    fn rels(items: &[(&str, &str, &str)]) -> String {
        let body: String = items
            .iter()
            .map(|(id, kind, target)| {
                format!(
                    "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}\" Target=\"{target}\"/>"
                )
            })
            .collect();
        format!(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>"
        )
    }

    fn worksheet(body: &str) -> String {
        format!("<worksheet {NS}>{body}</worksheet>")
    }

    fn styles_xml(body: &str) -> String {
        format!("<styleSheet {NS}>{body}</styleSheet>")
    }

    fn sst_xml(items: &[&str]) -> String {
        let body: String = items.iter().map(|i| format!("<si>{i}</si>")).collect();
        format!("<sst {NS}>{body}</sst>")
    }

    /// A one-sheet workbook whose sheet body is `body`; `extra` adds parts.
    fn single(tag: &str, body: &str, extra: &[(&str, String)]) -> Fixture {
        let mut parts = vec![
            (
                "xl/workbook.xml",
                workbook("", &[("Sheet1", "rId1", "")]),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                rels(&[("rId1", "worksheet", "worksheets/sheet1.xml")]),
            ),
            ("xl/worksheets/sheet1.xml", worksheet(body)),
            ("xl/styles.xml", styles_xml("<cellXfs count=\"1\"><xf/></cellXfs>")),
        ];
        parts.extend(extra.iter().cloned());
        Fixture::new(tag, &parts)
    }

    /// The emitted markup with the leading `<style>` block dropped. Class names
    /// appear in both, so an assertion about what the *grid* contains has to look
    /// past the stylesheet — every structural class is named there by definition.
    fn body(html: &str) -> &str {
        html.split_once("</style>").map(|(_, b)| b).unwrap_or(html)
    }

    /// Everything inside a `<…>`, i.e. tag names and attributes. This is the only
    /// context where a stray quote in document text could open an attribute, so
    /// it is the context an injection assertion has to inspect.
    fn tag_text(html: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in html.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                _ if depth > 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// Text content of the emitted table, tags stripped — for asserting on what a
    /// reader would see rather than on markup shape.
    fn visible_text(html: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        // The `<style>` block is markup, not content.
        let body = body(html);
        for c in body.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// Every `<td>` of the row whose gutter header is `row`, as raw markup.
    fn row_cells(html: &str, row: u32) -> Vec<String> {
        let needle = format!(">{row}</th>");
        let Some(start) = html.find(&needle) else {
            return Vec::new();
        };
        let rest = &html[start..];
        let end = rest.find("</tr>").unwrap_or(rest.len());
        rest[..end]
            .split("<td")
            .skip(1)
            .map(|s| format!("<td{}", s.split_once("</td>").map(|(a, _)| a).unwrap_or(s)))
            .collect()
    }

    /// Counts opening tags named exactly `tag`. The terminator check matters:
    /// a bare `"<th"` substring also matches every `<thead>`, which silently
    /// inflates the open count by one per table.
    fn open_count(html: &str, tag: &str) -> usize {
        html.match_indices(&format!("<{tag}"))
            .filter(|(i, m)| {
                matches!(
                    html[i + m.len()..].chars().next(),
                    Some('>') | Some(' ') | Some('/')
                )
            })
            .count()
    }

    fn balanced(html: &str) {
        for tag in ["div", "table", "thead", "tbody", "tr", "td", "th", "span"] {
            let open = open_count(html, tag);
            let close = html.matches(&format!("</{tag}>")).count();
            assert_eq!(open, close, "unbalanced <{tag}> in: {html}");
        }
    }

    // ── sheet discovery ─────────────────────────────────────────────────────

    #[test]
    fn sheet_order_comes_from_the_workbook_not_from_part_names() {
        // The regression this whole module exists for: the part names are
        // deliberately misleading — the *second* sheet is stored as `sheet1.xml`,
        // so a renderer that reads `xl/worksheets/sheet1.xml` shows the wrong
        // sheet for section 0 and cannot reach section 1 at all.
        let f = Fixture::new(
            "order",
            &[
                (
                    "xl/workbook.xml",
                    workbook("", &[("Q1 café", "rId7", ""), ("Widgets", "rId3", "")]),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId7", "worksheet", "worksheets/sheet4.xml"),
                        ("rId3", "worksheet", "worksheets/sheet1.xml"),
                    ]),
                ),
                (
                    "xl/worksheets/sheet4.xml",
                    worksheet(
                        "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>first-sheet</t></is></c></row></sheetData>",
                    ),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet(
                        "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>second-sheet</t></is></c></row></sheetData>",
                    ),
                ),
                ("xl/styles.xml", styles_xml("<cellXfs count=\"1\"><xf/></cellXfs>")),
            ],
        );

        let first = f.render(None);
        assert_eq!(first.sections, ["Q1 café", "Widgets"]);
        assert_eq!(first.sections.len(), 2);
        assert_eq!(first.section, 0);
        assert!(
            visible_text(&first.html).contains("first-sheet"),
            "section 0 must be the workbook's first sheet: {}",
            first.html
        );

        let second = f.render(Some(1));
        assert_eq!(second.section, 1);
        assert_eq!(second.sections, ["Q1 café", "Widgets"]);
        let text = visible_text(&second.html);
        assert!(text.contains("second-sheet"), "{}", second.html);
        assert!(!text.contains("first-sheet"));

        // An out-of-range section clamps instead of failing.
        assert_eq!(f.render(Some(99)).section, 1);
    }

    #[test]
    fn hidden_sheets_are_flagged_and_never_the_default() {
        let f = Fixture::new(
            "hidden-sheet",
            &[
                (
                    "xl/workbook.xml",
                    workbook(
                        "",
                        &[("Scratch", "rId1", "hidden"), ("Widgets", "rId2", "")],
                    ),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId1", "worksheet", "worksheets/sheet1.xml"),
                        ("rId2", "worksheet", "worksheets/sheet2.xml"),
                    ]),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>scratch</t></is></c></row></sheetData>"),
                ),
                (
                    "xl/worksheets/sheet2.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>widgets</t></is></c></row></sheetData>"),
                ),
                ("xl/styles.xml", styles_xml("<cellXfs count=\"1\"><xf/></cellXfs>")),
            ],
        );
        let doc = f.render(None);
        // Both sheets stay in the list so section indices match the workbook.
        assert_eq!(doc.sections, ["Scratch", "Widgets"]);
        assert_eq!(doc.section, 1, "a hidden sheet must not be the default");
        assert!(visible_text(&doc.html).contains("widgets"));
        assert!(
            doc.notes.iter().any(|n| n.contains("Scratch")),
            "the hidden sheet must be flagged: {:?}",
            doc.notes
        );
        // It is still reachable when asked for by index.
        assert!(visible_text(&f.render(Some(0)).html).contains("scratch"));
    }

    #[test]
    fn very_hidden_sheets_count_as_hidden() {
        let f = Fixture::new(
            "very-hidden",
            &[
                (
                    "xl/workbook.xml",
                    workbook("", &[("Gone", "rId1", "veryHidden"), ("Q1", "rId2", "")]),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId1", "worksheet", "worksheets/a.xml"),
                        ("rId2", "worksheet", "worksheets/b.xml"),
                    ]),
                ),
                ("xl/worksheets/a.xml", worksheet("<sheetData/>")),
                (
                    "xl/worksheets/b.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Q1</t></is></c></row></sheetData>"),
                ),
            ],
        );
        let doc = f.render(None);
        assert_eq!(doc.section, 1);
        assert!(doc.notes.iter().any(|n| n.contains("Gone")));
    }

    #[test]
    fn a_sheet_whose_relationship_is_missing_degrades_to_a_note() {
        let f = Fixture::new(
            "no-rel",
            &[
                ("xl/workbook.xml", workbook("", &[("Orphan", "rId9", "")])),
                ("xl/_rels/workbook.xml.rels", rels(&[])),
            ],
        );
        let doc = f.render(None);
        assert_eq!(doc.sections, ["Orphan"]);
        assert!(doc.truncated);
        assert!(
            doc.notes.iter().any(|n| n.contains("could not be shown")),
            "{:?}",
            doc.notes
        );
        balanced(&doc.html);
    }

    #[test]
    fn a_workbook_without_a_workbook_part_is_an_error() {
        let f = Fixture::new(
            "no-workbook",
            &[("xl/worksheets/sheet1.xml", worksheet("<sheetData/>"))],
        );
        let err = super::render(f.path(), None, &[]).expect_err("must fail");
        assert!(err.contains("workbook"), "{err}");

        // Present but empty is also an error: there is no sheet to show.
        let f = Fixture::new("no-sheets", &[("xl/workbook.xml", workbook("", &[]))]);
        let err = super::render(f.path(), None, &[]).expect_err("must fail");
        assert!(err.contains("no sheets"), "{err}");

        // And a non-package path fails cleanly rather than panicking.
        assert!(super::render("/nonexistent/café.xlsx", None, &[]).is_err());
    }

    #[test]
    fn the_workbook_part_is_found_through_the_package_relationship() {
        // Nothing lives at the conventional path: the officeDocument relationship
        // is the only way in.
        let f = Fixture::new(
            "odd-layout",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "/book/main.xml")]),
                ),
                (
                    "book/main.xml",
                    workbook("", &[("Sheet1", "rId1", "")]),
                ),
                (
                    "book/_rels/main.xml.rels",
                    rels(&[("rId1", "worksheet", "grid.xml")]),
                ),
                (
                    "book/grid.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Widget</t></is></c></row></sheetData>"),
                ),
            ],
        );
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains("Widget"), "{}", doc.html);
    }

    // ── values ──────────────────────────────────────────────────────────────

    #[test]
    fn a_date_formatted_number_renders_as_a_date_not_a_serial() {
        // The headline fix: 45678 is a date serial, and only the format code says
        // so. Rendering the raw number is the bug this replaces.
        let f = single(
            "date",
            "<sheetData><row r=\"1\">\
               <c r=\"A1\" s=\"1\"><v>45678</v></c>\
               <c r=\"B1\" s=\"2\"><v>45678</v></c>\
               <c r=\"C1\" s=\"0\"><v>45678</v></c>\
             </row></sheetData>",
            &[(
                "xl/styles.xml",
                styles_xml(
                    "<numFmts count=\"1\"><numFmt numFmtId=\"164\" formatCode=\"yyyy-mm-dd\"/></numFmts>\
                     <cellXfs count=\"3\"><xf numFmtId=\"0\"/><xf numFmtId=\"164\" applyNumberFormat=\"1\"/>\
                     <xf numFmtId=\"14\" applyNumberFormat=\"1\"/></cellXfs>",
                ),
            )],
        );
        let doc = f.render(None);
        let text = visible_text(&doc.html);
        // Custom code (id >= 164) and built-in 14 both resolve.
        assert!(text.contains("2025-01-21"), "custom numFmt: {text}");
        assert!(text.contains("1/21/2025"), "builtin numFmt 14: {text}");
        // The General-formatted cell is the only place the serial may appear.
        assert_eq!(text.matches("45678").count(), 1, "{text}");
    }

    #[test]
    fn date1904_shifts_the_epoch() {
        let body = "<sheetData><row r=\"1\"><c r=\"A1\" s=\"1\"><v>44216</v></c></row></sheetData>";
        let styles = styles_xml(
            "<numFmts count=\"1\"><numFmt numFmtId=\"164\" formatCode=\"yyyy-mm-dd\"/></numFmts>\
             <cellXfs count=\"2\"><xf/><xf numFmtId=\"164\" applyNumberFormat=\"1\"/></cellXfs>",
        );
        let parts = |pr: &str| {
            vec![
                ("xl/workbook.xml", workbook(pr, &[("Sheet1", "rId1", "")])),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[("rId1", "worksheet", "worksheets/sheet1.xml")]),
                ),
                ("xl/worksheets/sheet1.xml", worksheet(body)),
                ("xl/styles.xml", styles.clone()),
            ]
        };
        let a = Fixture::new("d1900", &parts(""));
        let b = Fixture::new("d1904", &parts("<workbookPr date1904=\"1\"/>"));
        // The same serial is 1462 days apart between the two date systems:
        // 44216 is 2021-01-20 under the 1900 epoch and 2025-01-21 under 1904.
        assert!(visible_text(&a.render(None).html).contains("2021-01-20"));
        assert!(visible_text(&b.render(None).html).contains("2025-01-21"));
    }

    #[test]
    fn every_cell_type_renders() {
        let f = single(
            "types",
            "<sheetData><row r=\"1\">\
               <c r=\"A1\" t=\"s\"><v>0</v></c>\
               <c r=\"B1\" t=\"s\"><v>1</v></c>\
               <c r=\"C1\" t=\"inlineStr\"><is><t>naïve</t></is></c>\
               <c r=\"D1\" t=\"b\"><v>1</v></c>\
               <c r=\"E1\" t=\"b\"><v>0</v></c>\
               <c r=\"F1\" t=\"e\"><v>#DIV/0!</v></c>\
               <c r=\"G1\" t=\"str\"><f>A1</f><v>formula-result</v></c>\
               <c r=\"H1\"><v>1234.5</v></c>\
               <c r=\"I1\"/>\
               <c r=\"J1\" t=\"s\"><v>77</v></c>\
             </row></sheetData>",
            &[(
                "xl/sharedStrings.xml",
                // The second entry is split across rich-text runs: reading only the
                // first <t> would silently drop "get", and the phonetic block must
                // not be appended.
                sst_xml(&[
                    "<t>café</t>",
                    "<r><rPr><b/></rPr><t>Wid</t></r><r><t>get</t></r>",
                    "<t>漢字</t><rPh sb=\"0\" eb=\"2\"><t>かんじ</t></rPh>",
                ]),
            )],
        );
        let doc = f.render(None);
        let text = visible_text(&doc.html);
        assert!(text.contains("café"), "{text}");
        assert!(text.contains("Widget"), "rich-text runs must join: {text}");
        assert!(text.contains("naïve"), "{text}");
        assert!(text.contains("TRUE") && text.contains("FALSE"), "{text}");
        assert!(text.contains("#DIV/0!"), "{text}");
        assert!(text.contains("formula-result"), "{text}");
        assert!(text.contains("1234.5"), "{text}");
        // An out-of-range shared-string index degrades to a note, not a panic.
        assert!(
            doc.notes.iter().any(|n| n == "Some cell text missing"),
            "{:?}",
            doc.notes
        );
        balanced(&doc.html);

        // Booleans and errors are centred, numbers right-aligned, text neither.
        let cells = row_cells(&doc.html, 1);
        assert_eq!(cells.len(), 10);
        assert!(cells[0].contains("café") && !cells[0].contains("xl-num"));
        assert!(cells[3].contains("xl-bool"), "{}", cells[3]);
        assert!(cells[7].contains("xl-num"), "{}", cells[7]);
    }

    #[test]
    fn phonetic_readings_are_not_duplicated_into_the_cell() {
        let f = single(
            "phonetic",
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c></row></sheetData>",
            &[(
                "xl/sharedStrings.xml",
                sst_xml(&["<t>Widget</t><rPh sb=\"0\" eb=\"2\"><t>ruby</t></rPh>"]),
            )],
        );
        let text = visible_text(&f.render(None).html);
        assert!(text.contains("Widget"));
        assert!(!text.contains("ruby"), "{text}");
    }

    // ── layout ──────────────────────────────────────────────────────────────

    #[test]
    fn a_merge_spans_the_anchor_and_omits_the_covered_cells() {
        let f = single(
            "merge",
            "<mergeCells count=\"1\"><mergeCell ref=\"A1:C2\"/></mergeCells>\
             <sheetData>\
               <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>anchor</t></is></c>\
                 <c r=\"B1\" t=\"inlineStr\"><is><t>covered-b1</t></is></c>\
                 <c r=\"D1\" t=\"inlineStr\"><is><t>after</t></is></c></row>\
               <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>covered-a2</t></is></c>\
                 <c r=\"D2\" t=\"inlineStr\"><is><t>tail</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        let r1 = row_cells(&doc.html, 1);
        // A1 spans 2 rows x 3 columns; B1 and C1 are not emitted at all, so the row
        // holds the anchor plus D1.
        assert_eq!(r1.len(), 2, "{r1:?}");
        assert!(r1[0].contains("rowspan=\"2\""), "{}", r1[0]);
        assert!(r1[0].contains("colspan=\"3\""), "{}", r1[0]);
        assert!(r1[0].contains("anchor"));
        assert!(r1[1].contains("after"));
        let text = visible_text(&doc.html);
        assert!(!text.contains("covered-b1"), "{text}");
        assert!(!text.contains("covered-a2"), "{text}");
        // Row 2 keeps only the cell past the merge.
        let r2 = row_cells(&doc.html, 2);
        assert_eq!(r2.len(), 1, "{r2:?}");
        assert!(r2[0].contains("tail"));
        balanced(&doc.html);
    }

    #[test]
    fn a_merge_running_past_the_grid_is_clipped_not_dropped() {
        // A whole-column merge: the range extends far past the emitted bounds, so
        // materializing its covered cells is not an option and the span has to be
        // clamped.
        let f = single(
            "merge-huge",
            "<mergeCells count=\"1\"><mergeCell ref=\"A1:B1048576\"/></mergeCells>\
             <sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>tall</t></is></c>\
             <c r=\"B1\" t=\"inlineStr\"><is><t>hidden-by-merge</t></is></c></row>\
             <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>under</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        let r1 = row_cells(&doc.html, 1);
        assert_eq!(r1.len(), 1);
        assert!(r1[0].contains("rowspan=\"2\""), "{}", r1[0]);
        assert!(r1[0].contains("colspan=\"2\""), "{}", r1[0]);
        let text = visible_text(&doc.html);
        assert!(!text.contains("hidden-by-merge"));
        assert!(!text.contains("under"), "row 2 is inside the merge: {text}");
        balanced(&doc.html);
    }

    #[test]
    fn hidden_rows_and_columns_are_left_out_of_the_grid() {
        let f = single(
            "hidden-tracks",
            "<cols><col min=\"2\" max=\"2\" width=\"12\" hidden=\"1\"/>\
             <col min=\"3\" max=\"3\" width=\"30\" customWidth=\"1\"/></cols>\
             <sheetData>\
               <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>a1</t></is></c>\
                 <c r=\"B1\" t=\"inlineStr\"><is><t>b1-hidden-col</t></is></c>\
                 <c r=\"C1\" t=\"inlineStr\"><is><t>c1</t></is></c></row>\
               <row r=\"2\" hidden=\"1\"><c r=\"A2\" t=\"inlineStr\"><is><t>a2-hidden-row</t></is></c></row>\
               <row r=\"3\" ht=\"40\" customHeight=\"1\"><c r=\"A3\" t=\"inlineStr\"><is><t>a3</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        let text = visible_text(&doc.html);
        assert!(text.contains("a1") && text.contains("c1"));
        assert!(!text.contains("b1-hidden-col"), "{text}");
        assert!(!text.contains("a2-hidden-row"), "{text}");
        assert!(text.contains("a3"));
        // The gutter keeps the real row numbers, so the skip is visible (1, 3) —
        // which is exactly what Excel shows.
        assert!(doc.html.contains(">1</th>"));
        assert!(!doc.html.contains(">2</th>"));
        assert!(doc.html.contains(">3</th>"));
        // Only two column headers survive, and the custom width became a rule.
        // Counted in the grid, not the stylesheet — which names `xl-ch` too.
        assert_eq!(body(&doc.html).matches("xl-ch").count(), 2, "{}", doc.html);
        assert!(doc.html.contains("width:215px"), "30 chars: {}", doc.html);
        assert!(doc.html.contains("height:53px"), "40pt: {}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn a_merge_anchored_in_a_hidden_track_keeps_the_row_aligned() {
        // The anchor of A1:C2 is A1, which is in a hidden column *and* a hidden row.
        // Both the span and its value have to move to the first visible cell of the
        // range, or the row emits fewer cells than it has headers and the whole grid
        // slides left under its column letters.
        let f = single(
            "merge-hidden-anchor",
            "<cols><col min=\"1\" max=\"1\" hidden=\"1\"/></cols>\
             <mergeCells count=\"1\"><mergeCell ref=\"A1:C2\"/></mergeCells>\
             <sheetData>\
               <row r=\"1\" hidden=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>merged</t></is></c></row>\
               <row r=\"2\"/>\
               <row r=\"3\"><c r=\"A3\" t=\"inlineStr\"><is><t>a3</t></is></c>\
                 <c r=\"B3\" t=\"inlineStr\"><is><t>b3</t></is></c>\
                 <c r=\"C3\" t=\"inlineStr\"><is><t>c3</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        // Row 1 is hidden, so the span lands on row 2 at column B (A is hidden):
        // two visible columns of the three, one visible row of the two.
        let r2 = row_cells(&doc.html, 2);
        assert_eq!(r2.len(), 1, "{:?}", r2);
        assert!(r2[0].contains("colspan=\"2\""), "{}", r2[0]);
        assert!(!r2[0].contains("rowspan"), "one visible row: {}", r2[0]);
        // The value followed the span rather than vanishing with its stored cell.
        assert!(r2[0].contains("merged"), "{}", r2[0]);
        // And the unmerged row below still has both of its visible cells, so the
        // body lines up with the two column headers.
        assert_eq!(row_cells(&doc.html, 3).len(), 2, "{}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn a_merge_with_no_visible_track_is_dropped() {
        let f = single(
            "merge-all-hidden",
            "<mergeCells count=\"1\"><mergeCell ref=\"A1:B1\"/></mergeCells>\
             <sheetData>\
               <row r=\"1\" hidden=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>gone</t></is></c></row>\
               <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>a2</t></is></c>\
                 <c r=\"B2\" t=\"inlineStr\"><is><t>b2</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        // Nothing of the merge survives, and row 2 is untouched by it.
        assert!(!visible_text(&doc.html).contains("gone"));
        assert_eq!(row_cells(&doc.html, 2).len(), 2, "{}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn frozen_panes_become_sticky_and_only_then_separate_borders() {
        let plain = single(
            "no-freeze",
            "<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c></row></sheetData>",
            &[],
        )
        .render(None);
        // The base stylesheet always carries the sticky rules; what must be absent
        // is the class that switches the table into that mode.
        assert!(!body(&plain.html).contains("xl-frozen"), "{}", plain.html);

        let f = single(
            "freeze",
            "<sheetViews><sheetView><pane xSplit=\"1\" ySplit=\"1\" topLeftCell=\"B2\" state=\"frozen\"/></sheetView></sheetViews>\
             <sheetData>\
               <row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"><v>2</v></c></row>\
               <row r=\"2\"><c r=\"A2\"><v>3</v></c><c r=\"B2\"><v>4</v></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        // Sticky only works under border-collapse:separate, so the mode is opt-in.
        assert!(body(&doc.html).contains("xl-frozen"), "{}", doc.html);
        assert!(body(&doc.html).contains("fzc0"), "{}", doc.html);
        assert!(body(&doc.html).contains("fzr1"), "{}", doc.html);
        // The offset accounts for the row-number gutter.
        assert!(doc.html.contains(".fzc0{left:46px;}"), "{}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn the_gutter_carries_row_numbers_and_column_letters() {
        let f = single(
            "gutter",
            "<sheetData><row r=\"1\"><c r=\"AA1\"><v>1</v></c></row></sheetData>",
            &[],
        );
        let doc = f.render(None);
        assert!(doc.html.contains(">A</th>"));
        assert!(doc.html.contains(">Z</th>"));
        assert!(doc.html.contains(">AA</th>"));
        assert!(doc.html.contains(">1</th>"));
        // Chrome uses theme tokens; the sheet surface stays paper white.
        assert!(doc.html.contains("var(--bg-card"), "{}", doc.html);
        assert!(doc.html.contains(".xl-sheet{border-collapse:collapse;table-layout:fixed;background:#fff;"));
    }

    #[test]
    fn only_cells_with_text_are_marked_selectable() {
        // A1 holds text; B1 falls inside the used range (B2 has a value) but is
        // itself empty. The frame decides select-vs-pan from `xl-t`, so a cell with
        // nothing in it must not claim to hold text.
        let f = single(
            "sel",
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>café</t></is></c></row>\
             <row r=\"2\"><c r=\"B2\"><v>12</v></c></row></sheetData>",
            &[],
        );
        let doc = f.render(None);
        let cells = row_cells(&doc.html, 1);
        assert_eq!(cells.len(), 2, "{}", doc.html);
        assert!(cells[0].contains("xl-t"), "{}", cells[0]);
        assert!(!cells[1].contains("xl-t"), "{}", cells[1]);
        assert!(doc.html.contains("td.xl-t{cursor:text;}"));
    }

    // ── styles ──────────────────────────────────────────────────────────────

    #[test]
    fn cell_formats_become_classes_not_inline_styles() {
        let f = single(
            "classes",
            "<sheetData><row r=\"1\">\
               <c r=\"A1\" s=\"1\" t=\"inlineStr\"><is><t>bold</t></is></c>\
               <c r=\"B1\" s=\"1\" t=\"inlineStr\"><is><t>also bold</t></is></c>\
             </row></sheetData>",
            &[(
                "xl/styles.xml",
                styles_xml(
                    "<fonts count=\"1\"><font><b/><color rgb=\"FF123456\"/></font></fonts>\
                     <cellXfs count=\"2\"><xf/><xf fontId=\"0\" applyFont=\"1\"/></cellXfs>",
                ),
            )],
        );
        let doc = f.render(None);
        let cells = row_cells(&doc.html, 1);
        // Both cells share one xf, so both reference one class and neither carries
        // a `style` attribute.
        for c in &cells {
            assert!(c.contains("class=\"xf1"), "{c}");
            assert!(!c.contains("style="), "cells must not carry inline styles: {c}");
        }
        // ...and the rule really is in the emitted stylesheet.
        assert!(doc.html.contains("td.xf1{"), "{}", doc.html);
        assert!(doc.html.contains("font-weight:700;"), "{}", doc.html);
        assert!(doc.html.contains("color:#123456;"), "{}", doc.html);
        // Unused styles are not emitted.
        assert!(!doc.html.contains("td.xf0{"));
    }

    #[test]
    fn a_workbook_without_styles_still_renders_with_a_note() {
        let f = Fixture::new(
            "no-styles",
            &[
                ("xl/workbook.xml", workbook("", &[("Sheet1", "rId1", "")])),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[("rId1", "worksheet", "worksheets/sheet1.xml")]),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" s=\"4\"><v>2.5</v></c></row></sheetData>"),
                ),
            ],
        );
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains("2.5"));
        assert!(
            doc.notes.iter().any(|n| n == "Cell styles unreadable"),
            "{:?}",
            doc.notes
        );
        balanced(&doc.html);

        // A corrupt styles part is a note too, not a failed preview.
        let f = single(
            "bad-styles",
            "<sheetData><row r=\"1\"><c r=\"A1\"><v>7</v></c></row></sheetData>",
            &[("xl/styles.xml", "<styleSheet><cellXfs>".to_string())],
        );
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains('7'));
        assert!(doc.notes.iter().any(|n| n.contains("unreadable")), "{:?}", doc.notes);
    }

    #[test]
    fn a_number_format_colour_reaches_the_cell_as_a_class() {
        let f = single(
            "numcolor",
            "<sheetData><row r=\"1\"><c r=\"A1\" s=\"1\"><v>-5</v></c>\
             <c r=\"B1\" s=\"1\"><v>5</v></c></row></sheetData>",
            &[(
                "xl/styles.xml",
                styles_xml(
                    "<numFmts count=\"1\"><numFmt numFmtId=\"164\" formatCode=\"0;[Red]-0\"/></numFmts>\
                     <cellXfs count=\"2\"><xf/><xf numFmtId=\"164\" applyNumberFormat=\"1\"/></cellXfs>",
                ),
            )],
        );
        let doc = f.render(None);
        let cells = row_cells(&doc.html, 1);
        assert!(cells[0].contains("xnc0"), "negative gets the colour: {}", cells[0]);
        assert!(!cells[1].contains("xnc"), "positive does not: {}", cells[1]);
        assert!(doc.html.contains("td.xnc0{color:#ff0000;}"), "{}", doc.html);
    }

    // ── bounds and robustness ───────────────────────────────────────────────

    #[test]
    fn trailing_styled_but_blank_rows_are_not_part_of_the_extent() {
        // What Excel actually writes: the used range padded out with cells that
        // carry a style and nothing else. Here xf 1 is a font-only style (paints
        // nothing on an empty cell) and xf 2 has a border (paints, so it counts).
        let mut body = String::from(
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>café</t></is></c></row>\
             <row r=\"2\"><c r=\"A2\" s=\"2\"/></row>",
        );
        for r in 3..=900 {
            body.push_str(&format!("<row r=\"{r}\"><c r=\"A{r}\" s=\"1\"/></row>"));
        }
        body.push_str("</sheetData>");
        let f = single(
            "blank-tail",
            &body,
            &[(
                "xl/styles.xml",
                styles_xml(
                    "<borders count=\"2\"><border/>\
                     <border><left style=\"thin\"/></border></borders>\
                     <cellXfs count=\"3\"><xf/>\
                     <xf fontId=\"0\" applyFont=\"1\"/>\
                     <xf borderId=\"1\" applyBorder=\"1\"/></cellXfs>",
                ),
            )],
        );
        let doc = f.render(None);
        // Row 2 paints a border, so it is in. Rows 3+ show nothing and are not.
        assert!(doc.html.contains(">2</th>"), "{}", doc.html);
        assert!(!doc.html.contains(">3</th>"), "{}", doc.html);
        // 898 rows of nothing is not "clipped" - there was nothing there to clip.
        assert!(!doc.truncated);
        assert!(
            !doc.notes.iter().any(|n| n == "First 200 rows only"),
            "{:?}",
            doc.notes
        );
        balanced(&doc.html);
    }

    #[test]
    fn clipping_past_the_row_cap_sets_truncated_and_notes_it() {
        let mut body = String::from("<sheetData>");
        for r in 1..=(MAX_ROWS + 5) {
            body.push_str(&format!("<row r=\"{r}\"><c r=\"A{r}\"><v>{r}</v></c></row>"));
        }
        body.push_str("</sheetData>");
        let f = single("row-cap", &body, &[]);
        let doc = f.render(None);
        assert!(doc.truncated);
        assert!(
            doc.notes.iter().any(|n| n == "First 200 rows only"),
            "{:?}",
            doc.notes
        );
        // The last emitted row is the cap, and nothing past it appears.
        assert!(doc.html.contains(&format!(">{MAX_ROWS}</th>")));
        assert!(!doc.html.contains(&format!(">{}</th>", MAX_ROWS + 1)));
        balanced(&doc.html);
    }

    #[test]
    fn clipping_past_the_column_cap_sets_truncated_and_notes_it() {
        let over = col_letter(MAX_COLS); // one past the cap
        let f = single(
            "col-cap",
            &format!(
                "<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c>\
                 <c r=\"{over}1\" t=\"inlineStr\"><is><t>past-the-cap</t></is></c></row></sheetData>"
            ),
            &[],
        );
        let doc = f.render(None);
        assert!(doc.truncated);
        assert!(
            doc.notes.iter().any(|n| n == "First 200 columns only"),
            "{:?}",
            doc.notes
        );
        assert!(!visible_text(&doc.html).contains("past-the-cap"));
    }

    #[test]
    fn output_stays_balanced_when_the_writer_cap_trips_mid_sheet() {
        let mut body = String::from("<sheetData>");
        for r in 1..=60 {
            body.push_str(&format!("<row r=\"{r}\">"));
            for c in 0..20 {
                body.push_str(&format!(
                    "<c r=\"{}{r}\" t=\"inlineStr\"><is><t>café {r}-{c}</t></is></c>",
                    col_letter(c)
                ));
            }
            body.push_str("</row>");
        }
        body.push_str("</sheetData>");
        let f = single("cap", &body, &[]);
        // A cap small enough to trip in the middle of the grid.
        let doc = render_capped(f.path(), None, &[], 900).expect("render");
        assert!(doc.truncated);
        balanced(&doc.html);
        // The writer's own note survives, and the document ends closed.
        assert!(doc.html.contains("office-trunc"), "{}", doc.html);
        assert!(doc.html.trim_end().ends_with("</div>"), "{}", doc.html);
    }

    #[test]
    fn an_empty_sheet_renders_a_message_rather_than_an_empty_table() {
        let f = single("empty", "<sheetData/>", &[]);
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains("empty"), "{}", doc.html);
        assert!(!doc.truncated);
        balanced(&doc.html);
    }

    #[test]
    fn malformed_sheet_xml_and_absurd_references_never_panic() {
        // Unparseable sheet part: a note, and the other sheets stay listed.
        let f = single("bad-sheet", "", &[("xl/worksheets/sheet1.xml", "<worksheet".to_string())]);
        let doc = f.render(None);
        assert!(doc.notes.iter().any(|n| n.contains("could not be shown")), "{:?}", doc.notes);

        // References that are too long, non-numeric or out of range are skipped.
        let f = single(
            "bad-refs",
            "<sheetData>\
               <row r=\"1\"><c r=\"AAAAAAAAAAAAAAAAAAAA1\"><v>1</v></c>\
                 <c r=\"\"><v>2</v></c>\
                 <c r=\"12\"><v>3</v></c>\
                 <c r=\"B1\" t=\"inlineStr\"><is><t>ok</t></is></c></row>\
               <row r=\"0\"><c r=\"A0\"><v>9</v></c></row>\
               <row r=\"99999999\"><c r=\"A99999999\"><v>9</v></c></row>\
             </sheetData>",
            &[],
        );
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains("ok"), "{}", doc.html);
        balanced(&doc.html);

        // A merge reference that is not a range at all is ignored.
        let f = single(
            "bad-merge",
            "<mergeCells><mergeCell ref=\"nonsense\"/><mergeCell/><mergeCell ref=\"A1\"/></mergeCells>\
             <sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>fine</t></is></c></row></sheetData>",
            &[],
        );
        assert!(visible_text(&f.render(None).html).contains("fine"));
    }

    #[test]
    fn document_text_cannot_inject_markup() {
        let f = single(
            "escape",
            "<sheetData><row r=\"1\">\
               <c r=\"A1\" t=\"inlineStr\"><is><t>&lt;script&gt;alert(1)&lt;/script&gt;</t></is></c>\
               <c r=\"B1\" t=\"inlineStr\"><is><t>x&quot; onload=&quot;y</t></is></c>\
             </row></sheetData>",
            &[],
        );
        let doc = f.render(None);
        assert!(!doc.html.contains("<script"), "{}", doc.html);
        assert!(doc.html.contains("&lt;script&gt;"), "{}", doc.html);
        // A bare `"` in a text node is inert — it cannot close an attribute value
        // that was never opened, and over-escaping it here would be the wrong fix.
        // What must hold is that nothing document-derived reaches the inside of a
        // tag, so that is where the assertion looks.
        assert!(!tag_text(&doc.html).contains("onload"), "{}", doc.html);
        assert!(
            visible_text(&doc.html).contains("x\" onload=\"y"),
            "the text itself must still be shown: {}",
            doc.html
        );
    }

    // ── search terms ────────────────────────────────────────────────────────

    #[test]
    fn search_terms_are_marked_and_the_best_match_is_reported() {
        let f = single(
            "marks",
            "<sheetData>\
               <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>naïve numbers</t></is></c></row>\
               <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>Widget café</t></is></c></row>\
             </sheetData>",
            &[],
        );
        let doc = super::render(f.path(), None, &["café".to_string(), "widget".to_string()])
            .expect("render");
        assert_eq!(doc.html.matches("class=\"preview-hl\"").count(), 2, "{}", doc.html);
        // The scroll target is a stable per-mark id, not a hardcoded anchor.
        let id = doc.best_mark_id.as_deref().expect("a best mark");
        assert!(id.starts_with("pm-"), "{id}");
        assert!(doc.html.contains(&format!("id=\"{id}\"")), "{}", doc.html);
        assert!(!doc.html.contains("pmatch"));

        // No terms, no marks and no target.
        let plain = f.render(None);
        assert!(!plain.html.contains("preview-hl"));
        assert_eq!(plain.best_mark_id, None);
    }

    // ── drawings ────────────────────────────────────────────────────────────

    #[test]
    fn a_chart_reference_becomes_a_labelled_box_at_the_right_geometry() {
        let drawing = format!(
            "<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" \
               xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
               xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
               xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\">\
               <xdr:oneCellAnchor>\
                 <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>\
                 <xdr:ext cx=\"2857500\" cy=\"1905000\"/>\
                 <xdr:graphicFrame><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\">\
                   <c:chart r:id=\"rId1\"/></a:graphicData></a:graphic></xdr:graphicFrame>\
                 <xdr:clientData/>\
               </xdr:oneCellAnchor>\
             </xdr:wsDr>"
        );
        let f = single(
            "chart",
            "<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"><v>2</v></c></row>\
             <row r=\"2\"><c r=\"A2\"><v>3</v></c></row></sheetData>\
             <drawing r:id=\"rIdD\"/>",
            &[
                (
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    rels(&[("rIdD", "drawing", "../drawings/drawing1.xml")]),
                ),
                ("xl/drawings/drawing1.xml", drawing),
            ],
        );
        let doc = f.render(None);
        // 2857500 EMU = 300px, 1905000 = 200px; anchored at column B, row 2.
        assert!(doc.html.contains("class=\"xl-ph\""), "{}", doc.html);
        // Placeholder labels are lowercase in both renderers, matching the
        // "image unavailable" reasons they sit alongside.
        assert!(doc.html.contains(">chart</div>"), "{}", doc.html);
        assert!(doc.html.contains("width:300px"), "{}", doc.html);
        assert!(doc.html.contains("height:200px"), "{}", doc.html);
        assert!(doc.html.contains("left:64px"), "column B starts one default column in: {}", doc.html);
        assert!(doc.html.contains("top:20px"), "row 2 starts one default row down: {}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn an_embedded_picture_is_inlined_as_a_data_uri() {
        // Media can only reach the preview iframe as a data: URI — its opaque
        // origin cannot fetch anything from the parent document.
        let png = {
            use image::{ImageBuffer, Rgba};
            let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_fn(8, 8, |x, y| Rgba([(x * 30) as u8, (y * 30) as u8, 60, 255]));
            let mut out = Vec::new();
            img.write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Png,
            )
            .unwrap();
            out
        };
        let drawing = "<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" \
               xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
               xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
               <xdr:twoCellAnchor>\
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>\
                 <xdr:to><xdr:col>2</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>\
                 <xdr:pic><xdr:nvPicPr><xdr:cNvPr id=\"1\" name=\"Picture 1\" descr=\"a café\"/></xdr:nvPicPr>\
                   <xdr:blipFill><a:blip r:embed=\"rId1\"/></xdr:blipFill></xdr:pic>\
                 <xdr:clientData/>\
               </xdr:twoCellAnchor>\
             </xdr:wsDr>"
            .to_string();

        // The PNG is binary, so this fixture is assembled directly.
        let path = std::env::temp_dir()
            .join(format!("portunus-xlsx-pic-{}.xlsx", std::process::id()));
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            let text: Vec<(&str, String)> = vec![
                ("xl/workbook.xml", workbook("", &[("Sheet1", "rId1", "")])),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[("rId1", "worksheet", "worksheets/sheet1.xml")]),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet(
                        "<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"><v>2</v></c></row>\
                         <row r=\"2\"><c r=\"A2\"><v>3</v></c></row></sheetData><drawing r:id=\"rIdD\"/>",
                    ),
                ),
                (
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    rels(&[("rIdD", "drawing", "../drawings/drawing1.xml")]),
                ),
                ("xl/drawings/drawing1.xml", drawing),
                (
                    "xl/drawings/_rels/drawing1.xml.rels",
                    rels(&[("rId1", "image", "../media/image1.png")]),
                ),
            ];
            for (name, body) in &text {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.start_file("xl/media/image1.png", opts).unwrap();
            zip.write_all(&png).unwrap();
            zip.finish().unwrap();
        }
        let f = Fixture(path);
        let doc = f.render(None);
        assert!(doc.html.contains("src=\"data:image/"), "{}", doc.html);
        assert!(doc.html.contains("alt=\"a café\""), "{}", doc.html);
        assert!(doc.html.contains("class=\"xl-dw\""), "{}", doc.html);
        // Two default columns wide, two default rows tall.
        assert!(doc.html.contains("width:128px"), "{}", doc.html);
        assert!(doc.html.contains("height:40px"), "{}", doc.html);
        balanced(&doc.html);
    }

    #[test]
    fn an_unparseable_drawing_rels_part_says_the_images_were_lost() {
        // The sheet's `.rels` is where the drawing relationship lives, so a
        // malformed one takes every drawing on the sheet with it. Dropping them
        // silently is the failure mode this guards: the note has to say so.
        let f = single(
            "bad-rels",
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Widget</t></is></c></row></sheetData>\
             <drawing r:id=\"rIdD\"/>",
            &[(
                "xl/worksheets/_rels/sheet1.xml.rels",
                "<Relationships><Relationship".to_string(),
            )],
        );
        let doc = f.render(None);
        // The grid itself is unaffected.
        assert!(visible_text(&doc.html).contains("Widget"), "{}", doc.html);
        assert!(
            doc.notes
                .iter()
                .any(|n| n.contains("could not be read")),
            "{:?}",
            doc.notes
        );
        balanced(&doc.html);
    }

    #[test]
    fn a_missing_drawing_part_is_ignored_rather_than_fatal() {
        let f = single(
            "no-drawing",
            "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Widget</t></is></c></row></sheetData>\
             <drawing r:id=\"rIdMissing\"/>",
            &[],
        );
        let doc = f.render(None);
        assert!(visible_text(&doc.html).contains("Widget"));
        balanced(&doc.html);
    }

    // ── cell reference helpers ──────────────────────────────────────────────

    #[test]
    fn column_letters_round_trip_and_stay_bounded() {
        for (letters, idx) in [("A", 0usize), ("Z", 25), ("AA", 26), ("XFD", 16383)] {
            assert_eq!(col_letter_to_index(letters), Some(idx));
            assert_eq!(col_letter(idx), letters);
        }
        assert_eq!(col_letter_to_index(""), None);
        assert_eq!(col_letter_to_index("12"), None);
        // Past Excel's last column, and long enough to overflow an unbounded
        // accumulator — this must be a `None`, not a panic.
        assert_eq!(col_letter_to_index("XFE"), None);
        assert_eq!(col_letter_to_index(&"A".repeat(64)), None);
    }

    #[test]
    fn cell_ref_splits_letters_from_digits() {
        assert_eq!(split_cell_ref("AB12"), ("AB", "12"));
        assert_eq!(split_cell_ref("A1"), ("A", "1"));
        assert_eq!(split_cell_ref("Sheet"), ("Sheet", ""));
        assert_eq!(split_cell_ref(""), ("", ""));
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn indexed_sections_follow_the_workbook_order_and_carry_sheet_names() {
        // Same misleading part names as the render test above: the *second* sheet
        // is stored as `sheet1.xml`. An extractor that sorted part names would
        // hand the index two sections in the wrong order, and every preview
        // opened from a content hit would land on the other sheet.
        let f = Fixture::new(
            "index-order",
            &[
                (
                    "xl/workbook.xml",
                    workbook("", &[("Q1 café", "rId7", ""), ("Widgets", "rId3", "")]),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId7", "worksheet", "worksheets/sheet4.xml"),
                        ("rId3", "worksheet", "worksheets/sheet1.xml"),
                    ]),
                ),
                (
                    "xl/worksheets/sheet4.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>first-sheet</t></is></c></row></sheetData>"),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>second-sheet</t></is></c></row></sheetData>"),
                ),
                ("xl/styles.xml", styles_xml("<cellXfs count=\"1\"><xf/></cellXfs>")),
            ],
        );

        let sections = extract_sections(f.path()).expect("extract");
        // The index's section count and order must be the renderer's, or a jump
        // lands on the wrong sheet.
        assert_eq!(sections.len(), f.render(None).sections.len());
        assert_eq!(sections[0].name, "Q1 café");
        assert_eq!(sections[1].name, "Widgets");
        assert!(sections[0].text.contains("first-sheet"), "{:?}", sections[0].text);
        assert!(sections[1].text.contains("second-sheet"), "{:?}", sections[1].text);

        // The sheet name is searchable text, and the sections are joined by the
        // separator `content_index::doc_sections` splits on.
        let flat = super::super::extract_office_text(f.path()).expect("flat");
        let split: Vec<&str> = flat.split(super::super::SECTION_SEP).collect();
        assert_eq!(split.len(), 2);
        assert!(split[0].starts_with("Q1 café\n"), "{:?}", split[0]);
        assert!(split[1].starts_with("Widgets\n"), "{:?}", split[1]);
    }

    #[test]
    fn indexed_cells_are_more_than_the_shared_string_pool() {
        // The bug this replaces: extraction read only `sharedStrings.xml`, so a
        // number, a date, an inline string and a cached formula result were all
        // unsearchable — and a date was not even present as its serial.
        let styles = styles_xml(
            "<numFmts count=\"1\"><numFmt numFmtId=\"164\" formatCode=\"yyyy-mm-dd\"/></numFmts>\
             <cellXfs count=\"2\"><xf/><xf numFmtId=\"164\" applyNumberFormat=\"1\"/></cellXfs>",
        );
        let f = Fixture::new(
            "index-cells",
            &[
                ("xl/workbook.xml", workbook("", &[("Sheet1", "rId1", "")])),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId1", "worksheet", "worksheets/sheet1.xml"),
                        ("rId2", "sharedStrings", "sharedStrings.xml"),
                    ]),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet(
                        "<sheetData>\
                           <row r=\"1\">\
                             <c r=\"A1\" t=\"s\"><v>0</v></c>\
                             <c r=\"B1\" t=\"inlineStr\"><is><t>naïve</t></is></c>\
                           </row>\
                           <row r=\"2\">\
                             <c r=\"A2\" s=\"1\"><v>44216</v></c>\
                             <c r=\"B2\"><v>1234.5</v></c>\
                             <c r=\"C2\" t=\"str\"><f>A1</f><v>formula-result</v></c>\
                           </row>\
                         </sheetData>",
                    ),
                ),
                ("xl/sharedStrings.xml", sst_xml(&["<t>café</t>"])),
                ("xl/styles.xml", styles),
            ],
        );

        let sections = extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), 1);
        let text = &sections[0].text;
        for want in ["café", "naïve", "1234.5", "formula-result"] {
            assert!(text.contains(want), "{want:?} missing from {text:?}");
        }
        // A date indexes as the date it displays, not as its serial: searching
        // "2021" has to find the cell the reader sees as 2021-01-20.
        assert!(text.contains("2021-01-20"), "{text:?}");
        assert!(!text.contains("44216"), "{text:?}");
        // Cells are tab-separated and rows newline-separated, so neighbouring
        // values never fuse into one token.
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().contains('\t'));
    }

    #[test]
    fn indexed_rows_stop_where_the_renderer_clips() {
        // A hit past the emitted window could never be highlighted, so the index
        // does not claim it.
        let mut rows = String::new();
        for r in 1..=MAX_ROWS + 5 {
            rows.push_str(&format!(
                "<row r=\"{r}\"><c r=\"A{r}\" t=\"inlineStr\"><is><t>row{r}</t></is></c></row>"
            ));
        }
        let f = single("index-clip", &format!("<sheetData>{rows}</sheetData>"), &[]);
        let text = &extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains(&format!("row{MAX_ROWS}")));
        assert!(!text.contains(&format!("row{}", MAX_ROWS + 1)), "{text:?}");
    }

    #[test]
    fn an_unreadable_sheet_keeps_its_section_slot() {
        // `rId2` resolves to a part the package does not contain. The sheet still
        // occupies section 1, because dropping it would make section 2 of the
        // index mean section 1 of the renderer.
        let f = Fixture::new(
            "index-gap",
            &[
                (
                    "xl/workbook.xml",
                    workbook("", &[("A", "rId1", ""), ("Gone", "rId2", ""), ("C", "rId3", "")]),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    rels(&[
                        ("rId1", "worksheet", "worksheets/sheet1.xml"),
                        ("rId2", "worksheet", "worksheets/missing.xml"),
                        ("rId3", "worksheet", "worksheets/sheet3.xml"),
                    ]),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>alpha</t></is></c></row></sheetData>"),
                ),
                (
                    "xl/worksheets/sheet3.xml",
                    worksheet("<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>gamma</t></is></c></row></sheetData>"),
                ),
                ("xl/styles.xml", styles_xml("<cellXfs count=\"1\"><xf/></cellXfs>")),
            ],
        );
        let sections = extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), 3);
        assert!(sections[0].text.contains("alpha"));
        assert_eq!(sections[1].text, "");
        assert!(sections[2].text.contains("gamma"));
    }
}
