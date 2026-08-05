//! One worksheet → the format-neutral grid model: the SpreadsheetML parse phases,
//! the [`CellSource`] that resolves a `<c>` on demand, and the drawing overlay.
//!
//! The `<table>` itself belongs to [`super::super::sheet`]. Everything from
//! `build_geometry` on speaks only the `sheetmodel` vocabulary, so a second
//! spreadsheet dialect reaches that seam and no further.

use super::super::drawingml::geom::Xf;
use super::super::emit::{self, Notes};
use super::super::html::{attr, attrs, emu_to_px, pt_to_px, Writer};
use super::super::media::{self, Media};
use super::super::model::Align;
use super::super::numfmt::Format;
use super::super::sheet::{self, Classes, Frozen, Grid};
use super::super::sheetmodel::{resolve_anchors, Cell, CellSource, Merge, Track};
use super::super::xml::{
    self, attr_bool, attr_f32, attr_i64, attr_u32, child, descendant, elems, text_of,
};
use super::super::{opc, pkg};
use super::{col_letter_to_index, split_cell_ref, Ctx, SheetRef};

/// Max digit width of the default font (Calibri 11) in px, plus Excel's fixed
/// cell padding. A stored column `width` is a count of those digits, so
/// `px = width * 7 + 5` — which reproduces the familiar default: 8.43 chars is
/// 64px. It is an approximation because the real max-digit-width depends on the
/// workbook's default font, which the preview does not measure.
const MDW_PX: f32 = 7.0;
const COL_PAD_PX: f32 = 5.0;
const DEFAULT_COL_CHARS: f32 = 8.43;
const DEFAULT_ROW_PT: f32 = 15.0;

/// A column narrower than this is still given a sliver, so a hidden-by-zero-width
/// column does not collapse the gutter alignment. `MIN_ROW_PX` is the same guard on
/// the other axis: `ht="0"` is how some producers spell a hidden row, and a
/// genuinely 0px `<tr>` puts the gutter and the grid at different heights, because a
/// row's own cells can still force a box while the gutter cell does not.
///
/// A row the sheet really hides states `hidden="1"`, which drops the track outright.
const MIN_COL_PX: f32 = 2.0;
const MIN_ROW_PX: f32 = 2.0;
const MAX_COL_PX: f32 = 2000.0;
const MAX_ROW_PX: f32 = 1000.0;

const MAX_MERGES: usize = 8192;
const MAX_COL_DEFS: usize = 4096;
const MAX_DRAWINGS: usize = 200;

/// Longest rendered cell string. Excel's own limit is 32767 characters, which a
/// grid cell cannot show anyway.
const MAX_CELL_CHARS: usize = 512;

pub struct SheetOut {
    pub html: String,
    pub truncated: bool,
}


// ── phase state ──────────────────────────────────────────────────────────────
//
// `render` is a pipeline: parse the SpreadsheetML into these structs, hand the
// measurements to `sheet::build_geometry`, then emit. These two carry the
// OOXML-only inputs up to that hand-off; nothing past it names them.

/// Sheet-level view flags and default track sizes.
///
/// SpreadsheetML-specific: `<sheetView>`, `<pane>` and `<sheetFormatPr>` are
/// OOXML spellings with no shared shape in ODF, which stores the equivalents on
/// styles and in a separate settings part.
struct Settings {
    show_lines: bool,
    /// The pane split as stored, before it is clamped to the emitted grid.
    frozen_rows: usize,
    frozen_cols: usize,
    def_col_px: f32,
    def_row_px: f32,
}

/// The sheet's inked rectangle — rows `1..=last_row`, columns `0..ncols` — and
/// whether either axis was cut by the emission caps.
struct Extent {
    last_row: u32,
    ncols: usize,
    rows_clipped: bool,
    cols_clipped: bool,
}


// ── entry point ──────────────────────────────────────────────────────────────

