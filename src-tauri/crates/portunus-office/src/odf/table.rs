//! `table:table` → an HTML `<table>`: the column geometry, the spans, and the
//! per-cell property cascade.
//!
//! A cell holds *content* — paragraphs, lists, nested tables — so the block walk
//! is re-entered for a cell's children rather than a resolved string being handed
//! over, exactly as in `docx::table`.
//!
//! Two things are simpler here than in WordprocessingML and one is harder. ODF
//! states a vertical span on the cell that opens it and writes an explicit
//! `table:covered-table-cell` for every slot it covers, so there is no merge
//! pre-pass: the covered cells are skipped and HTML's own `rowspan` describes the
//! same geometry. Borders and padding are the cell style's alone — there is no
//! table-level "inside horizontal" vocabulary to resolve an edge against — so a
//! cell needs no knowledge of where it sits. What is harder is repetition:
//! `table:number-columns-repeated` and `-rows-repeated` are counts an untrusted
//! document states, and each one has to be expanded under a cap.
//!
//! Nothing here measures text, so the columns come from the document's own
//! `style:column-width` under `table-layout:fixed`, and the structural CSS
//! (`border-collapse`, the default cell padding) belongs to whichever shape's
//! stylesheet is rendering — `text::Classes` carries its names — so the common
//! cell carries no `style` attribute at all.

use super::super::cellstyle::{align_css, AlignSpec};
use super::super::html::{attr, attrs, fmt_pct, fmt_px, Style, Writer};
use super::super::xml::{attr_local, attr_u32, elems};
use super::length::Measure;
use super::style::{CellProps, Edge, Family, Sides, TableAlign, TableProps};
use super::text::{self, Ctx};
use roxmltree::Node;

/// Tables that may enclose one another. A layout table inside a layout table
/// happens; three deep is past every real use, and a generated document can nest
/// without bound.
const MAX_NEST: usize = 3;

/// Rows per table.
const MAX_ROWS: usize = 1_000;

/// Cells per table, and cells across the whole document. Both are needed: one
/// table of a million cells and a thousand tables of a thousand cells each cost
/// the same, and only the second gets past a per-table cap.
const MAX_CELLS: usize = 8_000;
const MAX_DOC_CELLS: usize = 40_000;

/// Grid columns, shared with the span clamp so the columns, the spans and the
/// `<colgroup>` cannot disagree about how wide the table is. The same ceiling
/// `docx::style::MAX_GRID_COLS` puts on a `w:gridSpan`.
const MAX_COLS: usize = 64;

/// Nesting of the elements that hold rows or columns without being one —
/// `table:table-header-rows`, `table:table-row-group`, `table:table-columns`.
const MAX_WRAP: usize = 8;

/// Sane geometry bounds. A column or row larger than these is corrupt: the page
/// itself is capped at 4096px, and a table cannot usefully exceed it.
const MAX_COL_PX: f32 = 2_000.0;
const MAX_ROW_PX: f32 = 2_000.0;
const MAX_TABLE_PX: f32 = 4_096.0;

/// A table stopped early: the row cap and the cell caps both end the same way, and
/// what the reader needs to know is that this is not all of it.
const NOTE_CLIPPED: &str = "Large table cut short";

/// The document-wide cell budget, which stops a whole table rather than trimming
/// one — so it needs its own wording.
const NOTE_DOC_CELLS: &str = "Later tables not shown";

// ── plan ─────────────────────────────────────────────────────────────────────

/// One cell that draws: where it reaches, not where it sits — a covered slot is
/// not planned at all, because HTML infers it from the span that covers it.
struct Cell<'d> {
    node: Node<'d, 'd>,
    /// `table:number-columns-spanned`, at least 1.
    span: usize,
    /// `table:number-rows-spanned`, at least 1.
    rowspan: usize,
}

struct Row<'d> {
    node: Node<'d, 'd>,
    /// Inside a `table:table-header-rows`, i.e. part of the `<thead>`.
    header: bool,
    cells: Vec<Cell<'d>>,
}

