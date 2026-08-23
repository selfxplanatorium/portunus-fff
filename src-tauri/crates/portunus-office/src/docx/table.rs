//! `w:tbl` → an HTML `<table>`: the grid geometry, the vertical-merge pre-pass,
//! and the per-cell property cascade.
//!
//! A cell holds *content* — paragraphs, nested tables, content controls — so the
//! block walk is re-entered for a cell's children instead of a resolved string
//! being handed over. That is the one thing which keeps this from being a variant
//! of the sheet renderer, where every cell is one line of already-resolved text
//! before anything is emitted.
//!
//! Two things have to be known before a byte is written, which is why the table is
//! planned whole first: a `w:vMerge` continuation only makes sense in terms of the
//! rows above it, and the row count decides which cells are on the table's bottom
//! edge.
//!
//! Nothing here measures text, so the columns come from the document's own
//! `w:tblGrid` under `table-layout:fixed`. Word's autofit — which *is* measured,
//! against the content and the window — is not reproducible, and a grid the author
//! saved is closer to what they saw than an equal split would be.
//!
//! The structural CSS (`border-collapse`, the default cell padding and vertical
//! alignment) lives in [`super::BASE_CSS`] under `.of-tbl` / `.of-tc`, so the
//! common cell carries no `style` attribute at all: a table in a forty-page report
//! is thousands of cells, and every redundant declaration is bytes against
//! [`super::HTML_CAP`].

use super::super::cellstyle::{align_css, AlignSpec};
use super::super::drawingml::color::Color;
use super::super::drawingml::theme::Theme;
use super::super::html::{attr, attrs, dxa_to_px, fmt_pct, fmt_px, Style, Writer};
use super::super::model::Align;
use super::super::xml::{self, child, elems};
use super::style::{
    self, BorderSides, BorderSpec, CellMargins, CellProps, ColorVal, HRule, RowProps, TableBorders,
    TableProps, VMerge, Width,
};
use super::{body, Ctx};
use roxmltree::Node;

/// Tables that may enclose one another. A layout table inside a layout table
/// happens; three deep is already past every real use, and a generated document
/// can nest without bound.
const MAX_NEST: usize = 3;

/// Rows per table.
const MAX_ROWS: usize = 1_000;

/// Cells per table, and cells across the whole document. Both are needed: one
/// table of a million cells and a thousand tables of a thousand cells each cost
/// the same, and only the second gets past a per-table cap.
const MAX_CELLS: usize = 8_000;
const MAX_DOC_CELLS: usize = 40_000;

/// Grid columns, shared with the `w:gridSpan` clamp so the grid, the spans and the
/// `<colgroup>` cannot disagree about how wide the table is.
const MAX_COLS: usize = style::MAX_GRID_COLS as usize;

/// Nesting of the wrappers that hold a row or a cell without being one —
/// `w:sdt`, `w:ins`, `w:customXml`.
const MAX_WRAP: usize = 8;

/// Sane geometry bounds. A column or row larger than these is corrupt: the page
/// itself is capped at 4096px, and a table cannot usefully exceed it.
const MAX_COL_PX: f32 = 2_000.0;
const MAX_ROW_PX: f32 = 2_000.0;
const MAX_TABLE_PX: f32 = 4_096.0;

/// Word's own default cell margins, twentieths of a point: 0.08in on the leading
/// and trailing edge, nothing vertical. [`super::BASE_CSS`] states them on
/// `.of-tc`, so a cell whose resolved margins are these needs no declaration —
/// which is the overwhelmingly common case.
const DEFAULT_MAR_DXA: [i64; 4] = [0, 108, 0, 108];

/// A table stopped early: the row cap and the cell caps both end the same way, and
/// what the reader needs to know is that this is not all of it.
const NOTE_CLIPPED: &str = "Large table cut short";

/// The document-wide cell budget, which stops a whole table rather than trimming
/// one — so it needs its own wording.
const NOTE_DOC_CELLS: &str = "Later tables not shown";

// ── plan ─────────────────────────────────────────────────────────────────────

/// One cell: where it sits in the grid and how far it reaches.
struct Cell<'d> {
    node: Node<'d, 'd>,
    /// The cell's own `w:tcPr`, parsed once here rather than again at emission.
    props: CellProps,
    /// 0-based grid column of the leading edge.
    col: usize,
    /// `w:gridSpan`, at least 1.
    span: usize,
    /// Rows this cell covers. **`0` marks a `w:vMerge` continuation**, which emits
    /// no element at all — the opener above it already covers this row.
    rowspan: usize,
}

struct Row<'d> {
    /// The row's own `w:trPr`, parsed once here rather than again at emission.
    props: RowProps,
    cells: Vec<Cell<'d>>,
}