pub fn render(ctx: &mut Ctx, sh: &SheetRef) -> Result<SheetOut, String> {
    let Some(part) = sh.part.clone() else {
        return Err("this sheet is not present in the workbook package".to_string());
    };
    let sheet_xml = pkg::read_entry(ctx.zip, &part, ctx.budget)?
        .ok_or_else(|| format!("xlsx: missing sheet part {part}"))?;
    let doc = xml::parse(&sheet_xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "worksheet" {
        ctx.notes
            .add("No cell grid — chart or macro sheet");
        return Ok(SheetOut {
            html: sheet::error_body("This sheet has no cell grid."),
            truncated: false,
        });
    }

    // Shared references are copied out of `ctx` so the emission loop can keep a
    // mutable borrow of the marker/notes fields at the same time.
    let styles = ctx.styles;
    let sst = ctx.sst;
    let terms = ctx.terms;
    let date1904 = ctx.date1904;

    // ── parse ───────────────────────────────────────────────────────────────
    let settings = sheet_settings(root);
    let cols = parse_cols(root, settings.def_col_px);
    let extent = used_extent(root, styles);
    if extent.last_row == 0 || extent.ncols == 0 {
        return Ok(SheetOut {
            html: sheet::error_body("This sheet is empty."),
            truncated: false,
        });
    }
    let nrows = extent.last_row.min(super::MAX_ROWS);
    let ncols = extent.ncols;
    let row_nodes = row_lookup(root, nrows);
    let mut merges = parse_merges(root, nrows, ncols);

    // ── geometry ────────────────────────────────────────────────────────────
    let rows = row_tracks(&row_nodes, nrows, settings.def_row_px);
    let layout = sheet::build_geometry(cols, rows, nrows, ncols);
    resolve_anchors(&mut merges, &layout.rows, &layout.cols);
    let frozen = Frozen::clamp(settings.frozen_rows, settings.frozen_cols, nrows, ncols);
    let frozen_css = sheet::frozen_pane_css(&layout, frozen);

    // ── emit ────────────────────────────────────────────────────────────────
    let mut w = Writer::new(ctx.html_cap);
    let mut classes = Classes::default();
    sheet::emit_head(&mut w, &layout, frozen, settings.show_lines, &mut classes);
    let mut src = SheetRows {
        row_nodes: &row_nodes,
        rows: &layout.rows,
        cols: &layout.cols,
        merges: &merges,
        styles,
        sst,
        date1904,
        notes: &mut *ctx.notes,
        // Almost never true, and the value-borrowing pass in `row` is skipped
        // entirely when it is not.
        displaced: merges.iter().any(|m| m.ar != m.r0 || m.ac != m.c0),
        r: 0,
        cells: vec![None; ncols],
    };
    sheet::emit_rows(
        &mut w,
        &layout,
        &merges,
        &mut src,
        frozen,
        &mut *ctx.marker,
        terms,
        &mut classes,
    );
    w.close(); // table

    // ── drawings ────────────────────────────────────────────────────────────
    if let Some(rid) = child(root, "drawing").and_then(|n| xml::attr_local(n, "id")) {
        let rid = rid.to_string();
        if !w.is_full() {
            if let Err(e) = drawings(ctx, &mut w, &part, &rid, &layout.grid) {
                if e == pkg::BUDGET_EXCEEDED {
                    ctx.notes.add(
                        "Some images omitted — size limit",
                    );
                } else {
                    ctx.notes.add("Some images could not be read");
                }
            }
        }
    }

    w.close(); // xl-grid
    w.close(); // xl-scroll
    w.close(); // xl-doc

    if extent.rows_clipped {
        ctx.notes
            .add(&format!("First {} rows only", super::MAX_ROWS));
    }
    if extent.cols_clipped {
        ctx.notes.add(&format!(
            "First {} columns only",
            super::MAX_COLS
        ));
    }

    let truncated = w.truncated() || extent.rows_clipped || extent.cols_clipped;
    Ok(SheetOut {
        html: emit::wrap_style(
            sheet::BASE_CSS,
            &sheet::collect_css(classes, &frozen_css, styles),
            w.finish(),
        ),
        truncated,
    })
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// Gridlines, the frozen split and the sheet's default track sizes.
///
/// SpreadsheetML-specific.
fn sheet_settings(root: roxmltree::Node<'_, '_>) -> Settings {
    let mut show_lines = true;
    let mut frozen_rows = 0usize;
    let mut frozen_cols = 0usize;
    if let Some(view) = child(root, "sheetViews").and_then(|v| child(v, "sheetView")) {
        if let Some(v) = attr_bool(view, "showGridLines") {
            show_lines = v;
        }
        if let Some(pane) = child(view, "pane") {
            let state = xml::attr_local(pane, "state").unwrap_or("split");
            if state == "frozen" || state == "frozenSplit" {
                frozen_cols = attr_u32(pane, "xSplit").unwrap_or(0) as usize;
                frozen_rows = attr_u32(pane, "ySplit").unwrap_or(0) as usize;
            }
        }
    }

    let fmt_pr = child(root, "sheetFormatPr");
    let def_col_px = fmt_pr
        .and_then(|n| attr_f32(n, "defaultColWidth"))
        .map(chars_to_px)
        .unwrap_or_else(|| chars_to_px(DEFAULT_COL_CHARS));
    let def_row_px = fmt_pr
        .and_then(|n| attr_f32(n, "defaultRowHeight"))
        .map(|pt| pt_to_px(pt).clamp(MIN_ROW_PX, MAX_ROW_PX).round())
        .unwrap_or_else(|| pt_to_px(DEFAULT_ROW_PT).round());

    Settings {
        show_lines,
        frozen_rows,
        frozen_cols,
        def_col_px,
        def_row_px,
    }
}

/// `<cols>` → per-column width, visibility and default xf, one entry per
/// possible column. SpreadsheetML-specific.
fn parse_cols(root: roxmltree::Node<'_, '_>, def_col_px: f32) -> Vec<Track> {
    let mut cols: Vec<Track> = (0..super::MAX_COLS).map(|_| Track::new(def_col_px)).collect();
    if let Some(list) = child(root, "cols") {
        for c in elems(list)
            .filter(|n| n.tag_name().name() == "col")
            .take(MAX_COL_DEFS)
        {
            // `min`/`max` are 1-based and inclusive; a single definition routinely
            // covers every column in the sheet.
            let min = attr_u32(c, "min").unwrap_or(1).max(1) as usize;
            let max = attr_u32(c, "max").unwrap_or(min as u32).max(1) as usize;
            if min > super::MAX_COLS {
                continue;
            }
            // `customWidth`/`customHeight` only record whether the author set the
            // measurement or Excel computed it; both are stored values and both are
            // honoured, so only presence matters here.
            let width = attr_f32(c, "width").map(chars_to_px);
            let hidden = attr_bool(c, "hidden");
            let style = attr_u32(c, "style");
            // One rule for all three: a definition changes what it states and leaves
            // the rest alone. Overlapping `<col>` runs are normal — a sheet-wide one
            // setting the width, a narrow one hiding a column — and resetting
            // `hidden`/`style` from an attribute the later definition never mentioned
            // silently unhid columns the sheet does hide.
            for i in min..=max.min(super::MAX_COLS) {
                let track = &mut cols[i - 1];
                if let Some(w) = width {
                    track.px = w;
                }
                if let Some(h) = hidden {
                    track.hidden = h;
                }
                if style.is_some() {
                    track.style = style;
                }
            }
        }
    }
    // Collapse every track to the whole pixel the table will actually lay out.
    // The `<col>` rule is rounded either way, so leaving the geometry fractional
    // makes the drawing overlay drift off the grid — the default width alone is
    // 64.01px, which puts a picture anchored at column 200 two pixels adrift.
    for track in cols.iter_mut() {
        track.px = track.px.clamp(MIN_COL_PX, MAX_COL_PX).round();
    }
    cols
}

/// The rectangle worth emitting, from `<sheetData>` and then from `<mergeCells>`.
///
/// SpreadsheetML-specific: it walks cell nodes. What it produces — a rectangle
/// plus two clipped flags — is not.
fn used_extent(root: roxmltree::Node<'_, '_>, styles: &super::styles::Styles) -> Extent {
    let mut extent = Extent {
        last_row: 0,
        ncols: 0,
        rows_clipped: false,
        cols_clipped: false,
    };
    if let Some(data) = child(root, "sheetData") {
        let mut implied_row: u32 = 0;
        for rn in elems(data).filter(|n| n.tag_name().name() == "row") {
            let r = attr_u32(rn, "r").unwrap_or(implied_row + 1);
            if r == 0 || r > super::MAX_ROW_NUMBER {
                continue;
            }
            implied_row = r;
            // The extent is taken from cells that *show* something, not from cells
            // that merely exist. Excel writes the used range out in full, so a
            // one-page form routinely carries a thousand rows of `<c s="4"/>` with
            // no value and an xf that paints nothing — 30k cells of nothing, which
            // dominates both the payload and every relayout the preview does.
            let mut inked = false;
            let mut implied_col: usize = 0;
            // Accumulated per row rather than straight into the extent: a row past
            // `MAX_ROWS` is never drawn, so nothing in it may widen the grid. Folding
            // it in as it was found emitted entirely empty columns for any sheet
            // whose one wide row sits below the row cap, and claimed columns were
            // clipped when none of the drawn ones were.
            let mut row_cols: usize = 0;
            let mut past_col_cap = false;
            for cn in elems(rn).filter(|n| n.tag_name().name() == "c") {
                let Some(c) = cell_col(cn, implied_col) else {
                    continue;
                };
                implied_col = c + 1;
                if !cell_inked(cn, styles) {
                    continue;
                }
                inked = true;
                if c >= super::MAX_COLS {
                    past_col_cap = true;
                    continue;
                }
                row_cols = row_cols.max(c + 1);
            }
            if !inked {
                continue;
            }
            if r > super::MAX_ROWS {
                extent.rows_clipped = true;
                continue;
            }
            extent.cols_clipped |= past_col_cap;
            extent.ncols = extent.ncols.max(row_cols);
            extent.last_row = extent.last_row.max(r);
        }
    }
    extend_by_merges(root, &mut extent);
    extent
}

/// A merge anchored inside the inked extent drags the rest of its range in with
/// it: half a merged title is worse than a few blank tracks. Runs before
/// `row_lookup`, which needs the final extent.
///
/// Bounded, because whole-column and whole-row merges are common (`A1:B1048576`
/// is how "merge across a column" is stored) and one of those would undo the
/// whole trim. A merge that reaches further than this is not a title block; it
/// gets clamped to the extent by `parse_merge` instead, as before.
fn extend_by_merges(root: roxmltree::Node<'_, '_>, extent: &mut Extent) {
    const MERGE_EXTEND_ROWS: u32 = 64;
    const MERGE_EXTEND_COLS: usize = 32;
    if extent.last_row == 0 || extent.ncols == 0 {
        return;
    }
    let Some(list) = child(root, "mergeCells") else {
        return;
    };
    for m in elems(list)
        .filter(|n| n.tag_name().name() == "mergeCell")
        .take(MAX_MERGES)
    {
        let Some((r0, c0, r1, c1)) = merge_bounds(xml::attr_local(m, "ref").unwrap_or("")) else {
            continue;
        };
        if r0 <= extent.last_row
            && r1 > extent.last_row
            && r1 - extent.last_row <= MERGE_EXTEND_ROWS
        {
            extent.rows_clipped |= r1 > super::MAX_ROWS;
            extent.last_row = r1.min(super::MAX_ROWS);
        }
        if c0 < extent.ncols && c1 >= extent.ncols && c1 + 1 - extent.ncols <= MERGE_EXTEND_COLS {
            extent.cols_clipped |= c1 >= super::MAX_COLS;
            extent.ncols = (c1 + 1).min(super::MAX_COLS);
        }
    }
}

/// Row node by row number, once the extent is settled. A blank row inside the
/// extent still needs its node — its height and `customFormat` apply either way.
///
/// SpreadsheetML-specific, and the only phase that hands XML nodes downstream —
/// to `SheetRows`, which is where they stop.
fn row_lookup<'a>(
    root: roxmltree::Node<'a, 'a>,
    last_row: u32,
) -> Vec<Option<roxmltree::Node<'a, 'a>>> {
    let mut row_nodes: Vec<Option<roxmltree::Node>> = vec![None; super::MAX_ROWS as usize + 1];
    if let Some(data) = child(root, "sheetData") {
        let mut implied_row: u32 = 0;
        for rn in elems(data).filter(|n| n.tag_name().name() == "row") {
            let r = attr_u32(rn, "r").unwrap_or(implied_row + 1);
            if r == 0 || r > super::MAX_ROW_NUMBER {
                continue;
            }
            implied_row = r;
            if r <= last_row {
                row_nodes[r as usize] = Some(rn);
            }
        }
    }
    row_nodes
}

/// Row heights, visibility and default xf, read off the `<row>` nodes.
///
/// Indexed by 1-based row number: index 0 is unused, and there is one slot past
/// the last row so the geometry pass can write the grid's closing edge without a
/// bounds check.
///
/// SpreadsheetML-specific (`ht`, `hidden`, `customFormat`); everything downstream
/// takes the measurements rather than the nodes, which is what makes
/// `build_geometry` reusable.
fn row_tracks(
    row_nodes: &[Option<roxmltree::Node<'_, '_>>],
    nrows: u32,
    def_row_px: f32,
) -> Vec<Track> {
    let mut rows = vec![Track::new(def_row_px); nrows as usize + 2];
    for r in 1..=nrows {
        let Some(rn) = row_nodes[r as usize] else {
            continue;
        };
        let track = &mut rows[r as usize];
        if let Some(ht) = attr_f32(rn, "ht") {
            track.px = pt_to_px(ht).clamp(MIN_ROW_PX, MAX_ROW_PX).round();
        }
        track.hidden = attr_bool(rn, "hidden").unwrap_or(false);
        // `customFormat` is what says the row's `s` applies to the row's cells;
        // without it the attribute records only the row's own formatting.
        if attr_bool(rn, "customFormat").unwrap_or(false) {
            track.style = attr_u32(rn, "s");
        }
    }
    rows
}

// ── cell values ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Num,
    Text,
    Bool,
    Err,
    Blank,
}

