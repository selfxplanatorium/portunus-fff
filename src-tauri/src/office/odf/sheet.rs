//! `office:spreadsheet` → a styled grid, one table per call.
//!
//! Drives `office::sheet`, the same emitter the xlsx path uses, so the two dialects
//! produce one kind of grid: the same gutters, the same frozen-pane mechanism, the
//! same `xl-t` hook the frame's selection engine reads a selection out of. What this
//! file owns is SpreadsheetML's opposite number — reading ODF's structure into
//! `sheetmodel`'s vocabulary.
//!
//! Three things differ from xlsx and each one shapes the code:
//!
//! - **Repetition is how ODS spells its used range.** `table:number-columns-repeated`
//!   reaches 16368 in a real file, and `-rows-repeated` does the same vertically. So
//!   nothing is materialized per cell: rows are expanded into an index of *nodes*
//!   under a cap, and a row's cells are mapped to columns only when the emitter pulls
//!   that row (see [`Rows`]).
//! - **The display text is stored, not computed.** A cell carries both
//!   `office:value` and the `text:p` its producer formatted, so the grid shows what
//!   the author saw without this renderer evaluating a number format at all. A number
//!   style is only reached for a cell that has a value and no text — which is what a
//!   producer writes when it never displayed the cell.
//! - **Styles are named, not numbered.** `sheetmodel::Cell::style` is an ordinal into
//!   a rule table, so [`SheetStyles`] interns `table:style-name` into one. Only
//!   equality matters to the emitter, so interning is lossless.
//!
//! Two hard limits are inherited deliberately: the emitter's own `MAX_ROWS`/`MAX_COLS`
//! window, and `emit_rows` pulling cells lazily through [`CellSource`] — a hidden
//! column, a merge-covered slot and everything past the byte cap are never resolved,
//! so resolving them eagerly would spend the budget on cells nobody sees.

use std::collections::{BTreeSet, HashMap};

use super::super::cellstyle::{align_css, AlignSpec};
use super::super::emit::{self, Notes};
use super::super::highlight::{Marker as Highlight, Terms};
use super::super::html::{fmt_px, pt_to_px, Style};
use super::super::model::Align;
use super::super::sheet::{self, Frozen, StyleTable};
use super::super::sheetmodel::{resolve_anchors, Cell, CellSource, Merge, Track};
use super::super::xml::{self, attr_local, attr_u32, elems};
use super::super::{OfficeDoc, Shape};
use super::numstyle::Numbers;
use super::pkg::Package;
use super::style::{CellProps, Edge, Family, Resolved, Sides, Styles, TextProps};
use roxmltree::Node;

/// Byte cap for the emitted grid. A sheet is one table of bounded extent, so it
/// matches the xlsx path's rather than a page column's.
pub const HTML_CAP: usize = 4 * 1024 * 1024;

/// Tables per document, and the emitted window into one of them. The window is the
/// xlsx renderer's, deliberately: a preview shows the top-left corner of a sheet,
/// and two dialects showing different amounts of it would be a surprise.
const MAX_SHEETS: usize = 200;
const MAX_ROWS: u32 = 200;
const MAX_COLS: usize = 200;

/// Rows scanned for the used extent before the scan gives up. A generated file can
/// state a million empty rows, and the emitted window is 200 of them.
const MAX_SCAN_ROWS: u32 = 20_000;

/// Track sizes. `MIN_COL_PX` is why a zero-width column cannot break the gutter
/// alignment; the maxima are the same corrupt-geometry bounds xlsx uses.
const MIN_COL_PX: f32 = 2.0;
const MAX_COL_PX: f32 = 2_000.0;
const MAX_ROW_PX: f32 = 1_000.0;

/// A column and a row with no stated size. ODF states neither in the corpus's own
/// workbook, so these are what most sheets actually render at.
const DEFAULT_COL_PX: f32 = 64.0;
const DEFAULT_ROW_PX: f32 = 17.0;

/// Merges read per table. Each one costs the emitter a scan per row it spans.
const MAX_MERGES: usize = 4_000;

/// Cell styles interned per document.
const MAX_STYLES: usize = 4_096;

/// Characters kept from one cell. A cell holding a novel is a cell nobody can read
/// in a grid.
const MAX_CELL_CHARS: usize = 200;

/// Font size a cell style's percentage resolves against, i.e. a spreadsheet's own
/// default rather than a document's body text.
const DEFAULT_CELL_PT: f32 = 10.0;

const NOTE_BODY: &str = "Workbook sheets unreadable";
/// A sheet's charts, images and drawing shapes. They are anchored to cell
/// coordinates rather than laid out in the grid, and nothing here places them — the
/// same gap the xlsx path fills only for pictures.
const NOTE_SHAPES: &str = "Charts and images not shown";
const NOTE_NO_VALUE: &str = "Some cells have no saved value";

pub fn render(
    package: Package,
    notes: Notes,
    section: Option<u32>,
    terms: &[String],
) -> Result<OfficeDoc, String> {
    render_with(package, notes, section, terms, HTML_CAP)
}