struct Plan<'d> {
    rows: Vec<Row<'d>>,
    /// Column widths in px from `w:tblGrid`; empty when the document states no
    /// usable grid.
    grid: Vec<f32>,
    /// Grid columns the table actually spans — the wider of the stated grid and
    /// the widest row, because a row may reach past the grid.
    ncols: usize,
    /// `w:tc` elements planned, for the document-wide budget.
    cells: usize,
    clipped: bool,
    clipped_cols: bool,
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Emits one `w:tbl`. `depth` is how many tables already enclose it, so the
/// outermost table in a body is depth 0.
pub fn emit_table(ctx: &mut Ctx, w: &mut Writer, tbl: Node, depth: usize) {
    if depth >= MAX_NEST {
        ctx.notes.add(&format!(
            "Deeply nested table not shown"
        ));
        return;
    }
    let budget = MAX_DOC_CELLS.saturating_sub(ctx.cells);
    if budget == 0 {
        ctx.notes.add(NOTE_DOC_CELLS);
        return;
    }

    // The style chain first, the table's own `w:tblPr` over it. A "Grid Table"
    // style is where most real tables keep every border they have.
    let direct = child(tbl, "tblPr")
        .map(|n| style::parse_table_props(n, ctx.theme))
        .unwrap_or_default();
    let from_style = direct
        .style
        .as_deref()
        .map(|id| ctx.styles.resolve_table(id))
        .unwrap_or_default();
    let mut tp = from_style.table.clone();
    style::merge_table(&mut tp, &direct);

    let plan = plan(tbl, ctx.theme, MAX_CELLS.min(budget));
    if plan.rows.is_empty() {
        // A table with no row draws nothing at all, and an empty `<table>` would
        // still take the stylesheet's borders.
        return;
    }
    ctx.cells += plan.cells;
    if plan.clipped {
        ctx.notes.add(NOTE_CLIPPED);
    }
    if plan.clipped_cols {
        ctx.notes.add(&format!(
            "Wide table cut short"
        ));
    }

    let class = if plan.grid.is_empty() {
        // Nothing to lay out against: a fixed layout with no column widths splits
        // the table equally, which is a geometry no document asked for.
        "of-tbl of-tbl-auto"
    } else {
        "of-tbl"
    };
    w.open(
        "table",
        &attrs(&[&attr("class", class), &table_css(&tp, &plan.grid).to_attr()]),
    );
    if !plan.grid.is_empty() {
        w.open("colgroup", "");
        for px in &plan.grid {
            let mut s = Style::new();
            s.push_opt("width", fmt_px(*px));
            w.void("col", &s.to_attr());
        }
        w.close();
    }

    let frame = Frame {
        borders: &tp.borders,
        mar: tp.cell_mar,
        base: &from_style.cell,
        shade: tp.shade.and_then(ColorVal::color),
        ncols: plan.ncols,
        nrows: plan.rows.len(),
        no_grid: plan.grid.is_empty(),
    };
    for (r, row) in plan.rows.iter().enumerate() {
        if w.is_full() {
            break;
        }
        let mut rp = from_style.row.clone();
        style::merge_row(&mut rp, &row.props);
        w.open("tr", &row_css(&rp).to_attr());
        for cell in &row.cells {
            // A continuation emits nothing: the opener above it carries the
            // `rowspan` that covers this row.
            if cell.rowspan == 0 {
                continue;
            }
            emit_cell(ctx, w, cell, &frame, r, depth);
        }
        w.close();
    }
    w.close();
}

/// Reads the whole table before anything is emitted.
fn plan<'d>(tbl: Node<'d, 'd>, theme: &Theme, limit: usize) -> Plan<'d> {
    let mut p = Plan {
        rows: Vec::new(),
        grid: grid_px(tbl),
        ncols: 0,
        cells: 0,
        clipped: false,
        clipped_cols: false,
    };
    p.ncols = p.grid.len();

    let mut row_nodes: Vec<Node> = Vec::new();
    collect(tbl, "tr", &mut row_nodes, 0, MAX_ROWS + 1);
    if row_nodes.len() > MAX_ROWS {
        p.clipped = true;
        row_nodes.truncate(MAX_ROWS);
    }

    // `open[c]` locates the vertical span currently covering grid column `c`, as
    // an index into `p.rows` and into that row's cells.
    let mut open: Vec<Option<(usize, usize)>> = vec![None; MAX_COLS];
    let mut budget = limit;

    for rn in row_nodes {
        if budget == 0 {
            p.clipped = true;
            break;
        }
        let props = child(rn, "trPr")
            .map(style::parse_row_props)
            .unwrap_or_default();
        // A tracked deletion of the row itself: the row is gone, not merely its
        // text, which is the same rule the run walk follows for `w:del`. A row
        // wrapped *in* a `w:del` never reaches here — `collect` refuses to descend
        // into one.
        if props.del {
            continue;
        }

        let mut tc_nodes: Vec<Node> = Vec::new();
        collect(rn, "tc", &mut tc_nodes, 0, budget + 1);
        if tc_nodes.len() > budget {
            p.clipped = true;
            tc_nodes.truncate(budget);
        }
        budget -= tc_nodes.len();
        p.cells += tc_nodes.len();

        let mut cells: Vec<Cell> = Vec::new();
        let mut col = 0usize;
        for tc in tc_nodes {
            if col >= MAX_COLS {
                p.clipped_cols = true;
                break;
            }
            let props = child(tc, "tcPr")
                .map(|n| style::parse_cell_props(n, theme))
                .unwrap_or_default();
            let span = props
                .grid_span
                .unwrap_or(1)
                .clamp(1, MAX_COLS as i64) as usize;
            let span = span.min(MAX_COLS - col);
            cells.push(Cell {
                node: tc,
                props,
                col,
                span,
                rowspan: 1,
            });
            col += span;
        }
        p.ncols = p.ncols.max(col);

        let r = p.rows.len();
        for (i, c) in cells.iter_mut().enumerate() {
            if c.props.v_merge == Some(VMerge::Continue) {
                if let Some((rr, ci)) = open[c.col] {
                    p.rows[rr].cells[ci].rowspan += 1;
                    c.rowspan = 0;
                }
                // A continuation with no opener above it is malformed. It stays an
                // ordinary cell rather than being dropped: dropping it would slide
                // every cell after it one column to the left, which is exactly how
                // a grid comes apart from its header row.
                continue;
            }
            // Every other cell ends whatever spans its own columns were holding.
            for cc in c.col..(c.col + c.span).min(MAX_COLS) {
                open[cc] = None;
            }
            if c.props.v_merge == Some(VMerge::Restart) {
                open[c.col] = Some((r, i));
            }
        }

        p.rows.push(Row { props, cells });
    }
    p
}