impl Kind {
    /// Excel's "General" alignment: it depends on the value, which is why it
    /// travels on the cell instead of in the per-`xf` rule.
    fn align(self) -> Option<Align> {
        match self {
            Kind::Num => Some(Align::Right),
            Kind::Bool | Kind::Err => Some(Align::Center),
            Kind::Text | Kind::Blank => None,
        }
    }
}

/// The SpreadsheetML side of the cell loop: the `<row>`/`<c>` nodes values are
/// read off, plus the workbook-wide inputs a displayed value depends on.
///
/// Bundled because `Ctx` cannot lend its fields one by one across a call boundary
/// while the rest of it stays borrowed.
struct SheetRows<'a, 'd> {
    row_nodes: &'a [Option<roxmltree::Node<'d, 'd>>],
    rows: &'a [Track],
    cols: &'a [Track],
    merges: &'a [Merge],
    styles: &'a super::styles::Styles,
    sst: &'a [String],
    date1904: bool,
    notes: &'a mut Notes,
    /// Whether any merge's span moved off its stored top-left cell.
    displaced: bool,
    /// The row `row` last positioned on.
    r: u32,
    /// That row's `<c>` nodes by column. Reused so a wide sheet does not
    /// reallocate it every row.
    cells: Vec<Option<roxmltree::Node<'d, 'd>>>,
}