fn render_with(
    package: Package,
    mut notes: Notes,
    section: Option<u32>,
    terms: &[String],
    html_cap: usize,
) -> Result<OfficeDoc, String> {
    let Package {
        content,
        styles: styles_xml,
        ..
    } = package;

    let styles = Styles::parse(styles_xml.as_deref(), &content);
    let numbers = Numbers::parse(styles_xml.as_deref(), &content);

    let parsed = xml::parse(&content)?;
    let root = parsed.root_element();
    let tables = tables(root);
    if tables.is_empty() {
        return Err(NOTE_BODY.to_string());
    }

    let last = tables.len().saturating_sub(1) as u32;
    let idx = section.map(|s| s.min(last)).unwrap_or(0);
    let sections = section_names(&tables);

    let table = tables[idx as usize];
    let query = Terms::new(terms);
    let mut hl = Highlight::new();
    let mut interned = SheetStyles::new();

    // ── structure ───────────────────────────────────────────────────────────
    let cols = columns(&styles, table);
    let rows = Rows::plan(&styles, table);
    if rows.shapes || has_shapes(table) {
        notes.add(NOTE_SHAPES);
    }
    // The used range decides the width, not the `table:table-column` count: a
    // producer may declare fewer columns than its rows fill, and a declared column
    // past the data is an empty column nobody wrote in. The declarations are the
    // *sizes* (see the `resize` below), not the extent.
    let ncols = rows.ncols.clamp(1, MAX_COLS);
    let nrows = rows.nrows.min(MAX_ROWS);
    if nrows == 0 {
        // A sheet whose only content is a chart is not "empty" in a way the reader
        // caused, so the card says which it is.
        let msg = if rows.shapes || has_shapes(table) {
            "This sheet holds no cell data."
        } else {
            "This sheet is empty."
        };
        return Ok(empty(sections, idx, notes, msg));
    }
    let mut col_tracks = cols;
    // A column the sheet never declared still needs a track, or the gutter and the
    // cells disagree about where a column edge is.
    col_tracks.resize(ncols, Track::new(DEFAULT_COL_PX));
    let row_tracks = rows.tracks(nrows);
    let mut merges = rows.merges(nrows, ncols);

    let layout = sheet::build_geometry(col_tracks, row_tracks, nrows, ncols);
    resolve_anchors(&mut merges, &layout.rows, &layout.cols);
    // ODF keeps a frozen pane in `settings.xml`, keyed by table name, and the
    // corpus's own workbook states none. Until that part is read, no sheet claims
    // one: a wrongly frozen row is worse than none.
    let frozen = Frozen::clamp(0, 0, nrows, ncols);
    let frozen_css = sheet::frozen_pane_css(&layout, frozen);

    let mut w = super::super::html::Writer::new(html_cap);
    let mut classes = sheet::Classes::default();
    sheet::emit_head(&mut w, &layout, frozen, true, &mut classes);
    let mut src = Source {
        rows: &rows,
        styles: &styles,
        numbers: &numbers,
        interned: &mut interned,
        cells: HashMap::new(),
        missing_value: false,
    };
    sheet::emit_rows(
        &mut w,
        &layout,
        &merges,
        &mut src,
        frozen,
        &mut hl,
        &query,
        &mut classes,
    );
    let missing_value = src.missing_value;
    w.close(); // table
    w.close(); // xl-grid
    w.close(); // xl-scroll
    w.close(); // xl-doc

    if missing_value {
        notes.add(NOTE_NO_VALUE);
    }
    if rows.nrows > nrows {
        notes.add(&format!("First {nrows} rows only"));
    }
    if rows.ncols > ncols {
        notes.add(&format!("First {ncols} columns only"));
    }

    let truncated = w.truncated() || rows.nrows > nrows || rows.ncols > ncols;
    let css = sheet::collect_css(classes, &frozen_css, &interned);
    Ok(OfficeDoc {
        html: emit::wrap_style(sheet::BASE_CSS, &css, w.finish()),
        shape: Shape::Sheet,
        sections,
        section: idx,
        // A grid has neither a slide canvas nor a page box.
        natural: None,
        page: None,
        best_mark_id: hl.best_mark_id(),
        truncated,
        notes: notes.into_vec(),
    })
}

// ── sections ─────────────────────────────────────────────────────────────────
//
// The section list is the seam the content index shares with the renderer: the
// index stores one row per section and hands the section number back, so both
// sides must enumerate the workbook's tables the same way. One function each,
// used by both, rather than two matching loops.

/// The document's tables, in body order.
pub(super) fn tables<'a>(root: Node<'a, 'a>) -> Vec<Node<'a, 'a>> {
    xml::child(root, "body")
        .and_then(|b| xml::child(b, "spreadsheet"))
        .map(|b| {
            elems(b)
                .filter(|n| n.tag_name().name() == "table")
                .take(MAX_SHEETS)
                .collect()
        })
        .unwrap_or_default()
}

/// Sheet names for the tab strip, positional when a table states none.
pub(super) fn section_names(tables: &[Node]) -> Vec<String> {
    tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            attr_local(*t, "name")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Sheet {}", i + 1))
        })
        .collect()
}

/// One table's cells as flat text for the content index, rows separated by
/// newlines and cells by tabs.
///
/// Reads the *stored* display text, which is what the grid shows — so a formatted
/// number indexes the way the author saw it. A cell holding only `office:value`
/// (one its producer never displayed) contributes the raw value: the renderer runs
/// it through a number style, and reproducing that here would mean carrying the
/// style cascade into the index for a cell the preview marks as having no saved
/// text anyway.
///
/// Repetition contributes its text once. `table:number-columns-repeated` reaches
/// 16368 in a real file, and 16368 copies of the same word is not recall.
pub(super) fn table_text(table: Node) -> String {
    let mut out = String::new();
    let mut r: u32 = 0;
    for row in elems(table).filter(|n| n.tag_name().name() == "table-row") {
        r += attr_u32(row, "number-rows-repeated").unwrap_or(1).max(1);
        // Clipped where the renderer clips: a row past the window is not drawn, so
        // a hit in it could never be highlighted.
        if r > MAX_ROWS {
            break;
        }
        let mut c: usize = 0;
        let mut cells = 0usize;
        for cell in elems(row).filter(|n| {
            matches!(n.tag_name().name(), "table-cell" | "covered-table-cell")
        }) {
            c += attr_u32(cell, "number-columns-repeated").unwrap_or(1).max(1) as usize;
            if c > MAX_COLS {
                break;
            }
            let mut text = String::new();
            xml::odf_walk(cell, &mut text);
            let text = text.trim();
            let owned = if text.is_empty() {
                attr_local(cell, "value").unwrap_or("").trim().to_string()
            } else {
                text.replace('\n', " ")
            };
            if owned.is_empty() {
                continue;
            }
            if cells > 0 {
                out.push('\t');
            }
            out.push_str(&owned);
            cells += 1;
        }
        if cells > 0 {
            out.push('\n');
        }
    }
    out
}

/// A sheet with nothing in it: the grid's own empty card, so the tab strip still
/// works and the reader can switch to a sheet that has content.
fn empty(sections: Vec<String>, idx: u32, notes: Notes, msg: &str) -> OfficeDoc {
    OfficeDoc {
        html: sheet::error_body(msg),
        shape: Shape::Sheet,
        sections,
        section: idx,
        natural: None,
        page: None,
        best_mark_id: None,
        truncated: false,
        notes: notes.into_vec(),
    }
}

// ── columns ──────────────────────────────────────────────────────────────────