struct Plan<'d> {
    rows: Vec<Row<'d>>,
    /// `table:table-cell` elements planned, for the document-wide budget.
    cells: usize,
    clipped: bool,
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Emits one `table:table`. `depth` is how many tables already enclose it, so the
/// outermost table in a body is depth 0.
pub fn emit_table<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, tbl: Node<'a, 'a>, depth: usize) {
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

    let tp = ctx
        .styles
        .resolve(Family::Table, attr_local(tbl, "style-name").unwrap_or(""))
        .table;
    // `table:display="false"` is a table the document hides. An empty `<table>`
    // would still take the stylesheet's borders, so nothing is emitted at all.
    if tp.display == Some(false) {
        return;
    }

    let grid = columns(ctx, tbl);
    let plan = plan(tbl, MAX_CELLS.min(budget));
    if plan.rows.is_empty() {
        return;
    }
    ctx.cells += plan.cells;
    if plan.clipped {
        ctx.notes.add(NOTE_CLIPPED);
    }

    let class = if grid.is_empty() {
        // Nothing to lay out against: a fixed layout with no column widths splits
        // the table equally, which is a geometry no document asked for.
        ctx.classes.table_auto
    } else {
        ctx.classes.table
    };
    w.open(
        "table",
        &attrs(&[&attr("class", class), &table_css(&tp, &grid).to_attr()]),
    );
    if !grid.is_empty() {
        w.open("colgroup", "");
        for px in &grid {
            let mut s = Style::new();
            s.push_opt("width", fmt_px(*px));
            w.void("col", &s.to_attr());
        }
        w.close();
    }

    // A `<thead>` per *run* of header rows: a producer writes them first, and
    // document order is what the single forward pass buys.
    let mut in_head = false;
    for row in &plan.rows {
        if w.is_full() {
            break;
        }
        if row.header != in_head {
            if in_head {
                w.close();
            } else {
                w.open("thead", "");
            }
            in_head = row.header;
        }
        emit_row(ctx, w, row, depth);
    }
    if in_head {
        w.close();
    }
    w.close();
}

fn emit_row<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, row: &Row<'a>, depth: usize) {
    let rp = ctx
        .styles
        .resolve(Family::TableRow, attr_local(row.node, "style-name").unwrap_or(""))
        .row;
    let mut s = Style::new();
    // `style:use-optimal-row-height` says the height is the content's, not the
    // stated one. A row's `height` is a floor whatever it says, so an exact one
    // that would clip its content grows instead — for a preview, growing beats
    // hiding text.
    if rp.optimal != Some(true) {
        s.push_opt(
            "height",
            rp.height_px
                .filter(|v| *v > 0.0)
                .map(|v| v.min(MAX_ROW_PX))
                .and_then(fmt_px),
        );
    }
    w.open("tr", &s.to_attr());
    for cell in &row.cells {
        if w.is_full() {
            break;
        }
        emit_cell(ctx, w, cell, depth);
    }
    w.close();
}

fn emit_cell<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, cell: &Cell<'a>, depth: usize) {
    let cp = ctx
        .styles
        .resolve(
            Family::TableCell,
            attr_local(cell.node, "style-name").unwrap_or(""),
        )
        .cell;
    let (css, inner) = cell_css(&cp);
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
    // The cell class is the stable hook: what the structural stylesheet styles,
    // and what the frame's selection engine reads a table out of. A variant whose
    // stylesheet styles the tag instead publishes none, and gets no `class=""`.
    let class_attr = if ctx.classes.cell.is_empty() {
        String::new()
    } else {
        attr("class", ctx.classes.cell)
    };
    w.open(
        "td",
        &attrs(&[
            &class_attr,
            &span_attr("colspan", cell.span),
            &span_attr("rowspan", cell.rowspan),
            &style_attr,
        ]),
    );
    // Rotation cannot ride on a table cell (`transform` does not apply to one), so
    // it lands on an inner box — the same split `cellstyle::align_css` makes for a
    // sheet cell.
    let rotated = !inner.is_empty();
    if rotated {
        w.open("div", &attr("style", &inner));
    }
    // A cell's first paragraph has no predecessor: `style:contextual-spacing` drops
    // the space between neighbours of one style, and the paragraph before the
    // *table* is in another box entirely.
    ctx.prev_style = None;
    // The block walk, not a text extraction: a cell holds paragraphs, lists and
    // nested tables.
    text::walk(ctx, w, cell.node, 0, depth + 1);
    if rotated {
        w.close();
    }
    w.close();
}