impl CellSource for SheetRows<'_, '_> {
    fn row(&mut self, r: u32) {
        self.r = r;
        self.cells.iter_mut().for_each(|v| *v = None);
        if let Some(rn) = self.row_nodes[r as usize] {
            let mut implied_col = 0usize;
            for cn in elems(rn).filter(|n| n.tag_name().name() == "c") {
                let Some(c) = cell_col(cn, implied_col) else {
                    continue;
                };
                implied_col = c + 1;
                if c < self.cells.len() {
                    self.cells[c] = Some(cn);
                }
            }
        }

        // A merged range stores its value in the top-left cell only. When that cell
        // is in a hidden row or column the span is emitted somewhere else, so the
        // visible anchor borrows the value — otherwise a merged title above a hidden
        // row, or spanning a hidden helper column, renders blank.
        if self.displaced {
            for m in self.merges {
                if r != m.ar || (m.ar == m.r0 && m.ac == m.c0) || self.cells[m.ac].is_some() {
                    continue;
                }
                let Some(anchor) = self.row_nodes[m.r0 as usize] else {
                    continue;
                };
                let mut implied = 0usize;
                for cn in elems(anchor).filter(|n| n.tag_name().name() == "c") {
                    let Some(c) = cell_col(cn, implied) else {
                        continue;
                    };
                    implied = c + 1;
                    if c == m.c0 {
                        self.cells[m.ac] = Some(cn);
                        break;
                    }
                }
            }
        }
    }

    fn cell(&mut self, c: usize) -> Cell {
        let styles = self.styles;
        let node = self.cells[c];
        // A cell states its own xf, or falls back to its row's and then its
        // column's default.
        let style_id = node
            .and_then(|n| attr_u32(n, "s"))
            .or(self.rows[self.r as usize].style)
            .or(self.cols[c].style)
            .unwrap_or(0);
        let cs = styles.get(style_id);
        let (text, kind) = match node {
            Some(n) => cell_text(n, &cs.fmt, self.sst, self.date1904, self.notes),
            None => (String::new(), Kind::Blank),
        };
        let color = node
            .filter(|_| kind == Kind::Num)
            .and_then(numeric_value)
            .and_then(|v| cs.fmt.color(v))
            .map(|c| c.to_string());
        Cell {
            text,
            align: kind.align(),
            style: styles.has_css(style_id).then_some(style_id),
            inner: styles.has_inner(style_id),
            color,
        }
    }
}