/// The column tracks, with `table:number-columns-repeated` expanded.
fn columns(styles: &Styles, table: Node) -> Vec<Track> {
    let mut out: Vec<Track> = Vec::new();
    collect_columns(styles, table, &mut out, 0);
    out
}

fn collect_columns(styles: &Styles, parent: Node, out: &mut Vec<Track>, depth: usize) {
    if depth > 4 {
        return;
    }
    for n in elems(parent) {
        if out.len() >= MAX_COLS {
            return;
        }
        match n.tag_name().name() {
            "table-column" => {
                let r = styles.resolve(Family::TableColumn, attr_local(n, "style-name").unwrap_or(""));
                let px = r
                    .column
                    .width_px
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .map(|v| v.clamp(MIN_COL_PX, MAX_COL_PX))
                    .unwrap_or(DEFAULT_COL_PX);
                let mut t = Track::new(px);
                t.hidden = hidden(n);
                // `table:default-cell-style-name` styles the column's *cells*, not
                // the column, and a cell that states its own style wins — so it is
                // read where a cell is resolved rather than stored here.
                let reps = repeat(n, "number-columns-repeated").min(MAX_COLS - out.len());
                for _ in 0..reps {
                    out.push(t);
                }
            }
            "table-columns" | "table-header-columns" | "table-column-group" => {
                collect_columns(styles, n, out, depth + 1)
            }
            _ => {}
        }
    }
}

/// Whether a track is absent from the rendered grid. ODF has two kinds of hidden —
/// `collapse` is the author's, `filter` is a filter's — and both render as absent;
/// only a future "rows hidden by a filter" note would need to tell them apart.
fn hidden(n: Node) -> bool {
    matches!(attr_local(n, "visibility"), Some("collapse") | Some("filter"))
}

/// A `table:number-*-repeated` count: at least one, because the element itself is
/// the first copy, and bounded because it is document-controlled.
fn repeat(n: Node, name: &str) -> usize {
    attr_u32(n, name)
        .map(|v| (v as usize).clamp(1, MAX_COLS.max(MAX_ROWS as usize)))
        .unwrap_or(1)
}

// ── rows ─────────────────────────────────────────────────────────────────────

/// The rows of one table as an index rather than as content: which node covers each
/// row number, how wide the used range is, and where the merges are.
///
/// Repetition is expanded here and nowhere else. A row repeated 30 000 times is one
/// node in this index, referenced by every row number it covers, so the cost is the
/// index rather than the sheet.
struct Rows<'d> {
    /// One entry per used row, 1-based: `at[r - 1]` is the node covering row `r`.
    at: Vec<Node<'d, 'd>>,
    nrows: u32,
    ncols: usize,
    /// Row heights, parallel to `at`.
    px: Vec<f32>,
    hidden: Vec<bool>,
    /// A cell somewhere holds a chart, an image or a drawing shape. Collected here
    /// because this walk already visits every cell element, and a `descendants()`
    /// scan of a megabyte-long table to answer one boolean is not free.
    shapes: bool,
}

impl<'d> Rows<'d> {
    fn plan(styles: &Styles, table: Node<'d, 'd>) -> Rows<'d> {
        let mut rows = Rows {
            at: Vec::new(),
            nrows: 0,
            ncols: 0,
            px: Vec::new(),
            hidden: Vec::new(),
            shapes: false,
        };
        rows.collect(styles, table, 0);
        // A trailing run of empty rows is what a producer writes to spell "the rest
        // of the sheet"; the used range ends at the last row with content in it.
        while rows
            .at
            .last()
            .is_some_and(|n| row_width(*n) == 0)
        {
            rows.at.pop();
            rows.px.pop();
            rows.hidden.pop();
        }
        rows.nrows = rows.at.len() as u32;
        rows
    }

    fn collect(&mut self, styles: &Styles, parent: Node<'d, 'd>, depth: usize) {
        if depth > 4 {
            return;
        }
        for n in elems(parent) {
            if self.at.len() as u32 >= MAX_SCAN_ROWS {
                return;
            }
            match n.tag_name().name() {
                "table-row" => {
                    let r = styles
                        .resolve(Family::TableRow, attr_local(n, "style-name").unwrap_or(""));
                    let px = r
                        .row
                        .height_px
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .map(|v| v.min(MAX_ROW_PX))
                        .unwrap_or(DEFAULT_ROW_PX);
                    let hid = hidden(n);
                    if !self.shapes {
                        self.shapes = elems(n).any(|tc| {
                            elems(tc).any(|c| {
                                matches!(
                                    c.tag_name().name(),
                                    "frame" | "custom-shape" | "g" | "object" | "image"
                                )
                            })
                        });
                    }
                    let width = row_width(n);
                    // The extent grows only for a row that has content: a row of
                    // 16368 empty repeated cells states no columns.
                    if width > 0 {
                        self.ncols = self.ncols.max(width);
                    }
                    let reps = repeat(n, "number-rows-repeated")
                        .min(MAX_SCAN_ROWS as usize - self.at.len());
                    for _ in 0..reps {
                        self.at.push(n);
                        self.px.push(px);
                        self.hidden.push(hid);
                    }
                }
                "table-header-rows" | "table-rows" | "table-row-group" => {
                    self.collect(styles, n, depth + 1)
                }
                _ => {}
            }
        }
    }

    /// The row tracks the emitter's own indexing wants: **1-based**, with an unused
    /// slot at 0 and one past the last row, so `build_geometry` can write the
    /// closing edge without a bounds check. Getting this length wrong is a panic,
    /// not a wrong pixel.
    fn tracks(&self, nrows: u32) -> Vec<Track> {
        let mut out = vec![Track::new(DEFAULT_ROW_PX); nrows as usize + 2];
        for r in 1..=nrows as usize {
            let t = &mut out[r];
            t.px = self.px.get(r - 1).copied().unwrap_or(DEFAULT_ROW_PX);
            t.hidden = self.hidden.get(r - 1).copied().unwrap_or(false);
        }
        out
    }

    /// The merges inside the emitted window. ODF states a span on the cell that
    /// opens it, so there is no pre-pass: the covered slots are the ones the
    /// emitter skips.
    fn merges(&self, nrows: u32, ncols: usize) -> Vec<Merge> {
        let mut out = Vec::new();
        for r in 1..=nrows {
            let Some(node) = self.at.get(r as usize - 1) else {
                break;
            };
            let mut c = 0usize;
            for tc in elems(*node) {
                if c >= ncols || out.len() >= MAX_MERGES {
                    break;
                }
                let name = tc.tag_name().name();
                if !matches!(name, "table-cell" | "covered-table-cell") {
                    continue;
                }
                let reps = repeat(tc, "number-columns-repeated");
                if name == "table-cell" {
                    let cs = span(tc, "number-columns-spanned", ncols);
                    let rs = span(tc, "number-rows-spanned", nrows as usize);
                    if cs > 1 || rs > 1 {
                        let r1 = (r as usize + rs - 1).min(nrows as usize) as u32;
                        out.push(Merge {
                            r0: r,
                            r1,
                            c0: c,
                            c1: (c + cs - 1).min(ncols - 1),
                            // Corrected by `resolve_anchors` once visibility is
                            // known; the opening cell is the anchor until then.
                            ar: r,
                            ac: c,
                        });
                    }
                }
                c += reps;
            }
        }
        out
    }

    /// The cell nodes of one row, mapped to the columns they cover.
    fn cells(&self, r: u32, ncols: usize) -> HashMap<usize, Node<'d, 'd>> {
        let mut out = HashMap::new();
        let Some(node) = self.at.get(r as usize - 1) else {
            return out;
        };
        let mut c = 0usize;
        for tc in elems(*node) {
            if c >= ncols {
                break;
            }
            let name = tc.tag_name().name();
            if !matches!(name, "table-cell" | "covered-table-cell") {
                continue;
            }
            let reps = repeat(tc, "number-columns-repeated");
            if name == "table-cell" && has_paint(tc) {
                for i in 0..reps {
                    if c + i >= ncols {
                        break;
                    }
                    out.insert(c + i, tc);
                }
            }
            c += reps;
        }
        out
    }
}