/// Element children named `local`, descending through the wrappers that hold a
/// row or a cell without being one.
///
/// `w:del` and `w:moveFrom` are *not* descended into: what they hold is a tracked
/// deletion, and a preview that shows one is showing a document that does not
/// exist. `max` bounds the result so the caller can tell "exactly the cap" from
/// "more than the cap".
fn collect<'d>(
    parent: Node<'d, 'd>,
    local: &str,
    out: &mut Vec<Node<'d, 'd>>,
    depth: usize,
    max: usize,
) {
    if depth > MAX_WRAP {
        return;
    }
    for n in elems(parent) {
        if out.len() >= max {
            return;
        }
        let name = n.tag_name().name();
        if name == local {
            out.push(n);
        } else if name == "sdt" {
            if let Some(c) = child(n, "sdtContent") {
                collect(c, local, out, depth + 1, max);
            }
        } else if matches!(name, "ins" | "moveTo" | "customXml") {
            collect(n, local, out, depth + 1, max);
        }
    }
}

/// `w:tblGrid` as px column widths.
///
/// A column of zero or unreadable width still takes its slot: the `<colgroup>` has
/// to line up with the cells, so a missing entry would shift every column after
/// it.
fn grid_px(tbl: Node) -> Vec<f32> {
    let Some(g) = child(tbl, "tblGrid") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in elems(g).filter(|e| e.tag_name().name() == "gridCol") {
        if out.len() >= MAX_COLS {
            break;
        }
        let px = xml::attr_i64(e, "w")
            .map(dxa_to_px)
            .filter(|v| v.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, MAX_COL_PX);
        out.push(px);
    }
    // A grid of nothing but zero-width columns states no geometry, and a
    // `<colgroup>` of zeroes under a fixed layout collapses the table.
    if out.iter().all(|v| *v <= 0.0) {
        out.clear();
    }
    out
}

// ── table and row geometry ───────────────────────────────────────────────────

fn table_css(tp: &TableProps, grid: &[f32]) -> Style {
    let mut s = Style::new();
    match tp.width {
        Some(Width::Pct(v)) => s.push_opt("width", fmt_pct(v.clamp(1.0, 100.0))),
        Some(Width::Dxa(v)) if v > 0 => {
            s.push_opt("width", fmt_px(dxa_to_px(v).min(MAX_TABLE_PX)))
        }
        // `auto`, `nil`, absent, or a length that reads as nothing: the grid's own
        // sum is the width the author saw. `.of-tbl`'s `max-width` is what keeps a
        // table wider than the text column from pushing the page open.
        _ => s.push_opt(
            "width",
            Some(grid.iter().sum::<f32>())
                .filter(|v| *v > 0.0)
                .and_then(fmt_px),
        ),
    }
    match tp.align {
        // A table is a block, so `w:jc` moves the box and not its text.
        Some(Align::Center) => {
            s.push("margin-left", "auto");
            s.push("margin-right", "auto");
        }
        Some(Align::Right) => s.push("margin-left", "auto"),
        // An indent and an auto margin are the same property, and `w:tblInd` only
        // means anything for a table that starts at the leading edge anyway.
        _ => s.push_opt(
            "margin-left",
            tp.ind_dxa
                .filter(|v| *v > 0)
                .map(dxa_to_px)
                .filter(|v| v.is_finite())
                .and_then(fmt_px),
        ),
    }
    s
}

fn row_css(rp: &RowProps) -> Style {
    let mut s = Style::new();
    let Some(dxa) = rp.height_dxa.filter(|v| *v > 0) else {
        return s;
    };
    if rp.height_rule == Some(HRule::Auto) {
        return s;
    }
    // Both remaining rules land on `height`, which a table row treats as a floor
    // whatever it says. So an `hRule="exact"` row that would have to clip its own
    // content grows instead — and for a preview, growing beats hiding text.
    s.push_opt("height", fmt_px(dxa_to_px(dxa).min(MAX_ROW_PX)));
    s
}