/// The cell's displayed string. `t` selects the storage, and for the numeric case
/// the number format is what turns a serial back into a date — `Format::apply_with`
/// performs that conversion internally for a date-kind code, which is why a bare
/// `45678` never reaches the output.
pub(super) fn cell_text(
    cell: roxmltree::Node<'_, '_>,
    fmt: &Format,
    sst: &[String],
    date1904: bool,
    notes: &mut Notes,
) -> (String, Kind) {
    let t = xml::attr_local(cell, "t").unwrap_or("n");
    match t {
        "s" => {
            let idx = child(cell, "v")
                .and_then(text_of)
                .and_then(|s| s.trim().parse::<usize>().ok());
            match idx.and_then(|i| sst.get(i)) {
                Some(s) => (clip(fmt.apply_text(s)), Kind::Text),
                None => {
                    notes.add(
                        "Some cell text missing",
                    );
                    (String::new(), Kind::Blank)
                }
            }
        }
        // An inline string is a full rich-text body, so its runs are joined the
        // same way a shared string's are.
        "inlineStr" => match child(cell, "is") {
            Some(is) => (clip(fmt.apply_text(&rich_text(is))), Kind::Text),
            None => (String::new(), Kind::Blank),
        },
        // A formula's cached string result.
        "str" => match child(cell, "v").and_then(text_of) {
            Some(s) => (clip(fmt.apply_text(s)), Kind::Text),
            None => (String::new(), Kind::Blank),
        },
        "b" => match child(cell, "v").and_then(text_of) {
            Some(v) => (
                if v.trim() == "0" { "FALSE" } else { "TRUE" }.to_string(),
                Kind::Bool,
            ),
            None => (String::new(), Kind::Blank),
        },
        "e" => match child(cell, "v").and_then(text_of) {
            Some(v) => (clip(v.trim().to_string()), Kind::Err),
            None => (String::new(), Kind::Blank),
        },
        // ISO 8601 in the cell itself (`t="d"`, rare outside Office 365). The text
        // is already human-readable, so it is passed through rather than re-parsed
        // into a serial just to format it again.
        "d" => match child(cell, "v").and_then(text_of) {
            Some(v) => (clip(v.trim().to_string()), Kind::Text),
            None => (String::new(), Kind::Blank),
        },
        _ => match child(cell, "v").and_then(text_of) {
            Some(v) => match v.trim().parse::<f64>() {
                Ok(n) if n.is_finite() => (clip(fmt.apply_with(n, date1904)), Kind::Num),
                // A non-numeric `<v>` in a numeric cell is malformed; showing the
                // raw text beats showing nothing.
                _ => (clip(v.trim().to_string()), Kind::Text),
            },
            None => (String::new(), Kind::Blank),
        },
    }
}