/// How many columns a row's cells reach, ignoring the trailing repeat that spells
/// "and the rest of the sheet".
///
/// Two kinds of cell count. One holding data obviously does. A
/// `table:covered-table-cell` counts too, even though it holds nothing: it exists
/// *because* a merge from an earlier row or column covers it, so a row of nothing but
/// covered slots is part of that merge rather than an empty row past the end of the
/// sheet.
///
/// Every element — covered or not — occupies exactly its
/// `table:number-columns-repeated`, and **not** its span: ODF writes one explicit
/// covered slot for each further column a span reaches, so counting the span as well
/// would place every later cell in the row one column too far right.
fn row_width(row: Node) -> usize {
    let mut c = 0usize;
    let mut last = 0usize;
    for tc in elems(row) {
        let covered = match tc.tag_name().name() {
            "table-cell" => false,
            "covered-table-cell" => true,
            _ => continue,
        };
        let reps = repeat(tc, "number-columns-repeated");
        if covered || has_data(tc) {
            last = c + reps;
        }
        c += reps;
        if c > MAX_COLS {
            break;
        }
    }
    last.min(MAX_COLS)
}

/// Whether a sheet carries anything drawn over the grid rather than in it. A sheet
/// whose only content is a chart otherwise renders as an empty grid with no
/// explanation, which reads as a broken preview.
fn has_shapes(table: Node) -> bool {
    xml::child(table, "shapes").is_some()
        || elems(table).any(|n| matches!(n.tag_name().name(), "frame" | "custom-shape" | "g"))
}

/// Whether a cell holds data, which is what the *used range* means.
///
/// Deliberately not "has a style": ODS writes `table:style-name` onto the run of
/// 16368 empty cells it uses to spell "and the rest of the row", so counting a style
/// as content makes every sheet as wide as the emitter's column cap. A style-only
/// cell still paints — see [`has_paint`] — it just does not decide the extent.
fn has_data(tc: Node) -> bool {
    attr_local(tc, "value-type").is_some()
        || attr_local(tc, "value").is_some()
        || attr_local(tc, "formula").is_some()
        || elems(tc).any(|c| matches!(c.tag_name().name(), "p" | "h" | "list"))
}

/// Whether a cell is worth resolving at all: data, or a style that might paint it.
/// Inside the used range an empty shaded cell is part of the table the author drew.
fn has_paint(tc: Node) -> bool {
    has_data(tc) || attr_local(tc, "style-name").is_some()
}

fn span(n: Node, name: &str, max: usize) -> usize {
    attr_u32(n, name)
        .map(|v| (v as usize).clamp(1, max.max(1)))
        .unwrap_or(1)
}

// ── cells ────────────────────────────────────────────────────────────────────

/// Resolves one cell at a time, in the order the emitter asks for them.
struct Source<'a, 'd> {
    rows: &'a Rows<'d>,
    styles: &'a Styles,
    numbers: &'a Numbers,
    interned: &'a mut SheetStyles,
    /// The current row's cells by column. Rebuilt per row, which is what keeps a
    /// 16368-wide repeat from ever being materialized as cells.
    cells: HashMap<usize, Node<'d, 'd>>,
    /// A formula whose cached value the producer did not write. Reported once, as a
    /// note, because the cell renders empty and that looks like a bug in the
    /// preview rather than in the file.
    missing_value: bool,
}

impl CellSource for Source<'_, '_> {
    fn row(&mut self, r: u32) {
        self.cells = self.rows.cells(r, MAX_COLS);
    }

    fn cell(&mut self, c: usize) -> Cell {
        let Some(node) = self.cells.get(&c).copied() else {
            return Cell::default();
        };
        let style_name = attr_local(node, "style-name").unwrap_or("");
        let resolved = self.styles.resolve(Family::TableCell, style_name);
        let style = (!style_name.is_empty())
            .then(|| self.interned.intern(style_name, &resolved))
            .flatten();

        let kind = attr_local(node, "value-type");
        let text = match display_text(node) {
            // The producer's own formatting of the cell, which is what the author
            // saw — no number format is evaluated to get it.
            Some(t) => t,
            None => {
                let formatted = self.formatted(node, style_name);
                if formatted.is_none() && attr_local(node, "formula").is_some() {
                    self.missing_value = true;
                }
                formatted.unwrap_or_default()
            }
        };
        Cell {
            text: clip(&text),
            align: align_of(kind),
            style,
            inner: resolved.cell.rotation.is_some(),
            color: None,
        }
    }
}