// ── cells ────────────────────────────────────────────────────────────────────

/// The table-level inputs every cell of one table resolves against.
struct Frame<'a> {
    /// `w:tblBorders` after the style chain and the table's own `w:tblPr`.
    borders: &'a TableBorders,
    /// `w:tblCellMar`, likewise.
    mar: CellMargins,
    /// The table style's base `w:tcPr`, under every cell's own.
    base: &'a CellProps,
    /// Fill behind the whole table, under a cell's own `w:shd`.
    shade: Option<Color>,
    ncols: usize,
    nrows: usize,
    /// True when the table states no usable grid, which is the only case where a
    /// cell's own `w:tcW` is worth emitting — with a `<colgroup>` the column is
    /// already sized and a per-cell width would only repeat it.
    no_grid: bool,
}

/// Where a cell sits in its table, which is what decides whether an edge takes the
/// table's outer border or its interior line.
struct Pos {
    first_row: bool,
    last_row: bool,
    first_col: bool,
    last_col: bool,
}

fn emit_cell(ctx: &mut Ctx, w: &mut Writer, cell: &Cell, f: &Frame, r: usize, depth: usize) {
    let mut cp = f.base.clone();
    style::merge_cell(&mut cp, &cell.props);

    let sides = cell_borders(
        f.borders,
        &cp.borders,
        Pos {
            first_row: r == 0,
            last_row: r + cell.rowspan >= f.nrows,
            first_col: cell.col == 0,
            last_col: cell.col + cell.span >= f.ncols,
        },
    );

    let mut s = Style::new();
    borders_css(&mut s, &sides);
    // A cell's own `w:shd` is a statement even when it paints nothing: an
    // explicit `nil` fill has to beat the table's.
    let fill = match cp.shade {
        Some(v) => v.color(),
        None => f.shade,
    };
    if let Some(c) = fill {
        s.push("background-color", &c.css());
    }
    let mut mar = f.mar;
    mar.merge(&cp.mar);
    padding_css(&mut s, &mar);
    if f.no_grid {
        match cp.width {
            Some(Width::Dxa(v)) if v > 0 => {
                s.push_opt("width", fmt_px(dxa_to_px(v).min(MAX_COL_PX)))
            }
            Some(Width::Pct(v)) => s.push_opt("width", fmt_pct(v.clamp(1.0, 100.0))),
            _ => {}
        }
    }
    if cp.no_wrap == Some(true) {
        // `pre`, not `nowrap`: a run of spaces inside a cell is content, exactly as
        // it is on the page.
        s.push("white-space", "pre");
    }

    let align = align_css(&AlignSpec {
        // `w:tcPr` has no horizontal member — a cell's text alignment is each
        // paragraph's own `w:jc`, which the paragraph emitter already carries.
        horizontal: "general",
        vertical: cp.v_align,
        // `.of-page` is already `pre-wrap` and `.of-tc` already breaks long words,
        // so asking for them here would only repeat the stylesheet per cell.
        wrap: false,
        indent_px: 0.0,
        rotation: cp.text_direction,
    });
    let mut css = s.css().to_string();
    css.push_str(&align.cell);

    let span_attr = |name: &str, v: usize| {
        if v > 1 {
            attr(name, &v.to_string())
        } else {
            String::new()
        }
    };
    let style_attr = if css.is_empty() {
        String::new()
    } else {
        attr("style", &css)
    };
    // `of-tc` is the stable hook: it is what the structural stylesheet styles, and
    // what a later pass teaches the frame's selection engine to read a table out of.
    w.open(
        "td",
        &attrs(&[
            &attr("class", "of-tc"),
            &span_attr("colspan", cell.span),
            &span_attr("rowspan", cell.rowspan),
            &style_attr,
        ]),
    );
    // Rotation cannot ride on a table cell (`transform` does not apply to one), so
    // it lands on an inner box — the same split `cellstyle::align_css` makes for a
    // sheet cell.
    let rotated = !align.inner.is_empty();
    if rotated {
        w.open("div", &attr("style", &align.inner));
    }
    // A cell's first paragraph has no predecessor: `w:contextualSpacing` drops the
    // space between neighbours of one style, and the paragraph before the *table*
    // is in another box entirely — comparing against it swallows the space at the
    // top of a cell whose style happens to match.
    ctx.prev_style = None;
    // The block walk, not a text extraction: a cell holds paragraphs, nested
    // tables and content controls, and `w:tcPr` falls through its dispatch.
    body::walk(ctx, w, cell.node, 0, depth + 1);
    if rotated {
        w.close();
    }
    w.close();
}

/// The four edges of one cell: the table's outer border where the cell is on that
/// edge, its interior line where it is not, and the cell's own `w:tcBorders` over
/// the top of both.
fn cell_borders(tbl: &TableBorders, own: &TableBorders, pos: Pos) -> BorderSides {
    let pick = |outer: Option<BorderSpec>, inner: Option<BorderSpec>, on_edge: bool| {
        if on_edge {
            outer
        } else {
            inner
        }
    };
    let mut b = BorderSides {
        top: pick(tbl.sides.top, tbl.inside_h, pos.first_row),
        bottom: pick(tbl.sides.bottom, tbl.inside_h, pos.last_row),
        left: pick(tbl.sides.left, tbl.inside_v, pos.first_col),
        right: pick(tbl.sides.right, tbl.inside_v, pos.last_col),
    };
    b.merge(&own.sides);
    b
}