/// Reads the whole table before anything is emitted, so the document-wide cell
/// budget is charged once and a clipped table can say so before its first row.
fn plan<'d>(tbl: Node<'d, 'd>, limit: usize) -> Plan<'d> {
    let mut p = Plan {
        rows: Vec::new(),
        cells: 0,
        clipped: false,
    };
    let mut nodes: Vec<(Node<'d, 'd>, bool)> = Vec::new();
    collect_rows(tbl, false, &mut nodes, 0, &mut p.clipped);

    let mut budget = limit;
    for (node, header) in nodes {
        if budget == 0 {
            p.clipped = true;
            break;
        }
        let mut cells: Vec<Cell> = Vec::new();
        let mut cols = 0usize;
        for tc in elems(node) {
            if budget == 0 || cols >= MAX_COLS {
                p.clipped = true;
                break;
            }
            // A covered slot is planned as nothing: the span that covers it already
            // describes it, and an empty `<td>` beside a `rowspan` would push the
            // rest of the row one column right.
            match tc.tag_name().name() {
                "table-cell" => {}
                "covered-table-cell" => {
                    cols += repeat(tc, "number-columns-repeated", MAX_COLS);
                    continue;
                }
                _ => continue,
            }
            let span = count(tc, "number-columns-spanned", MAX_COLS).min(MAX_COLS - cols);
            let rowspan = count(tc, "number-rows-spanned", MAX_ROWS);
            // A repeated cell is the same content again — empty, in every real
            // document — and each copy is a cell against both budgets.
            let reps = repeat(tc, "number-columns-repeated", MAX_COLS)
                .min(budget)
                .min((MAX_COLS - cols) / span.max(1));
            for _ in 0..reps {
                cells.push(Cell {
                    node: tc,
                    span,
                    rowspan,
                });
                cols += span;
                budget -= 1;
                p.cells += 1;
            }
        }
        p.rows.push(Row {
            node,
            header,
            cells,
        });
    }
    p
}

/// Rows in document order, descending through the elements that group them, with
/// each row's `table:number-rows-repeated` expanded.
fn collect_rows<'d>(
    parent: Node<'d, 'd>,
    header: bool,
    out: &mut Vec<(Node<'d, 'd>, bool)>,
    depth: usize,
    clipped: &mut bool,
) {
    if depth > MAX_WRAP {
        return;
    }
    for n in elems(parent) {
        if out.len() >= MAX_ROWS {
            *clipped = true;
            return;
        }
        match n.tag_name().name() {
            "table-row" => {
                let reps = repeat(n, "number-rows-repeated", MAX_ROWS)
                    .min(MAX_ROWS - out.len());
                for _ in 0..reps {
                    out.push((n, header));
                }
            }
            // A header row group repeats at the top of every printed page; here it
            // is a `<thead>` once.
            "table-header-rows" => collect_rows(n, true, out, depth + 1, clipped),
            "table-rows" | "table-row-group" => {
                collect_rows(n, header, out, depth + 1, clipped)
            }
            _ => {}
        }
    }
}

/// The column widths, from each `table:table-column`'s own `table-column` style.
///
/// A column of zero or unstated width still takes its slot: the `<colgroup>` has
/// to line up with the cells, so a missing entry would shift every column after
/// it.
fn columns(ctx: &Ctx, tbl: Node) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    collect_columns(ctx, tbl, &mut out, 0);
    // A grid of nothing but zero-width columns states no geometry, and a
    // `<colgroup>` of zeroes under a fixed layout collapses the table.
    if out.iter().all(|v| *v <= 0.0) {
        out.clear();
    }
    out
}