fn numeric_value(cell: roxmltree::Node<'_, '_>) -> Option<f64> {
    child(cell, "v")?
        .text()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Concatenates the `<t>` of a rich-text body, skipping the phonetic subtrees.
/// A run-formatted cell arrives as several `<r><t>` fragments — reading only the
/// first would silently drop most of the text — while `<rPh>` holds furigana that
/// is a *reading aid* for the same characters and would duplicate them.
pub(super) fn rich_text(node: roxmltree::Node<'_, '_>) -> String {
    let mut out = String::new();
    collect_text(node, &mut out);
    out
}

fn collect_text(node: roxmltree::Node<'_, '_>, out: &mut String) {
    for ch in node.children() {
        if !ch.is_element() {
            continue;
        }
        match ch.tag_name().name() {
            "rPh" | "phoneticPr" => continue,
            "t" => xml::inner_text(ch, out),
            _ => collect_text(ch, out),
        }
    }
}

fn clip(mut s: String) -> String {
    if s.chars().count() <= MAX_CELL_CHARS {
        return s;
    }
    let cut = s
        .char_indices()
        .nth(MAX_CELL_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s.truncate(cut);
    s.push('…');
    s
}

// ── merges ───────────────────────────────────────────────────────────────────

/// `<mergeCells>` → the spans of the emitted grid. SpreadsheetML-specific; what
/// it produces is a plain list of rectangles.
fn parse_merges(root: roxmltree::Node<'_, '_>, nrows: u32, ncols: usize) -> Vec<Merge> {
    let mut merges: Vec<Merge> = Vec::new();
    if let Some(list) = child(root, "mergeCells") {
        for m in elems(list)
            .filter(|n| n.tag_name().name() == "mergeCell")
            .take(MAX_MERGES)
        {
            if let Some(m) = parse_merge(xml::attr_local(m, "ref").unwrap_or(""), nrows, ncols) {
                merges.push(m);
            }
        }
    }
    merges
}

/// A merge's normalized `(r0, c0, r1, c1)`, unclamped — rows 1-based, columns
/// 0-based. Used before the sheet's extent is known, to let a merge extend it.
fn merge_bounds(r: &str) -> Option<(u32, usize, u32, usize)> {
    let (a, b) = r.split_once(':')?;
    let (r0, c0) = parse_ref(a)?;
    let (r1, c1) = parse_ref(b)?;
    Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
}

/// `A1:C3` → a merge clamped to the emitted grid. A range that runs past the
/// emitted bounds (a merge down a whole column) is clipped rather than dropped, so
/// its visible part still spans.
fn parse_merge(r: &str, nrows: u32, ncols: usize) -> Option<Merge> {
    let (r0, c0, r1, c1) = merge_bounds(r)?;
    if r0 > nrows || c0 >= ncols {
        return None;
    }
    Some(Merge {
        r0,
        r1: r1.min(nrows),
        c0,
        c1: c1.min(ncols.saturating_sub(1)),
        // Provisional; `resolve_anchors` corrects these once visibility is known.
        ar: r0,
        ac: c0,
    })
}

fn parse_ref(s: &str) -> Option<(u32, usize)> {
    let s = s.trim().trim_start_matches('$');
    let (letters, digits) = split_cell_ref(s);
    let col = col_letter_to_index(letters)?;
    let row: u32 = digits.trim_start_matches('$').parse().ok()?;
    (row > 0).then_some((row, col))
}

/// Whether a cell contributes to the sheet's visible extent.
///
/// A cell counts when it carries a value (`<v>`, `<is>`, a formula) or when its
/// xf paints something an empty cell would still show — a fill or a border. It
/// does *not* count for a font, an alignment or a number format: those are
/// invisible without text, and Excel stamps them across the whole used range.
fn cell_inked(cell: roxmltree::Node<'_, '_>, styles: &super::styles::Styles) -> bool {
    for n in elems(cell) {
        match n.tag_name().name() {
            "v" | "f" => return true,
            // An `<is>` holding only empty runs is still an empty cell.
            "is" => {
                if n.descendants().any(|d| {
                    d.text()
                        .map(|t| !t.trim().is_empty())
                        .unwrap_or(false)
                }) {
                    return true;
                }
            }
            _ => {}
        }
    }
    attr_u32(cell, "s").is_some_and(|s| styles.paints(s))
}

/// A cell's 0-based column from its `r` attribute, falling back to the position
/// after the previous cell (the `r` attribute is optional).
fn cell_col(cell: roxmltree::Node<'_, '_>, implied: usize) -> Option<usize> {
    match xml::attr_local(cell, "r") {
        Some(r) => col_letter_to_index(split_cell_ref(r).0),
        None => Some(implied),
    }
}

// ── drawings ─────────────────────────────────────────────────────────────────

/// Images and charts anchored over the grid.
///
/// Positions are computed from the *emitted* column/row offsets, so a sheet with
/// hidden rows or columns places its pictures where the visible grid puts those
/// cells rather than where Excel would — the alternative is a picture floating
/// over unrelated data.
fn drawings(
    ctx: &mut Ctx,
    w: &mut Writer,
    sheet_part: &str,
    rid: &str,
    grid: &Grid,
) -> Result<(), String> {
    // Strict: a `.rels` this renderer cannot parse means every drawing on the
    // sheet is gone, and the caller turns the error into a note saying so. The
    // lenient read would drop them without a word.
    let rels = opc::read_rels_strict(ctx.zip, sheet_part, ctx.budget)?;
    let Some(rel) = rels.get(rid) else {
        return Ok(());
    };
    if rel.external {
        return Ok(());
    }
    let Some(dpart) = opc::resolve_target(sheet_part, &rel.target) else {
        return Ok(());
    };
    let Some(dxml) = pkg::read_entry(ctx.zip, &dpart, ctx.budget)? else {
        return Ok(());
    };
    let ddoc = xml::parse(&dxml)?;
    let drels = opc::read_rels(ctx.zip, &dpart, ctx.budget)?;

    let mut opened = false;
    let mut unsupported = false;
    for anchor in elems(ddoc.root_element()).take(MAX_DRAWINGS) {
        if w.is_full() {
            break;
        }
        let Some(xf) = anchor_box(anchor, grid) else {
            continue;
        };
        if xf.cx < 1.0 || xf.cy < 1.0 {
            continue;
        }
        // `mc:AlternateContent` usually pairs a metafile with a raster fallback;
        // the fallback is the branch a consumer of no extension namespaces must
        // take, and it is the one that can actually be displayed.
        let scope = elems(anchor)
            .find(|n| n.tag_name().name() == "AlternateContent")
            .and_then(media::prefer_raster_branch)
            .unwrap_or(anchor);

        let embed = descendant(scope, "blip").and_then(|b| xml::attr_local(b, "embed"));
        // No `graphicData` at all: not a frame this renderer can even label.
        let label = descendant(scope, "graphicData")
            .and_then(|g| xml::attr_local(g, "uri"))
            .map(emit::graphic_label);
        if embed.is_none() && label.is_none() {
            unsupported = true;
            continue;
        }
        if !opened {
            w.open("div", &attr("class", "xl-drawings"));
            opened = true;
        }
        let style = xf.css();
        if let Some(id) = embed {
            let target = drels
                .get(id)
                .filter(|r| !r.external)
                .and_then(|r| opc::resolve_target(&dpart, &r.target));
            let media = match target {
                Some(part) => {
                    ctx.media
                        .get(ctx.zip, ctx.budget, ctx.mb, &part, xf.cx.round() as u32)
                }
                None => Media::Placeholder("image unavailable"),
            };
            match media {
                Media::DataUri(uri) => {
                    let alt = descendant(scope, "cNvPr")
                        .and_then(|p| {
                            xml::attr_local(p, "descr").or_else(|| xml::attr_local(p, "name"))
                        })
                        .unwrap_or("");
                    w.void(
                        "img",
                        &attrs(&[
                            &attr("class", "xl-dw"),
                            &attr("style", &style),
                            &attr("src", &uri),
                            &attr("alt", alt),
                        ]),
                    );
                }
                Media::Placeholder(reason) => emit::placeholder(w, "xl-ph", &style, reason),
            }
        } else if let Some(l) = label {
            emit::placeholder(w, "xl-ph", &style, l);
        }
    }
    if opened {
        w.close();
    }
    if unsupported {
        ctx.notes
            .add("Some shapes not shown");
    }
    Ok(())
}

/// Pixel box of a drawing anchor. All three anchor kinds are handled: two-cell
/// (a from/to cell pair), one-cell (a from cell plus an extent) and absolute (a
/// slide-style position plus an extent).
fn anchor_box(anchor: roxmltree::Node<'_, '_>, grid: &Grid) -> Option<Xf> {
    let kind = anchor.tag_name().name();
    let ext = child(anchor, "ext");
    let (x, y, cx, cy) = match kind {
        "twoCellAnchor" => {
            let (x0, y0) = cell_point(child(anchor, "from")?, grid)?;
            let (x1, y1) = cell_point(child(anchor, "to")?, grid)?;
            (x0, y0, x1 - x0, y1 - y0)
        }
        "oneCellAnchor" => {
            let (x0, y0) = cell_point(child(anchor, "from")?, grid)?;
            let e = ext?;
            (
                x0,
                y0,
                emu_to_px(attr_i64(e, "cx")?),
                emu_to_px(attr_i64(e, "cy")?),
            )
        }
        "absoluteAnchor" => {
            let pos = child(anchor, "pos")?;
            let e = ext?;
            (
                emu_to_px(attr_i64(pos, "x")?),
                emu_to_px(attr_i64(pos, "y")?),
                emu_to_px(attr_i64(e, "cx")?),
                emu_to_px(attr_i64(e, "cy")?),
            )
        }
        _ => return None,
    };
    if ![x, y, cx, cy].iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(Xf {
        x: x as f64,
        y: y as f64,
        cx: cx as f64,
        cy: cy as f64,
        rot: 0.0,
        flip_h: false,
        flip_v: false,
    })
}

/// `<xdr:from>` / `<xdr:to>`: a 0-based cell index plus an EMU offset inside it.
fn cell_point(node: roxmltree::Node<'_, '_>, grid: &Grid) -> Option<(f32, f32)> {
    let col = child(node, "col").and_then(text_of)?.trim().parse::<u32>().ok()? as usize;
    let row = child(node, "row").and_then(text_of)?.trim().parse::<u32>().ok()?;
    let col_off = child(node, "colOff")
        .and_then(text_of)
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let row_off = child(node, "rowOff")
        .and_then(text_of)
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Some((
        grid.x(col) + emu_to_px(col_off),
        grid.y(row.saturating_add(1)) + emu_to_px(row_off),
    ))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Stored column width (in default-font digits) → px.
fn chars_to_px(chars: f32) -> f32 {
    if !chars.is_finite() {
        return chars_to_px(DEFAULT_COL_CHARS);
    }
    (chars * MDW_PX + COL_PAD_PX).clamp(0.0, MAX_COL_PX)
}

#[cfg(test)]
mod tests {
    use super::super::super::drawingml::theme::Theme;
    use super::super::styles::Styles;
    use super::*;

    /// The pipeline phases take a `<worksheet>` root, so a fixture is a document
    /// the caller keeps alive — the nodes borrow it.
    fn ws(body: &str) -> String {
        format!("<worksheet>{body}</worksheet>")
    }

    /// One default column is 8.43 digits wide, i.e. 64px after rounding.
    const DEF_COL: f32 = 64.0;

    // ── extent ──────────────────────────────────────────────────────────────

    #[test]
    fn used_extent_counts_cells_that_show_something() {
        let xml = ws("<sheetData>\
             <row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"C1\"><v>2</v></c></row>\
             <row r=\"2\"><c r=\"A2\" s=\"4\"/><c r=\"Z2\" s=\"4\"/></row>\
             </sheetData>");
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &Styles::empty());
        // Row 2 is styled but blank and its xf paints nothing, so neither its row
        // nor its column Z reaches the extent.
        assert_eq!((ext.last_row, ext.ncols), (1, 3));
        assert!(!ext.rows_clipped && !ext.cols_clipped);
    }

    #[test]
    fn a_painting_xf_inks_an_otherwise_empty_cell() {
        let styles = Styles::parse(
            "<styleSheet><fills count=\"1\">\
               <fill><patternFill patternType=\"solid\"><fgColor rgb=\"FFFFC000\"/></patternFill></fill>\
             </fills><cellXfs count=\"2\"><xf/><xf fillId=\"0\" applyFill=\"1\"/></cellXfs></styleSheet>",
            &Theme::default(),
        )
        .unwrap();
        let xml = ws("<sheetData><row r=\"3\"><c r=\"B3\" s=\"1\"/></row></sheetData>");
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &styles);
        assert_eq!((ext.last_row, ext.ncols), (3, 2));
    }

    #[test]
    fn a_merge_reaching_just_past_the_inked_extent_pulls_it_along() {
        let xml = ws("<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c></row></sheetData>\
             <mergeCells>\
               <mergeCell ref=\"A1:C6\"/>\
               <mergeCell ref=\"A1:A1048576\"/>\
               <mergeCell ref=\"E10:F12\"/>\
             </mergeCells>");
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &Styles::empty());
        // `A1:C6` is a title block and drags the extent out to it. The
        // whole-column merge reaches too far and the detached one starts outside
        // the extent, so neither counts — otherwise the trim would be undone.
        assert_eq!((ext.last_row, ext.ncols), (6, 3));
    }

    #[test]
    fn a_merge_cannot_extend_an_extent_that_does_not_exist() {
        let xml = ws("<sheetData/><mergeCells><mergeCell ref=\"A1:C6\"/></mergeCells>");
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &Styles::empty());
        assert_eq!((ext.last_row, ext.ncols), (0, 0));
    }

    #[test]
    fn an_extent_past_the_emission_caps_is_clipped_and_flagged() {
        let over_row = super::super::MAX_ROWS + 1;
        let over_col = super::super::super::sheet::col_letter(super::super::MAX_COLS);
        let xml = ws(&format!(
            "<sheetData>\
               <row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"{over_col}1\"><v>2</v></c></row>\
               <row r=\"{over_row}\"><c r=\"A{over_row}\"><v>3</v></c></row>\
             </sheetData>"
        ));
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &Styles::empty());
        assert_eq!(ext.last_row, 1);
        assert_eq!(ext.ncols, 1);
        assert!(ext.rows_clipped && ext.cols_clipped);
    }

    #[test]
    fn a_clipped_row_cannot_widen_the_column_extent() {
        // The bug: `ncols` was folded in before the row cap was tested, so a sheet
        // whose one wide row sits past row 200 emitted 200 columns of nothing — and
        // said "First 200 columns only" about columns that were never dropped.
        let over_row = super::super::MAX_ROWS + 1;
        let wide = super::super::super::sheet::col_letter(50);
        let xml = ws(&format!(
            "<sheetData>\
               <row r=\"1\"><c r=\"A1\"><v>1</v></c></row>\
               <row r=\"{over_row}\"><c r=\"A{over_row}\"><v>2</v></c>\
                 <c r=\"{wide}{over_row}\"><v>3</v></c></row>\
             </sheetData>"
        ));
        let doc = xml::parse(&xml).unwrap();
        let ext = used_extent(doc.root_element(), &Styles::empty());
        assert_eq!((ext.last_row, ext.ncols), (1, 1));
        assert!(ext.rows_clipped, "the row itself is clipped");
        assert!(!ext.cols_clipped, "no drawn column was dropped");
    }

    // ── columns ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_cols_spreads_one_definition_over_its_whole_range() {
        let xml = ws("<cols>\
             <col min=\"1\" max=\"2\" width=\"20\" customWidth=\"1\"/>\
             <col min=\"3\" max=\"3\" hidden=\"1\"/>\
             <col min=\"4\" max=\"4\" style=\"7\"/>\
             </cols>");
        let doc = xml::parse(&xml).unwrap();
        let cols = parse_cols(doc.root_element(), chars_to_px(DEFAULT_COL_CHARS));
        assert_eq!(cols.len(), super::super::MAX_COLS);
        // 20 digits * 7px + 5px padding.
        assert_eq!(cols[0].px, 145.0);
        assert_eq!(cols[1].px, 145.0);
        assert!(cols[2].hidden);
        // A hidden column keeps its width; it is the geometry pass that drops it.
        assert_eq!(cols[2].px, DEF_COL);
        assert_eq!(cols[3].style, Some(7));
        // Untouched columns take the sheet default, rounded to a whole pixel.
        assert_eq!(cols[4].px, DEF_COL);
        assert!(!cols[4].hidden && cols[4].style.is_none());
    }

    #[test]
    fn an_overlapping_col_definition_changes_only_what_it_states() {
        // The asymmetry this fixes: an omitted `width` was preserved but an omitted
        // `hidden`/`style` was reset, so the sheet-wide definition that normally
        // follows a narrow one silently unhid a hidden column.
        let xml = ws("<cols>\
             <col min=\"1\" max=\"1\" hidden=\"1\" style=\"7\"/>\
             <col min=\"1\" max=\"3\" width=\"20\"/>\
             </cols>");
        let doc = xml::parse(&xml).unwrap();
        let cols = parse_cols(doc.root_element(), chars_to_px(DEFAULT_COL_CHARS));
        assert_eq!(cols[0].px, 145.0, "the later width applies");
        assert!(cols[0].hidden, "and does not unhide the column");
        assert_eq!(cols[0].style, Some(7), "nor drop its default xf");
        // A column the first definition never covered keeps its own defaults.
        assert!(!cols[1].hidden && cols[1].style.is_none());
    }

    #[test]
    fn parse_cols_keeps_a_sliver_and_ignores_out_of_range_definitions() {
        let xml = ws(&format!(
            "<cols><col min=\"1\" max=\"1\" width=\"-1\"/>\
               <col min=\"{0}\" max=\"{0}\" width=\"30\"/></cols>",
            super::super::MAX_COLS + 1
        ));
        let doc = xml::parse(&xml).unwrap();
        let cols = parse_cols(doc.root_element(), chars_to_px(DEFAULT_COL_CHARS));
        assert_eq!(cols[0].px, MIN_COL_PX);
        assert_eq!(cols.len(), super::super::MAX_COLS);
        assert_eq!(cols[super::super::MAX_COLS - 1].px, DEF_COL);
    }

    #[test]
    fn a_sheet_default_column_width_applies_to_every_column() {
        let xml = ws("<sheetFormatPr defaultColWidth=\"5\" defaultRowHeight=\"30\"/>");
        let doc = xml::parse(&xml).unwrap();
        let settings = sheet_settings(doc.root_element());
        let cols = parse_cols(doc.root_element(), settings.def_col_px);
        assert_eq!(cols[0].px, 40.0);
        // 30pt at 96dpi.
        assert_eq!(settings.def_row_px, 40.0);
    }

    #[test]
    fn only_a_frozen_pane_state_freezes() {
        for (state, want) in [("frozen", (1, 2)), ("frozenSplit", (1, 2)), ("split", (0, 0))] {
            let xml = ws(&format!(
                "<sheetViews><sheetView showGridLines=\"0\">\
                   <pane xSplit=\"2\" ySplit=\"1\" state=\"{state}\"/></sheetView></sheetViews>"
            ));
            let doc = xml::parse(&xml).unwrap();
            let s = sheet_settings(doc.root_element());
            assert_eq!((s.frozen_rows, s.frozen_cols), want, "state={state}");
            assert!(!s.show_lines);
        }
    }

    #[test]
    fn a_zero_height_row_keeps_a_sliver() {
        // The row equivalent of MIN_COL_PX: `ht="0"` emitted a genuinely 0px <tr>,
        // and a zero-size track is what MIN_COL_PX exists to prevent — the gutter
        // and the grid disagree about where the row edge is.
        let xml = ws("<sheetData><row r=\"1\" ht=\"0\" customHeight=\"1\"/>\
             <row r=\"2\" ht=\"-4\"/></sheetData>");
        let doc = xml::parse(&xml).unwrap();
        let nodes = row_lookup(doc.root_element(), 2);
        let rows = row_tracks(&nodes, 2, pt_to_px(DEFAULT_ROW_PT).round());
        assert_eq!(rows[1].px, MIN_ROW_PX);
        assert_eq!(rows[2].px, MIN_ROW_PX);
        // A row the sheet really hides says so, and that is what drops the track.
        assert!(!rows[1].hidden);
    }
}