impl Source<'_, '_> {
    /// A cell with a value but no display text: the number style is the only thing
    /// that can say what it should look like.
    fn formatted(&self, node: Node, style_name: &str) -> Option<String> {
        let raw = attr_local(node, "value")
            .or_else(|| attr_local(node, "date-value"))
            .or_else(|| attr_local(node, "time-value"))
            .or_else(|| attr_local(node, "boolean-value"))?;
        let value: f64 = raw.parse().ok()?;
        match self
            .styles
            .data_style_of(style_name)
            .and_then(|n| self.numbers.format(n))
        {
            Some(f) => Some(f.apply(value)),
            // No number style: the value itself, which is better than nothing at
            // all in the one cell a producer left undisplayed.
            None => Some(trim_float(value)),
        }
    }
}

/// The cell's stored display text. Multiple paragraphs join with a newline, which
/// the sheet stylesheet's `white-space` renders as the lines they are — the same
/// flattening the xlsx path applies to rich text.
fn display_text(node: Node) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for p in elems(node).filter(|c| matches!(c.tag_name().name(), "p" | "h")) {
        let mut s = String::new();
        xml::inner_text(p, &mut s);
        parts.push(s);
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// A value's own alignment, the way a spreadsheet aligns by type rather than by
/// style: numbers and dates right, booleans centred, text left.
fn align_of(kind: Option<&str>) -> Option<Align> {
    match kind {
        Some("float") | Some("percentage") | Some("currency") | Some("date") | Some("time") => {
            Some(Align::Right)
        }
        Some("boolean") => Some(Align::Center),
        _ => None,
    }
}

fn trim_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{v}");
    if s.len() > 24 {
        s.truncate(24);
    }
    s
}

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_CELL_CHARS {
        return s.to_string();
    }
    s.chars().take(MAX_CELL_CHARS).collect::<String>() + "…"
}

// ── the style table ──────────────────────────────────────────────────────────

/// ODF's named cell styles as the ordinals `sheetmodel::Cell` carries.
///
/// The interning exists because `Cell::style` is an `Option<u32>` — an index into a
/// deduplicated rule table, which is how SpreadsheetML numbers its formats natively.
/// Only equality matters to the emitter, so a name mapped to a position is lossless;
/// what the type cannot express is that these ordinals mean nothing outside this
/// document.
struct SheetStyles {
    /// `name → ordinal`, in first-seen order.
    ids: HashMap<String, u32>,
    /// The CSS for each ordinal.
    css: Vec<String>,
}

impl SheetStyles {
    fn new() -> SheetStyles {
        SheetStyles {
            ids: HashMap::new(),
            css: Vec::new(),
        }
    }

    /// The ordinal for one style name, building its CSS the first time it is asked
    /// for. `None` once the table is full, which renders the cell unstyled rather
    /// than growing an unbounded stylesheet.
    fn intern(&mut self, name: &str, resolved: &Resolved) -> Option<u32> {
        if let Some(id) = self.ids.get(name) {
            return Some(*id);
        }
        if self.css.len() >= MAX_STYLES {
            return None;
        }
        let css = cell_css(&resolved.cell, &resolved.text);
        // A style that paints nothing needs no rule and no class.
        if css.is_empty() {
            return None;
        }
        let id = self.css.len() as u32;
        self.css.push(css);
        self.ids.insert(name.to_string(), id);
        Some(id)
    }
}

impl StyleTable for SheetStyles {
    fn css_block(&self, used: &BTreeSet<u32>) -> String {
        let mut out = String::new();
        for id in used {
            if let Some(css) = self.css.get(*id as usize) {
                out.push_str(&format!("td.xf{id}{{{css}}}\n"));
            }
        }
        out
    }
}

/// One cell style's declarations: its paint, its padding, and how its content sits.
fn cell_css(cp: &CellProps, tp: &TextProps) -> String {
    let mut s = Style::new();
    borders(&mut s, &cp.borders);
    if let Some(c) = cp.background.as_ref() {
        s.push("background-color", &c.css());
    }
    for (side, px) in [
        ("top", cp.padding.top),
        ("right", cp.padding.right),
        ("bottom", cp.padding.bottom),
        ("left", cp.padding.left),
    ] {
        s.push_opt(
            &format!("padding-{side}"),
            px.filter(|v| v.is_finite() && *v >= 0.0).and_then(fmt_px),
        );
    }
    if let Some(f) = tp.font.as_deref() {
        s.push("font-family", f);
    }
    // Only a style that states a size gets one: the stylesheet's own is what an
    // unstyled cell should keep.
    s.push_opt(
        "font-size",
        tp.size
            .map(|_| super::style::size_pt(tp, DEFAULT_CELL_PT))
            .map(pt_to_px)
            .and_then(fmt_px),
    );
    if tp.bold == Some(true) {
        s.push("font-weight", "700");
    }
    if tp.italic == Some(true) {
        s.push("font-style", "italic");
    }
    match (tp.underline, tp.strike) {
        (Some(true), Some(true)) => s.push("text-decoration", "underline line-through"),
        (Some(true), _) => s.push("text-decoration", "underline"),
        (_, Some(true)) => s.push("text-decoration", "line-through"),
        _ => {}
    }
    if let Some(c) = tp.color.as_ref().and_then(|c| c.color()) {
        s.push("color", &c.css());
    }
    let align = align_css(&AlignSpec {
        // A cell's horizontal alignment is its paragraph's `fo:text-align`, which
        // the emitter carries as `Cell::align` when the value implies one.
        horizontal: "general",
        vertical: cp.v_align,
        wrap: cp.wrap == Some(true),
        indent_px: 0.0,
        rotation: cp.rotation,
    });
    let mut css = s.css().to_string();
    css.push_str(&align.cell);
    css
}