fn collect_columns(ctx: &Ctx, parent: Node, out: &mut Vec<f32>, depth: usize) {
    if depth > MAX_WRAP {
        return;
    }
    for n in elems(parent) {
        if out.len() >= MAX_COLS {
            return;
        }
        match n.tag_name().name() {
            "table-column" => {
                let px = ctx
                    .styles
                    .resolve(Family::TableColumn, attr_local(n, "style-name").unwrap_or(""))
                    .column
                    .width_px
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
                    .clamp(0.0, MAX_COL_PX);
                let reps =
                    repeat(n, "number-columns-repeated", MAX_COLS).min(MAX_COLS - out.len());
                for _ in 0..reps {
                    out.push(px);
                }
            }
            "table-columns" | "table-header-columns" | "table-column-group" => {
                collect_columns(ctx, n, out, depth + 1)
            }
            // The columns come before the rows; nothing after them is a column.
            _ => {}
        }
    }
}

/// A `table:number-*-repeated` count: at least one copy, because the element
/// itself is the first.
fn repeat(n: Node, name: &str, max: usize) -> usize {
    count(n, name, max)
}

/// A count attribute, clamped to `1..=max`. Zero is not a count, and an unbounded
/// one is a generated document asking for a million rows.
fn count(n: Node, name: &str, max: usize) -> usize {
    attr_u32(n, name)
        .map(|v| (v as usize).clamp(1, max))
        .unwrap_or(1)
}

// ── geometry and cell CSS ────────────────────────────────────────────────────

fn table_css(tp: &TableProps, grid: &[f32]) -> Style {
    let mut s = Style::new();
    // `table:align="margins"` is the one alignment that is also a width: the table
    // stretches between the page margins, i.e. across the whole text column.
    if tp.align == Some(TableAlign::Margins) {
        s.push("width", "100%");
    } else {
        match tp.width {
            Some(Measure::Px(v)) if v > 0.0 => {
                s.push_opt("width", fmt_px(v.min(MAX_TABLE_PX)))
            }
            Some(Measure::Percent(v)) => s.push_opt("width", fmt_pct(v.clamp(1.0, 100.0))),
            // Absent or unusable: the columns' own sum is the width the author saw.
            // `.of-tbl`'s `max-width` keeps a table wider than the text column from
            // pushing the page open.
            _ => s.push_opt(
                "width",
                Some(grid.iter().sum::<f32>())
                    .filter(|v| *v > 0.0)
                    .and_then(fmt_px),
            ),
        }
    }
    match tp.align {
        // A table is a block, so the alignment moves the box and not its text.
        Some(TableAlign::Center) => {
            s.push("margin-left", "auto");
            s.push("margin-right", "auto");
        }
        Some(TableAlign::Right) => s.push("margin-left", "auto"),
        // An indent and an auto margin are the same property, and `fo:margin-left`
        // only means anything for a table that starts at the leading edge anyway.
        _ => s.push_opt(
            "margin-left",
            tp.indent_px
                .filter(|v| *v > 0.0 && v.is_finite())
                .and_then(fmt_px),
        ),
    }
    s
}

/// One cell's own declarations, plus the inner box a rotation needs.
fn cell_css(cp: &CellProps) -> (String, String) {
    let mut s = Style::new();
    borders_css(&mut s, &cp.borders);
    if let Some(c) = cp.background.as_ref() {
        s.push("background-color", &c.css());
    }
    padding_css(&mut s, cp);
    if cp.wrap == Some(false) {
        // `pre`, not `nowrap`: a run of spaces inside a cell is content, exactly as
        // it is on the page.
        s.push("white-space", "pre");
    }
    let align = align_css(&AlignSpec {
        // ODF has no horizontal member on a cell — a cell's text alignment is each
        // paragraph's own `fo:text-align`, which the paragraph emitter carries.
        horizontal: "general",
        vertical: cp.v_align,
        // `.of-page` is already `pre-wrap` and `.of-tc` already breaks long words,
        // so asking for them here would only repeat the stylesheet per cell.
        wrap: false,
        indent_px: 0.0,
        rotation: cp.rotation,
    });
    let mut css = s.css().to_string();
    css.push_str(&align.cell);
    (css, align.inner)
}