/// Per-side `border-*` shorthands.
///
/// Not [`super::super::model`]'s emitter: that one also turns a `w:pBdr@w:space`
/// into padding on the same side, which in a cell would fight the cell margins.
fn borders_css(s: &mut Style, b: &BorderSides) {
    for (side, spec) in [
        ("top", b.top),
        ("right", b.right),
        ("bottom", b.bottom),
        ("left", b.left),
    ] {
        // `None` here is an edge that draws nothing, which under
        // `border-collapse` still lets the neighbouring cell's edge show — the
        // same thing Word does with a one-sided border between two cells.
        let Some(e) = spec.and_then(style::to_border) else {
            continue;
        };
        let Some(px) = Some(e.width_px).filter(|v| *v > 0.0).and_then(fmt_px) else {
            continue;
        };
        let style = if e.style.is_empty() { "solid" } else { e.style };
        let color = e
            .color
            .map(|c| c.css())
            // `auto` leaves the edge at the reader's text colour rather than
            // resolving to a black this document never stated.
            .unwrap_or_else(|| "currentColor".to_string());
        s.push(&format!("border-{side}"), &format!("{px} {style} {color}"));
    }
}

/// `padding`, but only when the resolved margins are not Word's defaults — which
/// `.of-tc` already states.
fn padding_css(s: &mut Style, m: &CellMargins) {
    let dxa = [
        m.top.unwrap_or(DEFAULT_MAR_DXA[0]),
        m.right.unwrap_or(DEFAULT_MAR_DXA[1]),
        m.bottom.unwrap_or(DEFAULT_MAR_DXA[2]),
        m.left.unwrap_or(DEFAULT_MAR_DXA[3]),
    ];
    if dxa == DEFAULT_MAR_DXA {
        return;
    }
    let px: Vec<String> = dxa
        .iter()
        .map(|d| fmt_px(dxa_to_px(*d)).unwrap_or_else(|| "0px".to_string()))
        .collect();
    s.push("padding", &px.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::style::Styles;

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    /// Renders one `w:tbl` on its own, with no stylesheet: what a table looks like
    /// when the document states everything itself.
    fn render(tbl: &str) -> String {
        render_with(tbl, "")
    }

    /// The same, plus a `word/styles.xml` body so a `w:tblStyle` can resolve.
    fn render_with(tbl: &str, styles_body: &str) -> String {
        let theme = Theme::default();
        let styles = Styles::parse(
            &format!("<w:styles {NS}>{styles_body}</w:styles>"),
            &theme,
        );
        let mut numbering = super::super::numbering::Numbering::empty();
        let terms = crate::highlight::Terms::new(&[]);
        let mut marker = crate::highlight::Marker::new();
        let mut notes = crate::emit::Notes::new();
        let src = format!("<w:body {NS}>{tbl}</w:body>");
        let doc = crate::xml::parse(&src).expect("fixture parses");
        let mut w = Writer::new(1 << 20);
        // A `Ctx` carries the media path, which is file-backed, so even a table
        // rendered on its own needs a package behind it. No fixture here
        // references an image, so it can be empty.
        let pkg = crate::pkg::TestPkg::new("tbl", &[]);
        let mut zip = pkg.open();
        let mut budget = crate::pkg::Budget::new();
        let mut media = crate::media::MediaCache::new();
        let mut mb = crate::media::MediaBudget::new();
        let rels = crate::opc::Rels::new();
        let note_store = super::super::notes::Store::default();
        let mut ctx = Ctx {
            zip: &mut zip,
            budget: &mut budget,
            media: &mut media,
            mb: &mut mb,
            rels: &rels,
            part: "word/document.xml",
            column_px: 624.0,
            images: 0,
            styles: &styles,
            numbering: &mut numbering,
            theme: &theme,
            default_font: None,
            terms: &terms,
            marker: &mut marker,
            notes: &mut notes,
            note_store: &note_store,
            used_notes: Vec::new(),
            in_note: false,
            pending: Vec::new(),
            boxes: 0,
            box_depth: 0,
            bookmarks: 0,
            paras: 0,
            cells: 0,
            prev_style: None,
        };
        body::walk(&mut ctx, &mut w, doc.root_element(), 0, 0);
        w.finish()
    }

    /// A `w:tbl` with a grid of `cols` equal columns and the given rows.
    fn table(cols: &[i64], rows: &str) -> String {
        let grid: String = cols
            .iter()
            .map(|w| format!("<w:gridCol w:w=\"{w}\"/>"))
            .collect();
        format!("<w:tbl><w:tblGrid>{grid}</w:tblGrid>{rows}</w:tbl>")
    }

    fn tr(cells: &str) -> String {
        format!("<w:tr>{cells}</w:tr>")
    }

    /// A cell with `tcpr` properties holding one paragraph of `text`.
    fn tc(tcpr: &str, text: &str) -> String {
        let pr = if tcpr.is_empty() {
            String::new()
        } else {
            format!("<w:tcPr>{tcpr}</w:tcPr>")
        };
        format!("<w:tc>{pr}<w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:tc>")
    }

    #[test]
    fn a_grid_becomes_a_colgroup_of_explicit_widths() {
        // 1440 dxa = 96px, 2880 = 192px.
        let html = render(&table(&[1440, 2880], &tr(&tc("", "café"))));
        assert!(
            html.contains("<colgroup><col style=\"width:96px;\"><col style=\"width:192px;\">"),
            "{html}"
        );
        // The table's own width is the grid's sum when `w:tblW` states nothing.
        assert!(html.contains("<table class=\"of-tbl\" style=\"width:288px;\">"), "{html}");
        assert!(html.contains("<td class=\"of-tc\">"), "{html}");
    }

    #[test]
    fn a_table_with_no_grid_falls_back_to_an_auto_layout() {
        // A table with no rows at all draws nothing: an empty `<table>` would still
        // take the stylesheet's borders.
        assert!(render("<w:tbl><w:tblGrid/></w:tbl>").is_empty());
        let html = render(&format!(
            "<w:tbl><w:tblGrid/>{}</w:tbl>",
            tr(&tc("<w:tcW w:w=\"1440\" w:type=\"dxa\"/>", "café"))
        ));
        assert!(html.contains("class=\"of-tbl of-tbl-auto\""), "{html}");
        assert!(!html.contains("<colgroup>"), "{html}");
        // With no grid the cell's own `w:tcW` is the only geometry there is.
        assert!(html.contains("width:96px"), "{html}");
    }

    #[test]
    fn tbl_w_states_the_width_in_percent_or_pixels() {
        // 2500 fiftieths of a percent = 50%.
        let pct = render(&format!(
            "<w:tbl><w:tblPr><w:tblW w:w=\"2500\" w:type=\"pct\"/></w:tblPr>\
             <w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>{}</w:tbl>",
            tr(&tc("", "café"))
        ));
        assert!(pct.contains("width:50%"), "{pct}");
        let dxa = render(&format!(
            "<w:tbl><w:tblPr><w:tblW w:w=\"2880\" w:type=\"dxa\"/><w:jc w:val=\"center\"/>\
             </w:tblPr><w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>{}</w:tbl>",
            tr(&tc("", "café"))
        ));
        assert!(dxa.contains("width:192px"), "{dxa}");
        assert!(dxa.contains("margin-left:auto;margin-right:auto;"), "{dxa}");
        // An indent is a leading margin, and only for a table that starts at the
        // leading edge.
        let ind = render(&format!(
            "<w:tbl><w:tblPr><w:tblInd w:w=\"720\" w:type=\"dxa\"/></w:tblPr>\
             <w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>{}</w:tbl>",
            tr(&tc("", "café"))
        ));
        assert!(ind.contains("margin-left:48px"), "{ind}");
    }

    #[test]
    fn grid_span_becomes_a_colspan() {
        let rows = format!(
            "{}{}",
            tr(&tc("<w:gridSpan w:val=\"2\"/>", "café")),
            tr(&format!("{}{}", tc("", "naïve"), tc("", "Widget")))
        );
        let html = render(&table(&[1440, 1440], &rows));
        assert!(html.contains("colspan=\"2\""), "{html}");
        // The spanning cell is one element, not two.
        assert_eq!(html.matches("<td").count(), 3, "{html}");
    }

    #[test]
    fn a_three_row_vmerge_is_one_rowspan_with_no_continuation_elements() {
        let merged = |val: &str, text: &str| {
            let v = if val.is_empty() {
                "<w:vMerge/>".to_string()
            } else {
                format!("<w:vMerge w:val=\"{val}\"/>")
            };
            tc(&v, text)
        };
        let rows = format!(
            "{}{}{}",
            tr(&format!("{}{}", merged("restart", "café"), tc("", "a"))),
            // No `w:val` at all is the continuation, which is the trap.
            tr(&format!("{}{}", merged("", ""), tc("", "b"))),
            tr(&format!("{}{}", merged("continue", ""), tc("", "c")))
        );
        let html = render(&table(&[1440, 1440], &rows));
        assert!(html.contains("rowspan=\"3\""), "{html}");
        assert_eq!(html.matches("rowspan").count(), 1, "{html}");
        // Three rows, four cells: the opener, and one on the right of each row.
        assert_eq!(html.matches("<td").count(), 4, "{html}");
        assert_eq!(html.matches("<tr").count(), 3, "{html}");
        assert!(html.contains("café"), "{html}");
    }

    #[test]
    fn an_orphan_continuation_degrades_without_shifting_its_row() {
        // A continuation in the very first row has nothing above it to continue.
        let rows = format!(
            "{}{}",
            tr(&format!(
                "{}{}",
                tc("<w:vMerge/>", "café"),
                tc("", "naïve")
            )),
            tr(&format!("{}{}", tc("", "Widget"), tc("", "example.org")))
        );
        let html = render(&table(&[1440, 1440], &rows));
        // Two cells per row: the orphan is emitted as an ordinary cell, so the
        // second column still lines up with the row below.
        assert_eq!(html.matches("<td").count(), 4, "{html}");
        assert!(!html.contains("rowspan"), "{html}");
        assert!(html.contains("café") && html.contains("naïve"), "{html}");
    }

    #[test]
    fn a_deleted_row_is_absent() {
        let rows = format!(
            "{}{}{}",
            tr(&tc("", "café")),
            // The row's own `w:trPr` marks it deleted.
            format!(
                "<w:tr><w:trPr><w:del w:id=\"1\"/></w:trPr>{}</w:tr>",
                tc("", "removed")
            ),
            // And a row wrapped in a `w:del`.
            format!("<w:del>{}</w:del>", tr(&tc("", "also removed")))
        );
        let html = render(&table(&[1440], &rows));
        assert!(html.contains("café"), "{html}");
        assert!(!html.contains("removed"), "{html}");
        assert_eq!(html.matches("<tr").count(), 1, "{html}");
    }

    #[test]
    fn table_borders_reach_a_cell_and_a_cell_can_override_them() {
        let rows = format!(
            "{}{}",
            tr(&format!(
                "{}{}",
                tc("", "café"),
                tc(
                    "<w:tcBorders><w:top w:val=\"double\" w:sz=\"24\" w:color=\"FF0000\"/>\
                     </w:tcBorders>",
                    "naïve"
                )
            )),
            tr(&format!("{}{}", tc("", "Widget"), tc("", "example.org")))
        );
        let html = render(&format!(
            "<w:tbl><w:tblPr><w:tblBorders>\
             <w:top w:val=\"single\" w:sz=\"8\" w:color=\"000000\"/>\
             <w:insideH w:val=\"dotted\" w:sz=\"8\"/>\
             </w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w=\"1440\"/>\
             <w:gridCol w:w=\"1440\"/></w:tblGrid>{rows}</w:tbl>"
        ));
        // The first row's cells take the table's outer top edge, 8 eighths = 1pt.
        assert!(
            html.contains("border-top:1.33px solid #000000;"),
            "{html}"
        );
        // The cell that states its own top edge wins: 24 eighths = 3pt = 4px.
        assert!(html.contains("border-top:4px double #ff0000;"), "{html}");
        // The second row is not on the outer edge, so its top is the interior line.
        assert!(html.contains("border-top:1.33px dotted currentColor;"), "{html}");
    }

    #[test]
    fn a_cell_shading_paints_a_background() {
        let html = render(&table(
            &[1440],
            &tr(&tc(
                "<w:shd w:val=\"clear\" w:fill=\"EEEEEE\"/>",
                "café",
            )),
        ));
        assert!(html.contains("background-color:#eeeeee"), "{html}");
    }

    #[test]
    fn cell_properties_reach_the_shared_cell_vocabulary() {
        let html = render(&table(
            &[1440],
            &tr(&tc(
                "<w:vAlign w:val=\"center\"/><w:noWrap/>\
                 <w:tcMar><w:top w:w=\"120\" w:type=\"dxa\"/><w:left w:w=\"0\" w:type=\"dxa\"/>\
                 <w:bottom w:w=\"120\" w:type=\"dxa\"/><w:right w:w=\"0\" w:type=\"dxa\"/></w:tcMar>",
                "café",
            )),
        ));
        assert!(html.contains("vertical-align:middle"), "{html}");
        assert!(html.contains("white-space:pre;"), "{html}");
        // 120 dxa = 6pt = 8px, and a stated zero is a real zero.
        assert!(html.contains("padding:8px 0px 8px 0px"), "{html}");

        // A rotated cell splits: the writing direction on an inner box, because a
        // transform does not apply to a table cell.
        let rot = render(&table(
            &[1440],
            &tr(&tc("<w:textDirection w:val=\"btLr\"/>", "café")),
        ));
        assert!(rot.contains("<div style=\"display:inline-block;transform:rotate(-90deg);\">"), "{rot}");
    }

    #[test]
    fn default_cell_margins_cost_no_declaration() {
        // Word's own defaults, stated explicitly: the stylesheet already says them.
        let html = render(&table(
            &[1440],
            &tr(&tc(
                "<w:tcMar><w:left w:w=\"108\" w:type=\"dxa\"/>\
                 <w:right w:w=\"108\" w:type=\"dxa\"/></w:tcMar>",
                "café",
            )),
        ));
        assert!(!html.contains("padding"), "{html}");
        assert!(html.contains("<td class=\"of-tc\">"), "{html}");
    }

    #[test]
    fn a_row_height_lands_on_the_row() {
        let exact = render(&format!(
            "<w:tbl><w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>\
             <w:tr><w:trPr><w:trHeight w:val=\"480\" w:hRule=\"exact\"/></w:trPr>{}</w:tr></w:tbl>",
            tc("", "café")
        ));
        // 480 dxa = 24pt = 32px.
        assert!(exact.contains("<tr style=\"height:32px;\">"), "{exact}");
        // An `auto` rule ignores the length, as Word does.
        let auto = render(&format!(
            "<w:tbl><w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>\
             <w:tr><w:trPr><w:trHeight w:val=\"480\" w:hRule=\"auto\"/></w:trPr>{}</w:tr></w:tbl>",
            tc("", "café")
        ));
        assert!(auto.contains("<tr>"), "{auto}");
    }

    #[test]
    fn a_table_style_base_border_reaches_a_cell() {
        let styles = r#"<w:style w:type="table" w:styleId="Base"><w:name w:val="Base"/>
             <w:tblPr><w:tblBorders>
               <w:top w:val="single" w:sz="8"/><w:bottom w:val="single" w:sz="8"/>
               <w:left w:val="single" w:sz="8"/><w:right w:val="single" w:sz="8"/>
               <w:insideH w:val="single" w:sz="8"/><w:insideV w:val="single" w:sz="8"/>
             </w:tblBorders></w:tblPr></w:style>
             <w:style w:type="table" w:styleId="Grid"><w:name w:val="Grid"/>
               <w:basedOn w:val="Base"/>
               <w:tcPr><w:shd w:val="clear" w:fill="F2F2F2"/></w:tcPr></w:style>"#;
        let html = render_with(
            &format!(
                "<w:tbl><w:tblPr><w:tblStyle w:val=\"Grid\"/></w:tblPr>\
                 <w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>{}</w:tbl>",
                tr(&tc("", "café"))
            ),
            styles,
        );
        // The borders come from the `w:basedOn` parent's base `w:tblPr`, and the
        // shading from the leaf's base `w:tcPr`.
        assert!(html.contains("border-top:1.33px solid currentColor;"), "{html}");
        assert!(html.contains("border-left:1.33px solid currentColor;"), "{html}");
        assert!(html.contains("background-color:#f2f2f2"), "{html}");
    }

    #[test]
    fn a_nested_table_renders_and_a_fourth_level_degrades() {
        // Levels 1..3 render; the fourth is the one that is dropped.
        let mut inner = table(&[720], &tr(&tc("", "level4")));
        for name in ["level3", "level2"] {
            inner = table(
                &[1440],
                &tr(&format!(
                    "<w:tc><w:p><w:r><w:t>{name}</w:t></w:r></w:p>{inner}</w:tc>"
                )),
            );
        }
        let html = render(&table(
            &[2880],
            &tr(&format!(
                "<w:tc><w:p><w:r><w:t>level1</w:t></w:r></w:p>{inner}</w:tc>"
            )),
        ));
        for name in ["level1", "level2", "level3"] {
            assert!(html.contains(name), "{name} missing from {html}");
        }
        assert!(!html.contains("level4"), "{html}");
        assert_eq!(html.matches("<table").count(), 3, "{html}");
    }

    #[test]
    fn a_run_inside_a_cell_keeps_its_formatting() {
        let cell = "<w:tc><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>café</w:t></w:r></w:p></w:tc>";
        let html = render(&table(&[1440], &tr(cell)));
        assert!(html.contains("font-weight:700"), "{html}");
        // The paragraph goes through the ordinary emitter, class and all.
        assert!(html.contains("<p class=\"of-p\""), "{html}");
    }

    #[test]
    fn a_content_control_in_a_cell_keeps_its_text() {
        let cell = "<w:tc><w:sdt><w:sdtContent><w:p><w:r><w:t>café</w:t></w:r></w:p>\
                    </w:sdtContent></w:sdt></w:tc>";
        let html = render(&table(&[1440], &tr(cell)));
        assert!(html.contains("café"), "{html}");
        // A row and a cell may be wrapped the same way.
        let wrapped = render(&table(
            &[1440],
            &format!(
                "<w:sdt><w:sdtContent>{}</w:sdtContent></w:sdt>",
                tr(&tc("", "naïve"))
            ),
        ));
        assert!(wrapped.contains("naïve"), "{wrapped}");
    }

    #[test]
    fn a_wide_grid_and_a_long_table_are_bounded() {
        let cols: Vec<i64> = vec![120; MAX_COLS + 8];
        let cells: String = (0..MAX_COLS + 8).map(|_| tc("", "x")).collect();
        let html = render(&table(&cols, &tr(&cells)));
        assert_eq!(html.matches("<col ").count(), MAX_COLS, "colgroup unbounded");
        assert_eq!(html.matches("<td").count(), MAX_COLS, "cells unbounded");

        let rows: String = (0..MAX_ROWS + 5).map(|_| tr(&tc("", "x"))).collect();
        let html = render(&table(&[1440], &rows));
        assert_eq!(html.matches("<tr").count(), MAX_ROWS, "rows unbounded");
    }

    #[test]
    fn a_cell_border_of_none_cancels_the_table_edge() {
        let html = render(&format!(
            "<w:tbl><w:tblPr><w:tblBorders><w:top w:val=\"single\" w:sz=\"8\"/>\
             </w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>{}</w:tbl>",
            tr(&tc(
                "<w:tcBorders><w:top w:val=\"none\"/></w:tcBorders>",
                "café"
            ))
        ));
        assert!(!html.contains("border-top"), "{html}");
    }
}