fn borders(s: &mut Style, b: &Sides) {
    for (side, edge) in [
        ("top", b.top),
        ("right", b.right),
        ("bottom", b.bottom),
        ("left", b.left),
    ] {
        let Some(e) = edge.and_then(|e| match e {
            Edge::Set(b) => Some(b),
            Edge::None => None,
        }) else {
            continue;
        };
        if let Some(css) = e.css() {
            s.push(&format!("border-{side}"), &css);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::office::odf::text::tests::{Fixture, NS};

    /// A spreadsheet package. `named` goes into `styles.xml`'s `office:styles`,
    /// `auto` into `content.xml`'s automatic styles, `tables` into the body.
    fn book(tag: &str, named: &str, auto: &str, tables: &str) -> Fixture {
        let styles = format!(
            "<office:document-styles {NS} \
             xmlns:number=\"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0\">\
             <office:styles>{named}</office:styles></office:document-styles>"
        );
        let content = format!(
            "<office:document-content {NS} \
             xmlns:number=\"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0\">\
             <office:automatic-styles>{auto}</office:automatic-styles>\
             <office:body><office:spreadsheet>{tables}\
             </office:spreadsheet></office:body></office:document-content>"
        );
        Fixture::new(
            tag,
            &[
                (
                    "mimetype",
                    b"application/vnd.oasis.opendocument.spreadsheet".to_vec(),
                ),
                ("styles.xml", styles.into_bytes()),
                ("content.xml", content.into_bytes()),
            ],
        )
    }

    /// A sheet with `cols` column elements and `rows` markup.
    fn sheet(name: &str, cols: &str, rows: &str) -> String {
        format!("<table:table table:name=\"{name}\">{cols}{rows}</table:table>")
    }

    /// A row of plain string cells.
    fn text_row(cells: &[&str]) -> String {
        let body: String = cells
            .iter()
            .map(|c| {
                format!(
                    "<table:table-cell office:value-type=\"string\"><text:p>{c}</text:p>\
                     </table:table-cell>"
                )
            })
            .collect();
        format!("<table:table-row>{body}</table:table-row>")
    }

    /// The `<td>` elements of the rendered grid, in document order.
    fn cells(html: &str) -> Vec<String> {
        let body = html.split_once("</style>").map(|(_, b)| b).unwrap_or(html);
        body.split("<td")
            .skip(1)
            .map(|chunk| {
                let inner = chunk.splitn(2, '>').nth(1).unwrap_or("");
                inner
                    .split("</td>")
                    .next()
                    .unwrap_or("")
                    .replace("<span class=\"xr\">", "")
                    .replace("</span>", "")
            })
            .collect()
    }

    #[test]
    fn a_workbook_renders_one_sheet_and_lists_them_all() {
        let f = book(
            "book",
            "",
            "",
            &format!(
                "{}{}",
                sheet("Alap", "<table:table-column/>", &text_row(&["café"])),
                sheet("Widget", "<table:table-column/>", &text_row(&["naïve"])),
            ),
        );
        let doc = f.doc();
        assert_eq!(doc.shape, Shape::Sheet);
        assert_eq!(doc.sections, vec!["Alap".to_string(), "Widget".to_string()]);
        assert_eq!(doc.section, 0);
        assert!(doc.natural.is_none() && doc.page.is_none());
        assert_eq!(cells(&doc.html), vec!["café".to_string()]);
        // The shared emitter's own scaffolding, so the grid is the xlsx grid.
        assert!(doc.html.contains("class=\"xl-doc\""), "{}", doc.html);
        assert!(doc.html.contains("class=\"xl-sheet"), "{}", doc.html);
    }

    #[test]
    fn a_section_past_the_end_clamps_and_an_unnamed_sheet_gets_a_number() {
        let f = book(
            "clamp",
            "",
            "",
            &format!(
                "{}<table:table>{}</table:table>",
                sheet("First", "<table:table-column/>", &text_row(&["a"])),
                text_row(&["b"])
            ),
        );
        let doc = super::super::render(f.path(), Some(42), &[]).expect("render");
        assert_eq!(doc.section, 1);
        assert_eq!(doc.sections[1], "Sheet 2");
        assert_eq!(cells(&doc.html), vec!["b".to_string()]);
    }

    #[test]
    fn the_stored_display_text_is_what_the_grid_shows() {
        let rows = "<table:table-row>\
             <table:table-cell office:value-type=\"float\" office:value=\"433.5234\">\
             <text:p>433.52 km</text:p></table:table-cell>\
             <table:table-cell office:value-type=\"float\" office:value=\"1752\">\
             <text:p>1,752 m</text:p></table:table-cell>\
             </table:table-row>";
        let html = book("display", "", "", &sheet("S", "<table:table-column/>", rows)).html();
        // The producer already formatted these, which is what the author saw: no
        // number format is evaluated to get them, so the grouping and the unit
        // survive.
        assert_eq!(
            cells(&html),
            vec!["433.52 km".to_string(), "1,752 m".to_string()]
        );
        // A value aligns right whatever its text says.
        assert!(html.contains("xl-num"), "{html}");
    }

    #[test]
    fn a_value_with_no_display_text_falls_back_to_its_number_style() {
        let named = "<number:number-style style:name=\"N2\">\
             <number:number number:decimal-places=\"2\" number:min-integer-digits=\"1\" \
             number:grouping=\"true\"/></number:number-style>\
             <style:style style:name=\"ce1\" style:family=\"table-cell\" \
             style:data-style-name=\"N2\"/>";
        let rows = "<table:table-row>\
             <table:table-cell table:style-name=\"ce1\" office:value-type=\"float\" \
             office:value=\"1234.567\"/>\
             <table:table-cell office:value-type=\"float\" office:value=\"42\"/>\
             </table:table-row>";
        let html = book("numfmt", named, "", &sheet("S", "<table:table-column/>", rows)).html();
        // Its own `style:data-style-name`, through the same engine the xlsx path
        // uses for a format code.
        assert_eq!(
            cells(&html),
            vec!["1,234.57".to_string(), "42".to_string()]
        );
    }

    #[test]
    fn a_formula_with_no_cached_value_is_empty_and_says_why() {
        let rows = "<table:table-row>\
             <table:table-cell table:formula=\"of:=SUM([.A1:.A9])\"/>\
             <table:table-cell office:value-type=\"string\"><text:p>café</text:p>\
             </table:table-cell></table:table-row>";
        let doc = book("nocache", "", "", &sheet("S", "<table:table-column/>", rows)).doc();
        assert_eq!(cells(&doc.html), vec!["".to_string(), "café".to_string()]);
        // Reported once: an empty cell where a total should be looks like a broken
        // preview rather than a workbook saved without its results.
        assert!(
            doc.notes.iter().any(|n| n == NOTE_NO_VALUE),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn repetition_is_expanded_without_being_materialized() {
        // How ODS spells "and the rest of the row": a styled empty cell, repeated
        // to the end of the sheet.
        let rows = "<table:table-row>\
             <table:table-cell office:value-type=\"string\"><text:p>café</text:p>\
             </table:table-cell>\
             <table:table-cell table:style-name=\"ce1\" table:number-columns-repeated=\"16368\"/>\
             </table:table-row>\
             <table:table-row table:number-rows-repeated=\"3\">\
             <table:table-cell office:value-type=\"float\" office:value=\"1\">\
             <text:p>1</text:p></table:table-cell></table:table-row>\
             <table:table-row table:number-rows-repeated=\"1048576\"/>";
        let doc = book(
            "repeat",
            "<style:style style:name=\"ce1\" style:family=\"table-cell\"/>",
            "",
            &sheet("S", "<table:table-column table:number-columns-repeated=\"4\"/>", rows),
        )
        .doc();
        // The used range is where the *data* is: a styled empty cell is padding, so
        // one column of content stays one column.
        assert_eq!(cells(&doc.html), vec!["café", "1", "1", "1"]);
        // Four rows plus the header, not a million: the trailing empty run is the
        // format's way of spelling the end of the sheet.
        assert_eq!(doc.html.matches("<tr").count(), 5, "{}", doc.html);
        assert!(!doc.truncated);
    }

    #[test]
    fn a_span_becomes_rowspan_and_colspan_with_the_covered_slots_dropped() {
        let rows = "<table:table-row>\
             <table:table-cell table:number-columns-spanned=\"2\" \
             table:number-rows-spanned=\"2\" office:value-type=\"string\">\
             <text:p>wide</text:p></table:table-cell>\
             <table:covered-table-cell/></table:table-row>\
             <table:table-row><table:covered-table-cell/>\
             <table:covered-table-cell/></table:table-row>";
        let html = book(
            "spans",
            "",
            "",
            &sheet("S", "<table:table-column table:number-columns-repeated=\"2\"/>", rows),
        )
        .html();
        assert!(html.contains("rowspan=\"2\""), "{html}");
        assert!(html.contains("colspan=\"2\""), "{html}");
        assert_eq!(cells(&html), vec!["wide".to_string()]);
    }

    #[test]
    fn a_cell_style_becomes_one_rule_the_cells_share() {
        let named = "<style:style style:name=\"ce1\" style:family=\"table-cell\">\
             <style:table-cell-properties fo:background-color=\"#eeeeee\" \
             fo:border=\"0.5pt solid #808080\" style:vertical-align=\"middle\" \
             fo:padding-left=\"6pt\"/>\
             <style:text-properties fo:font-weight=\"bold\" fo:color=\"#ff0000\" \
             fo:font-size=\"14pt\"/></style:style>";
        let rows = format!(
            "<table:table-row>\
             <table:table-cell table:style-name=\"ce1\" office:value-type=\"string\">\
             <text:p>a</text:p></table:table-cell>\
             <table:table-cell table:style-name=\"ce1\" office:value-type=\"string\">\
             <text:p>b</text:p></table:table-cell>\
             <table:table-cell office:value-type=\"string\"><text:p>c</text:p>\
             </table:table-cell></table:table-row>"
        );
        let html = book(
            "cellstyle",
            named,
            "",
            &sheet("S", "<table:table-column table:number-columns-repeated=\"3\"/>", &rows),
        )
        .html();
        // One rule for the style, referenced by class — not the same declarations
        // inlined onto every cell that shares it.
        assert_eq!(html.matches("td.xf0{").count(), 1, "{html}");
        assert_eq!(html.matches("class=\"xf0").count(), 2, "{html}");
        assert!(html.contains("background-color:#eeeeee"), "{html}");
        assert!(html.contains("solid #808080"), "{html}");
        assert!(html.contains("font-weight:700"), "{html}");
        assert!(html.contains("color:#ff0000"), "{html}");
        // 14pt is 18.67px: the size is the document's, unrounded.
        assert!(html.contains("font-size:18.67px"), "{html}");
        assert!(html.contains("padding-left:8px"), "{html}");
        assert!(html.contains("vertical-align:middle"), "{html}");
        // The unstyled cell carries no style class at all.
        assert!(html.contains("<td class=\"xl-t\">c</td>"), "{html}");
    }

    #[test]
    fn column_widths_and_row_heights_come_from_their_styles() {
        let auto = "<style:style style:name=\"co1\" style:family=\"table-column\">\
             <style:table-column-properties style:column-width=\"1.5in\"/></style:style>\
             <style:style style:name=\"ro1\" style:family=\"table-row\">\
             <style:table-row-properties style:row-height=\"0.5in\"/></style:style>";
        let rows = "<table:table-row table:style-name=\"ro1\">\
             <table:table-cell office:value-type=\"string\"><text:p>a</text:p>\
             </table:table-cell></table:table-row>";
        let html = book(
            "tracks",
            "",
            auto,
            &sheet("S", "<table:table-column table:style-name=\"co1\"/>", rows),
        )
        .html();
        assert!(html.contains("width:144px"), "{html}");
        assert!(html.contains("height:48px"), "{html}");
    }

    #[test]
    fn a_hidden_column_or_row_is_absent_from_the_grid() {
        let auto = "<style:style style:name=\"co1\" style:family=\"table-column\">\
             <style:table-column-properties style:column-width=\"1in\"/></style:style>";
        let cols = "<table:table-column table:style-name=\"co1\"/>\
             <table:table-column table:style-name=\"co1\" table:visibility=\"collapse\"/>\
             <table:table-column table:style-name=\"co1\"/>";
        let rows = format!(
            "{}<table:table-row table:visibility=\"filter\">{}</table:table-row>{}",
            text_row(&["a", "hidden-col", "c"]),
            "<table:table-cell office:value-type=\"string\"><text:p>gone</text:p>\
             </table:table-cell>",
            text_row(&["d", "x", "f"]),
        );
        let doc = book("hidden", "", auto, &sheet("S", cols, &rows)).doc();
        // Both kinds of ODF hidden render as absent: `collapse` is the author's,
        // `filter` is a filter's.
        assert_eq!(cells(&doc.html), vec!["a", "c", "d", "f"]);
        assert!(!doc.html.contains("gone"), "{}", doc.html);
        assert!(!doc.html.contains("hidden-col"), "{}", doc.html);
    }

    #[test]
    fn a_sheet_of_only_a_chart_says_so_rather_than_looking_broken() {
        let rows = "<table:table-row><table:table-cell>\
             <draw:frame svg:width=\"4in\" svg:height=\"3in\">\
             <draw:object xlink:href=\"./Object 1\"/></draw:frame></table:table-cell>\
             </table:table-row>";
        let doc = book("chart", "", "", &sheet("Diagram", "<table:table-column/>", rows)).doc();
        assert!(doc.html.contains("This sheet holds no cell data"), "{}", doc.html);
        assert!(
            doc.notes.iter().any(|n| n == NOTE_SHAPES),
            "{:?}",
            doc.notes
        );
        // The tab strip still works, so the reader can reach a sheet that has data.
        assert_eq!(doc.sections, vec!["Diagram".to_string()]);
    }

    #[test]
    fn the_row_window_is_the_same_one_the_xlsx_path_shows() {
        let rows: String = (0..MAX_ROWS + 20)
            .map(|i| text_row(&[&format!("r{i}")]))
            .collect();
        let doc = book("window", "", "", &sheet("S", "<table:table-column/>", &rows)).doc();
        assert_eq!(doc.html.matches("<tr").count(), MAX_ROWS as usize + 1, "with the header");
        assert!(doc.truncated);
        assert!(
            doc.notes.iter().any(|n| n == "First 200 rows only"),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn multi_paragraph_cells_keep_their_lines_and_long_ones_are_clipped() {
        let long = "x".repeat(MAX_CELL_CHARS + 50);
        let rows = format!(
            "<table:table-row>\
             <table:table-cell office:value-type=\"string\"><text:p>café</text:p>\
             <text:p>naïve</text:p></table:table-cell>\
             <table:table-cell office:value-type=\"string\"><text:p>{long}</text:p>\
             </table:table-cell></table:table-row>"
        );
        let cells = cells(
            &book(
                "multi",
                "",
                "",
                &sheet("S", "<table:table-column table:number-columns-repeated=\"2\"/>", &rows),
            )
            .html(),
        );
        assert_eq!(cells[0], "café\nnaïve");
        assert!(cells[1].chars().count() <= MAX_CELL_CHARS + 1, "{}", cells[1]);
        assert!(cells[1].ends_with('…'));
    }

    #[test]
    fn terms_are_marked_and_the_best_one_is_named() {
        let f = book(
            "terms",
            "",
            "",
            &sheet("S", "<table:table-column/>", &text_row(&["the runner was running"])),
        );
        let doc = super::super::render(f.path(), None, &["run".to_string()]).expect("render");
        assert!(doc.html.contains("<mark class=\"preview-hl\""), "{}", doc.html);
        assert!(doc.best_mark_id.is_some());
    }

    #[test]
    fn a_workbook_with_no_tables_is_an_error() {
        let f = book("notables", "", "", "");
        assert!(super::super::render(f.path(), None, &[]).is_err());
    }

    #[test]
    fn the_track_vector_matches_the_emitters_one_based_indexing() {
        // The contract that made the first version of this renderer panic: the
        // emitter indexes rows by 1-based number and writes a closing edge one past
        // the last row, so the vector is `nrows + 2` long.
        let rows = Rows {
            at: Vec::new(),
            nrows: 3,
            ncols: 1,
            px: vec![10.0, 20.0, 30.0],
            hidden: vec![false, true, false],
            shapes: false,
        };
        let t = rows.tracks(3);
        assert_eq!(t.len(), 5);
        assert_eq!(t[1].px, 10.0);
        assert_eq!(t[3].px, 30.0);
        assert!(t[2].hidden);
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn indexed_sections_are_the_tables_and_carry_their_names() {
        let f = book(
            "index-book",
            "",
            "",
            &format!(
                "{}{}",
                sheet("Alap", "<table:table-column/>", &text_row(&["café"])),
                sheet("Widget", "<table:table-column/>", &text_row(&["naïve"])),
            ),
        );
        let sections = crate::office::odf::extract_sections(f.path()).expect("extract");
        // The index and the tab strip must agree, or a jump opens the wrong sheet.
        assert_eq!(sections.len(), f.doc().sections.len());
        assert_eq!(sections[0].name, "Alap");
        assert_eq!(sections[1].name, "Widget");
        assert!(sections[0].text.contains("café"), "{:?}", sections[0].text);
        assert!(sections[1].text.contains("naïve"), "{:?}", sections[1].text);
    }

    #[test]
    fn indexed_cells_keep_their_displayed_text_and_stay_separate_tokens() {
        // ODF stores the formatted text beside the value, so the index reads what
        // the author saw. A value-only cell (one the producer never displayed)
        // contributes its raw value rather than nothing.
        let rows = "<table:table-row>\
             <table:table-cell office:value-type=\"float\" office:value=\"1752\">\
             <text:p>1,752 m</text:p></table:table-cell>\
             <table:table-cell office:value-type=\"float\" office:value=\"42\"/>\
             </table:table-row>";
        let f = book("index-cells", "", "", &sheet("S", "<table:table-column/>", rows));
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("1,752 m"), "{text:?}");
        assert!(text.contains("42"), "{text:?}");
        assert!(text.contains('\t'), "cells must not fuse: {text:?}");
    }

    #[test]
    fn indexed_rows_stop_where_the_grid_clips() {
        let mut rows = String::new();
        for r in 1..=MAX_ROWS + 5 {
            rows.push_str(&text_row(&[&format!("row{r}")]));
        }
        let f = book("index-clip", "", "", &sheet("S", "<table:table-column/>", &rows));
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains(&format!("row{MAX_ROWS}")));
        assert!(!text.contains(&format!("row{}", MAX_ROWS + 1)), "{text:?}");
    }

    #[test]
    fn a_repeated_cell_contributes_its_text_once() {
        // `table:number-columns-repeated` reaches 16368 in a real file, and 16368
        // copies of one word is not recall.
        let rows = "<table:table-row><table:table-cell table:number-columns-repeated=\"9\" \
             office:value-type=\"string\"><text:p>café</text:p></table:table-cell>\
             </table:table-row>";
        let f = book("index-rep", "", "", &sheet("S", "<table:table-column/>", rows));
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert_eq!(text.matches("café").count(), 1, "{text:?}");
    }
}