/// Per-side `border-*` shorthands.
///
/// Not [`super::super::model`]'s emitter: that one also turns a paragraph border's
/// `space` into padding on the same side, which in a cell would fight
/// `fo:padding`.
fn borders_css(s: &mut Style, b: &Sides) {
    for (side, edge) in [
        ("top", b.top),
        ("right", b.right),
        ("bottom", b.bottom),
        ("left", b.left),
    ] {
        // `Edge::None` is an edge the document switched off, which under
        // `border-collapse` still lets the neighbouring cell's edge show — the same
        // thing Writer does with a one-sided border between two cells.
        let Some(e) = edge.and_then(|e| match e {
            Edge::Set(b) => Some(b),
            Edge::None => None,
        }) else {
            continue;
        };
        let Some(css) = e.css() else { continue };
        s.push(&format!("border-{side}"), &css);
    }
}

/// The sides the cell states, as longhands: an unstated side keeps `.of-tc`'s own
/// padding rather than being reset to zero by a shorthand.
fn padding_css(s: &mut Style, cp: &CellProps) {
    for (side, px) in [
        ("top", cp.padding.top),
        ("right", cp.padding.right),
        ("bottom", cp.padding.bottom),
        ("left", cp.padding.left),
    ] {
        s.push_opt(
            &format!("padding-{side}"),
            px.filter(|v| v.is_finite()).and_then(fmt_px),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odf::text::tests::{body, content, odt, styles};

    /// A `table-column` style of `w` inches and a `table-cell` style that paints,
    /// which between them cover every per-axis lookup a table makes.
    fn table_styles() -> String {
        "<style:style style:name=\"co1\" style:family=\"table-column\">\
         <style:table-column-properties style:column-width=\"1in\"/></style:style>\
         <style:style style:name=\"co2\" style:family=\"table-column\">\
         <style:table-column-properties style:column-width=\"2in\"/></style:style>\
         <style:style style:name=\"ro1\" style:family=\"table-row\">\
         <style:table-row-properties style:row-height=\"0.5in\"/></style:style>\
         <style:style style:name=\"ce1\" style:family=\"table-cell\">\
         <style:table-cell-properties fo:border=\"0.5pt solid #808080\" \
         fo:background-color=\"#eeeeee\" fo:padding-left=\"6pt\" \
         style:vertical-align=\"middle\"/></style:style>\
         <style:style style:name=\"ta1\" style:family=\"table\">\
         <style:table-properties table:align=\"margins\"/></style:style>"
            .to_string()
    }

    fn table(tag: &str, tbl: &str) -> String {
        odt(tag, &styles(&table_styles()), &content("", tbl)).html()
    }

    #[test]
    fn the_column_geometry_comes_from_the_documents_own_widths() {
        let html = table(
            "cols",
            "<table:table>\
             <table:table-column table:style-name=\"co1\"/>\
             <table:table-column table:style-name=\"co2\"/>\
             <table:table-row><table:table-cell><text:p>a</text:p></table:table-cell>\
             <table:table-cell><text:p>b</text:p></table:table-cell></table:table-row>\
             </table:table>",
        );
        assert!(html.contains("<colgroup>"), "{html}");
        assert!(html.contains("<col style=\"width:96px;\">"), "{html}");
        assert!(html.contains("<col style=\"width:192px;\">"), "{html}");
        // Nothing measures text, so the table is as wide as its columns say.
        assert!(html.contains("width:288px"), "{html}");
        assert!(html.contains("class=\"of-tbl\""), "{html}");
    }

    #[test]
    fn a_table_with_no_stated_widths_lays_itself_out() {
        let html = table(
            "auto",
            "<table:table><table:table-column/>\
             <table:table-row><table:table-cell><text:p>a</text:p></table:table-cell>\
             </table:table-row></table:table>",
        );
        // A fixed layout with no widths splits the table into equal columns, which
        // is a geometry no document asked for.
        assert!(html.contains("class=\"of-tbl of-tbl-auto\""), "{html}");
        assert!(!html.contains("<colgroup>"), "{html}");
    }

    #[test]
    fn spans_become_colspan_and_rowspan_and_covered_slots_are_dropped() {
        let html = table(
            "spans",
            "<table:table>\
             <table:table-column table:style-name=\"co1\"/>\
             <table:table-column table:style-name=\"co1\"/>\
             <table:table-row>\
             <table:table-cell table:number-columns-spanned=\"2\" \
             table:number-rows-spanned=\"2\"><text:p>wide</text:p></table:table-cell>\
             <table:covered-table-cell/></table:table-row>\
             <table:table-row><table:covered-table-cell/><table:covered-table-cell/>\
             </table:table-row></table:table>",
        );
        assert!(html.contains("colspan=\"2\""), "{html}");
        assert!(html.contains("rowspan=\"2\""), "{html}");
        // A covered slot is described by the span that covers it: an extra `<td>`
        // beside a `rowspan` would push the row one column right.
        assert_eq!(html.matches("<td").count(), 1, "{html}");
        // The second row still exists, holding nothing.
        assert_eq!(html.matches("<tr").count(), 2, "{html}");
    }

    #[test]
    fn a_run_of_header_rows_is_one_thead_and_the_body_follows_it() {
        let html = table(
            "thead2",
            "<table:table><table:table-column table:style-name=\"co1\"/>\
             <table:table-header-rows>\
             <table:table-row><table:table-cell><text:p>h1</text:p></table:table-cell>\
             </table:table-row>\
             <table:table-row><table:table-cell><text:p>h2</text:p></table:table-cell>\
             </table:table-row></table:table-header-rows>\
             <table:table-row><table:table-cell><text:p>naïve</text:p></table:table-cell>\
             </table:table-row></table:table>",
        );
        // One `<thead>` for the whole run of header rows, not one each.
        assert_eq!(html.matches("<thead>").count(), 1, "{html}");
        assert_eq!(html.matches("</thead>").count(), 1, "{html}");
        assert_eq!(html.matches("<tr").count(), 3, "{html}");
        // The header run closes before the ordinary row opens.
        let head_end = html.find("</thead>").expect("a thead");
        assert!(html.find("naïve").expect("the last row") > head_end, "{html}");
    }

    #[test]
    fn repetition_is_expanded_under_a_cap() {
        let html = table(
            "repeat",
            "<table:table>\
             <table:table-column table:style-name=\"co1\" table:number-columns-repeated=\"3\"/>\
             <table:table-row table:number-rows-repeated=\"2\">\
             <table:table-cell table:number-columns-repeated=\"3\"><text:p>x</text:p>\
             </table:table-cell></table:table-row></table:table>",
        );
        assert_eq!(html.matches("<col ").count(), 3, "{html}");
        assert_eq!(html.matches("<tr").count(), 2, "{html}");
        assert_eq!(html.matches("<td").count(), 6, "{html}");

        // An ODS-sized repeat is the format's way of spelling "to the end of the
        // sheet", and it must not become 16k cells.
        let wide = table(
            "repeat2",
            "<table:table>\
             <table:table-column table:style-name=\"co1\" \
             table:number-columns-repeated=\"16368\"/>\
             <table:table-row><table:table-cell table:number-columns-repeated=\"16368\">\
             <text:p>x</text:p></table:table-cell></table:table-row></table:table>",
        );
        assert_eq!(wide.matches("<col ").count(), MAX_COLS, "capped");
        assert_eq!(wide.matches("<td").count(), MAX_COLS, "capped");
    }

    #[test]
    fn a_cell_carries_its_own_paint_and_the_common_one_carries_none() {
        let html = table(
            "cells",
            "<table:table><table:table-column table:style-name=\"co1\"/>\
             <table:table-row table:style-name=\"ro1\">\
             <table:table-cell table:style-name=\"ce1\"><text:p>painted</text:p>\
             </table:table-cell></table:table-row>\
             <table:table-row><table:table-cell><text:p>plain</text:p></table:table-cell>\
             </table:table-row></table:table>",
        );
        assert!(html.contains("background-color:#eeeeee"), "{html}");
        assert!(html.contains("solid #808080"), "{html}");
        // Longhands, so an unstated side keeps the stylesheet's padding instead of
        // being reset by a shorthand.
        assert!(html.contains("padding-left:8px"), "{html}");
        assert!(html.contains("vertical-align:middle"), "{html}");
        assert!(html.contains("height:48px"), "{html}");
        // The common cell carries the class and no `style` attribute at all: its
        // padding and borders are the stylesheet's.
        assert!(html.contains("<td class=\"of-tc\"><p"), "{html}");
    }

    #[test]
    fn a_table_the_document_hides_is_not_drawn_at_all() {
        let extra = "<style:style style:name=\"hidden\" style:family=\"table\">\
             <style:table-properties table:display=\"false\"/></style:style>";
        let html = odt(
            "hidden",
            &styles(&format!("{}{extra}", table_styles())),
            &content(
                "",
                "<table:table table:style-name=\"hidden\">\
                 <table:table-row><table:table-cell><text:p>secret</text:p>\
                 </table:table-cell></table:table-row></table:table>",
            ),
        )
        .html();
        // Not an empty `<table>`: it would still take the stylesheet's borders.
        assert!(!html.contains("<table"), "{html}");
        assert!(!html.contains("secret"), "{html}");
    }

    #[test]
    fn a_table_stretched_to_the_margins_fills_the_column() {
        let html = table(
            "margins",
            "<table:table table:style-name=\"ta1\">\
             <table:table-column table:style-name=\"co1\"/>\
             <table:table-row><table:table-cell><text:p>a</text:p></table:table-cell>\
             </table:table-row></table:table>",
        );
        assert!(html.contains("width:100%"), "{html}");
    }

    #[test]
    fn a_cell_holds_blocks_rather_than_a_string() {
        let list = "<text:list-style style:name=\"L1\">\
             <text:list-level-style-number text:level=\"1\" style:num-format=\"1\" \
             style:num-suffix=\".\"/></text:list-style>";
        let html = odt(
            "cellblocks",
            &styles(&format!("{}{list}", table_styles())),
            &content(
                "",
                "<table:table><table:table-column table:style-name=\"co1\"/>\
                 <table:table-row><table:table-cell>\
                 <text:h text:outline-level=\"2\">head</text:h>\
                 <text:list text:style-name=\"L1\"><text:list-item><text:p>item</text:p>\
                 </text:list-item></text:list>\
                 <table:table><table:table-column table:style-name=\"co1\"/>\
                 <table:table-row><table:table-cell><text:p>nested</text:p>\
                 </table:table-cell></table:table-row></table:table>\
                 </table:table-cell></table:table-row></table:table>",
            ),
        )
        .html();
        // Paragraphs, headings, lists and tables — the same walk the page runs.
        assert!(html.contains("<h2 "), "{html}");
        assert!(html.contains("class=\"of-bu\""), "{html}");
        assert_eq!(html.matches("<table").count(), 2, "{html}");
        assert!(html.contains("nested"), "{html}");
    }

    #[test]
    fn tables_nested_past_the_cap_are_refused_with_a_note() {
        // Four deep: the outer three draw, the innermost does not.
        let inner = "<table:table><table:table-row><table:table-cell><text:p>deepest</text:p>\
             </table:table-cell></table:table-row></table:table>";
        let mut tbl = inner.to_string();
        for _ in 0..3 {
            tbl = format!(
                "<table:table><table:table-row><table:table-cell>{tbl}</table:table-cell>\
                 </table:table-row></table:table>"
            );
        }
        let doc = odt("nest", &styles(&table_styles()), &content("", &tbl)).doc();
        assert_eq!(doc.html.matches("<table").count(), MAX_NEST, "{}", doc.html);
        assert!(!doc.html.contains("deepest"), "{}", doc.html);
        assert!(
            doc.notes.iter().any(|n| n == "Deeply nested table not shown"),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn a_table_past_the_row_cap_is_cut_short_and_says_so() {
        let rows: String = (0..MAX_ROWS + 10)
            .map(|i| {
                format!(
                    "<table:table-row><table:table-cell><text:p>r{i}</text:p>\
                     </table:table-cell></table:table-row>"
                )
            })
            .collect();
        let doc = body(
            "rowcap",
            &format!("<table:table><table:table-column/>{rows}</table:table>"),
        )
        .doc();
        assert_eq!(doc.html.matches("<tr").count(), MAX_ROWS, "{:?}", doc.notes);
        assert!(
            doc.notes.iter().any(|n| n == NOTE_CLIPPED),
            "{:?}",
            doc.notes
        );
    }
}
