//! Neutral grid emission shared by the SpreadsheetML and ODF sheet renderers.
//!
//! [`super::sheetmodel`] fixes the vocabulary a spreadsheet is described in; this
//! is the half that consumes it. A dialect parses its own markup into [`Track`]s,
//! a [`Merge`] list and a [`CellSource`], calls [`build_geometry`], and from there
//! the frozen panes, the gutter, the colgroup, the row loop and the
//! deduplicated class tables are written once.
//!
//! The seam sits exactly where the last piece of markup is consumed. The only
//! thing emission still has to ask a format about is its style table, hence
//! [`StyleTable`] — one method, because there is one question.
//!
//! The `.xl-*` class names live here rather than per dialect because they are a
//! frontend contract (`srcdoc.ts` `SELECTORS`, `frameSelection.ts`): two sheet
//! renderers emitting two sets of selectors is exactly the drift this module
//! exists to prevent.

use super::emit;
use super::highlight::{Marker, Terms};
use super::html::{attr, attrs, Writer};
use super::model::Align;
use super::sheetmodel::{CellSource, Merge, Track};
use std::collections::BTreeSet;

/// Every frozen cell is positioned and transformed to stay pinned, so the depth of
/// the split is bounded. Tracks past this one are not pinned (and nothing else
/// about them changes).
const MAX_FROZEN: usize = 16;

/// Structural stylesheet.
///
/// Every selector here is at most one type + one class, so the per-`xf` rules
/// (`td.xfN`, appended after this block) tie on specificity and win on source
/// order. A `.xl-doc table.xl-sheet td` style rule would quietly beat every
/// document-authored border and fill.
pub const BASE_CSS: &str = "\
.xl-doc{font-family:Carlito,Lato,sans-serif;font-size:14.6667px;line-height:1.2;color:#000;}
.xl-grid{position:relative;display:inline-block;}
.xl-sheet{border-collapse:collapse;table-layout:fixed;background:#fff;border-spacing:0;}
.xl-sheet td{padding:0 3px;vertical-align:bottom;white-space:pre;overflow:hidden;}
.xl-lines td{border:1px solid #dcdcdc;}
td.xl-num{text-align:right;}
td.xl-bool,td.xl-err{text-align:center;}
/* Cells with text take the I-beam; everything else keeps body's grab cursor,
   because everything else pans. See the `xl-t` note in the cell loop. */
td.xl-t{cursor:text;}
th.xl-ch,th.xl-rh,th.xl-corner{background:var(--bg-card,#ececec);color:var(--fg,#1f1f1f);\
border:1px solid var(--border,#c0c0c0);font-weight:500;font-size:11px;letter-spacing:.02em;\
padding:0 4px;text-align:center;vertical-align:middle;position:relative;}
th.xl-rh{text-align:right;}
th.xl-corner{width:46px;}
/* The gutter's colour is painted once behind the whole strip, on the column and
   the header row group, not only on the cells. Cell boxes are scaled by the
   reader's zoom, so adjacent edges land on fractional device pixels and the seam
   between two rows shows whatever is underneath — which for the row gutter is the
   sheet's paper white, i.e. a flickering hairline that moves as you zoom. Table
   paint order is table, columns, row groups, rows, cells, so a column background
   sits directly under the cells with no seams of its own. */
col.xl-cg,.xl-sheet thead{background:var(--bg-card,#ececec);}
.xl-drawings{position:absolute;left:46px;top:21px;width:0;height:0;pointer-events:none;}
.xl-dw{position:absolute;}
.xl-ph{position:absolute;display:flex;align-items:center;justify-content:center;\
border:1px dashed #b0b0b0;background:#f7f7f7;color:#6b6b6b;font-size:11px;text-align:center;}
.xl-empty{color:var(--fg-mute,#6b6b6b);padding:8px 2px;}
.office-note{color:var(--fg-mute,#6b6b6b);font-size:11px;padding:6px 2px;}
.xl-cg{width:46px;}
td.xl-fz{background:#fff;}
/* Frozen panes pin by counter-translating, not by `position:sticky`: the reader
   positions the document with one transform and nothing here scrolls, so there is
   no scrollport for sticky to stick to (see the scaffold note in srcdoc.ts). A
   frozen track's sticky offset was always its own natural position in the grid —
   they accumulate from the gutter in document order — so the pin is the same pair
   of numbers for every frozen cell: how far the content has been panned past the
   grid's origin, which the frame writes as --fx/--fy on this table. `relative` is
   still needed for the z-index, and separate borders because a positioned cell in
   a collapsed table is not reliably painted. */
.xl-frozen{border-collapse:separate;}
.xl-frozen .xl-fzr,.xl-frozen .xl-fzc{position:relative;z-index:2;}
.xl-frozen .xl-fzc{transform:translateX(var(--fx,0px));}
.xl-frozen .xl-fzr{transform:translateY(var(--fy,0px));}
.xl-frozen .xl-fzr.xl-fzc{transform:translate(var(--fx,0px),var(--fy,0px));z-index:3;}
.xl-frozen th.xl-fzr,.xl-frozen th.xl-fzc{z-index:4;}
";

/// The sheet's style table, as emission sees it.
///
/// One method, because emission asks the table exactly one question: the rules for
/// the styles some emitted cell referenced. Every other decision a style feeds —
/// whether an id is worth a class at all, whether it needs the inner wrapper,
/// whether it inks an empty cell — is resolved by the dialect on its way to a
/// [`super::sheetmodel::Cell`], so it never crosses this seam.
///
/// Object-safe, matching [`CellSource`]: a renderer hands the emitter its two
/// format-specific halves the same way for both.
pub trait StyleTable {
    /// Rules for the styles `used` names. Emitted last, so they beat the base
    /// stylesheet's gridline and alignment rules on source order.
    ///
    /// Takes the set by reference rather than as an `impl IntoIterator`, which
    /// would not be object-safe.
    fn css_block(&self, used: &BTreeSet<u32>) -> String;
}

/// Deduplicated numeric class values (`width`, `height`): most columns of a sheet
/// share one width, so one rule per distinct value keeps the stylesheet small.
#[derive(Default)]
struct ClassMap {
    values: Vec<i32>,
}

impl ClassMap {
    /// Class ordinal for `px`, rounded to whole pixels (sub-pixel table tracks
    /// are not reproducible anyway).
    fn id(&mut self, px: f32) -> usize {
        let key = px.round() as i32;
        match self.values.iter().position(|v| *v == key) {
            Some(i) => i,
            None => {
                self.values.push(key);
                self.values.len() - 1
            }
        }
    }

    fn rules(&self, prefix: &str, prop: &str) -> String {
        let mut out = String::new();
        for (i, v) in self.values.iter().enumerate() {
            out.push('.');
            out.push_str(prefix);
            out.push_str(&i.to_string());
            out.push('{');
            out.push_str(prop);
            out.push(':');
            out.push_str(&v.to_string());
            out.push_str("px;}\n");
        }
        out
    }
}

/// Pixel offsets of the emitted grid, for placing a drawing overlay.
///
/// The overlay itself is a dialect's own phase — it resolves relationships and
/// reads media — but where it puts a shape is decided by the grid it sits over.
pub struct Grid {
    /// Left edge of each column, plus one past the last. Hidden columns are zero
    /// width here, exactly as they are in the table.
    col_left: Vec<f32>,
    /// Top edge of each row *number* (index 0 unused), plus one past the last.
    row_top: Vec<f32>,
}

impl Grid {
    pub fn x(&self, col: usize) -> f32 {
        *self
            .col_left
            .get(col)
            .unwrap_or_else(|| self.col_left.last().unwrap_or(&0.0))
    }

    pub fn y(&self, row_num: u32) -> f32 {
        *self
            .row_top
            .get(row_num as usize)
            .unwrap_or_else(|| self.row_top.last().unwrap_or(&0.0))
    }
}

/// Frozen pane depth in track counts, already clamped to `MAX_FROZEN` and to the
/// emitted grid.
#[derive(Clone, Copy)]
pub struct Frozen {
    rows: usize,
    cols: usize,
}

impl Frozen {
    /// Frozen panes are the only reason to switch the table to separate borders
    /// (a positioned cell is not reliably painted under
    /// `border-collapse:collapse`), and that changes how adjacent borders paint —
    /// so it is opt-in per document.
    ///
    /// `rows`/`cols` are the split as the document stored it; where that is stored
    /// is the dialect's business (a `<pane>` in SpreadsheetML, view settings in
    /// ODF), and clamping it to what was actually emitted is not.
    pub fn clamp(rows: usize, cols: usize, nrows: u32, ncols: usize) -> Frozen {
        Frozen {
            rows: rows.min(MAX_FROZEN).min(nrows as usize),
            cols: cols.min(MAX_FROZEN).min(ncols),
        }
    }

    fn on(self) -> bool {
        self.rows > 0 || self.cols > 0
    }
}

/// The emitted grid's shape and size — everything emission needs, with no
/// reference to the markup it was measured from.
///
/// Built by [`build_geometry`]: fill in the tracks, hand the emitter a
/// [`CellSource`], and the whole emission half is reusable.
pub struct Layout {
    nrows: u32,
    ncols: usize,
    /// Indexed by 0-based column, trimmed to `ncols`.
    pub cols: Vec<Track>,
    /// The emitted columns, in order; hidden ones are absent.
    vis_cols: Vec<usize>,
    /// Indexed by 1-based row number.
    pub rows: Vec<Track>,
    pub grid: Grid,
}

/// Class tables filled while emitting and turned into rules afterwards: the
/// stylesheet cannot be written before the grid that references it is walked.
#[derive(Default)]
pub struct Classes {
    widths: ClassMap,
    heights: ClassMap,
    /// Number-format colours, deduplicated in first-seen order.
    num_colors: Vec<String>,
    /// The styles some emitted cell actually referenced.
    used: BTreeSet<u32>,
}

// ── geometry ─────────────────────────────────────────────────────────────────

/// Measured tracks → the emitted grid: which columns survive, and where every
/// track edge lands in pixels.
///
/// `rows` is indexed by 1-based row number, with an unused slot at index 0 and one
/// past the last row so the closing edge can be written without a bounds check.
pub fn build_geometry(mut cols: Vec<Track>, rows: Vec<Track>, nrows: u32, ncols: usize) -> Layout {
    cols.truncate(ncols);
    let vis_cols: Vec<usize> = (0..ncols).filter(|c| !cols[*c].hidden).collect();
    let mut col_left = vec![0.0f32; ncols + 1];
    {
        let mut x = 0.0f32;
        for c in 0..ncols {
            col_left[c] = x;
            if !cols[c].hidden {
                x += cols[c].px;
            }
        }
        col_left[ncols] = x;
    }
    let mut row_top = vec![0.0f32; nrows as usize + 2];
    {
        let mut y = 0.0f32;
        for r in 1..=nrows {
            row_top[r as usize] = y;
            if !rows[r as usize].hidden {
                y += rows[r as usize].px;
            }
        }
        row_top[nrows as usize + 1] = y;
    }
    Layout {
        nrows,
        ncols,
        cols,
        vis_cols,
        rows,
        grid: Grid { col_left, row_top },
    }
}

// ── emission ─────────────────────────────────────────────────────────────────

/// Opens the document wrappers and the table, then writes the `<colgroup>` and
/// the column-letter header row.
///
/// The wrappers stay open: a drawing overlay is absolutely positioned inside
/// `.xl-grid`, so the caller closes them only once its overlay is emitted.
pub fn emit_head(
    w: &mut Writer,
    layout: &Layout,
    frozen: Frozen,
    show_lines: bool,
    classes: &mut Classes,
) {
    w.open("div", &attr("class", "xl-doc"));
    w.open("div", &attr("class", "xl-grid"));
    let table_class = match (show_lines, frozen.on()) {
        (true, true) => "xl-sheet xl-lines xl-frozen",
        (true, false) => "xl-sheet xl-lines",
        (false, true) => "xl-sheet xl-frozen",
        (false, false) => "xl-sheet",
    };
    w.open("table", &attr("class", table_class));

    w.open("colgroup", "");
    w.void("col", &attr("class", "xl-cg"));
    for &c in &layout.vis_cols {
        let id = classes.widths.id(layout.cols[c].px);
        w.void("col", &attr("class", &format!("xw{id}")));
    }
    w.close();

    w.open("thead", "");
    w.open("tr", "");
    let corner = if frozen.on() {
        "xl-corner xl-fzr xl-fzc"
    } else {
        "xl-corner"
    };
    w.open("th", &attr("class", corner));
    w.close();
    for &c in &layout.vis_cols {
        let mut cls = String::from("xl-ch");
        if frozen.on() {
            push_class(&mut cls, "xl-fzr");
            if c < frozen.cols {
                push_class(&mut cls, "xl-fzc");
            }
        }
        w.open("th", &attr("class", &cls));
        w.text(&col_letter(c));
        w.close();
    }
    w.close();
    w.close();
}

/// The `<tbody>`: one `<tr>` per visible row, with merge spans resolved and the
/// cells pulled from `src`.
///
/// Every cell arrives already resolved, so nothing here knows what a dialect's
/// cell element is. Do not simplify the pull into "hand over a row of cells":
/// resolving one cell is not free, and this loop is the only thing that knows
/// which cells will never be emitted — a hidden column, a cell some merge covers,
/// everything past the point the writer's cap stops the grid. See the reasoning on
/// [`CellSource`].
pub fn emit_rows(
    w: &mut Writer,
    layout: &Layout,
    merges: &[Merge],
    src: &mut dyn CellSource,
    frozen: Frozen,
    hl: &mut Marker,
    terms: &Terms,
    classes: &mut Classes,
) {
    let nrows = layout.nrows;
    let ncols = layout.ncols;
    let cols = &layout.cols;
    let vis_cols = &layout.vis_cols;
    let rows = &layout.rows;

    w.open("tbody", "");
    // Reused per row so a wide sheet does not reallocate these every row.
    let mut covered = vec![false; ncols];
    let mut spans: Vec<Option<(u32, usize)>> = vec![None; ncols];

    'rows: for r in 1..=nrows {
        if rows[r as usize].hidden {
            continue;
        }
        // Merge bookkeeping for this row: which columns a span already covers, and
        // where a span starts. Rebuilt per row so nothing has to materialize a
        // covered-cell set for a merge that runs the height of the sheet.
        covered.iter_mut().for_each(|v| *v = false);
        spans.iter_mut().for_each(|v| *v = None);
        for m in merges {
            if r < m.r0 || r > m.r1 {
                continue;
            }
            for c in m.c0..=m.c1.min(ncols - 1) {
                if r == m.ar && c == m.ac {
                    // Spans count *emitted* tracks only: hidden rows/columns are
                    // not in the table, so counting them would push the rest of the
                    // row sideways.
                    let rs = (m.r0..=m.r1)
                        .filter(|rr| !rows[*rr as usize].hidden)
                        .count()
                        .max(1);
                    let cs = (m.c0..=m.c1.min(ncols - 1))
                        .filter(|cc| !cols[*cc].hidden)
                        .count()
                        .max(1);
                    spans[c] = Some((rs as u32, cs));
                } else {
                    covered[c] = true;
                }
            }
        }

        src.row(r);

        let hid = classes.heights.id(rows[r as usize].px);
        let row_frozen = frozen.on() && (r as usize) <= frozen.rows;
        w.open("tr", &attr("class", &format!("xh{hid}")));
        let mut rh_cls = String::from("xl-rh");
        if frozen.on() {
            push_class(&mut rh_cls, "xl-fzc");
            if row_frozen {
                push_class(&mut rh_cls, "xl-fzr");
            }
        }
        w.open("th", &attr("class", &rh_cls));
        w.text(&r.to_string());
        w.close();

        for &c in vis_cols {
            if covered[c] {
                continue;
            }
            let cell = src.cell(c);

            let mut cls = String::new();
            if let Some(id) = cell.style {
                classes.used.insert(id);
                cls.push_str("xf");
                cls.push_str(&id.to_string());
            }
            if let Some(k) = align_class(cell.align) {
                push_class(&mut cls, k);
            }
            // The one CSS value that comes from a parsed format code rather than
            // from a fixed table, so it is checked here rather than trusted.
            if let Some(col) = cell.color.filter(|c| is_hex_color(c)) {
                let idx = match classes.num_colors.iter().position(|c| *c == col) {
                    Some(i) => i,
                    None => {
                        classes.num_colors.push(col);
                        classes.num_colors.len() - 1
                    }
                };
                push_class(&mut cls, &format!("xnc{idx}"));
            }
            if frozen.on() && (c < frozen.cols || row_frozen) {
                // Frozen cells need an opaque backdrop so scrolled content does not
                // show through. `td.xl-fz` ties with `td.xfN` on specificity and is
                // emitted first, so a document fill still wins.
                push_class(&mut cls, "xl-fz");
                if c < frozen.cols {
                    push_class(&mut cls, "xl-fzc");
                }
                if row_frozen {
                    push_class(&mut cls, "xl-fzr");
                }
            }

            let (rowspan, colspan) = match spans[c] {
                Some((rs, cols_n)) => (
                    if rs > 1 {
                        attr("rowspan", &rs.to_string())
                    } else {
                        String::new()
                    },
                    if cols_n > 1 {
                        attr("colspan", &cols_n.to_string())
                    } else {
                        String::new()
                    },
                ),
                None => (String::new(), String::new()),
            };
            // Marks the cell as carrying text. The frame's selection engine keys
            // both the I-beam cursor and the select-vs-pan decision off this: a
            // press on a cell with something in it starts a selection, anywhere
            // else starts a pan (the same rule the PDF text layer follows). CSS
            // cannot ask whether an element has text, and walking the DOM per
            // mousemove over a 6000-cell grid is not free, so the renderer says so.
            if !cell.text.is_empty() {
                push_class(&mut cls, "xl-t");
            }
            let class_attr = if cls.is_empty() {
                String::new()
            } else {
                attr("class", &cls)
            };
            w.open("td", &attrs(&[&class_attr, &rowspan, &colspan]));
            if !cell.text.is_empty() {
                if cell.inner {
                    w.open("span", &attr("class", "xr"));
                }
                // `mark` escapes as it wraps matches, so its output is the only
                // document-derived string that may go in raw.
                let marked = hl.mark(&cell.text, terms);
                w.raw(&marked);
                if cell.inner {
                    w.close();
                }
            }
            w.close();
            if w.is_full() {
                break 'rows;
            }
        }
        w.close();
    }
    w.close(); // tbody
}

/// The document stylesheet, assembled once the grid has been walked.
pub fn collect_css(classes: Classes, styles: &dyn StyleTable) -> String {
    let mut css = String::new();
    css.push_str(&classes.widths.rules("xw", "width"));
    css.push_str(&classes.heights.rules("xh", "height"));
    for (i, c) in classes.num_colors.iter().enumerate() {
        css.push_str(&format!("td.xnc{i}{{color:{c};}}\n"));
    }
    // Style rules last: they must beat the base gridline/alignment rules, which
    // they only do on source order.
    css.push_str(&styles.css_block(&classes.used));
    css
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Body for a sheet that has no grid to show — unreadable, empty, or not a sheet
/// at all. A sheet has no intrinsic size, so the canvas needs no style.
pub fn error_body(msg: &str) -> String {
    emit::error_doc(BASE_CSS, "xl-doc", "", "xl-empty", msg)
}

/// 0-based index → the column letters shown in the gutter.
///
/// Here rather than with a dialect's reference parsing: the gutter is part of the
/// emitted grid, and both dialects label it A, B, … AA.
pub fn col_letter(idx: usize) -> String {
    let mut n = idx.saturating_add(1);
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(b'A' + rem as u8);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// The class carrying a cell's own alignment. Left is the table's default and
/// needs no rule, and no *value* asks to be justified.
fn align_class(align: Option<Align>) -> Option<&'static str> {
    match align? {
        Align::Right => Some("xl-num"),
        Align::Center => Some("xl-bool"),
        Align::Left | Align::Justify => None,
    }
}

fn push_class(cls: &mut String, add: &str) {
    if !cls.is_empty() {
        cls.push(' ');
    }
    cls.push_str(add);
}

/// Guards the one CSS value that comes from a parsed format code rather than
/// from a fixed table.
fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::super::sheetmodel::{resolve_anchors, Cell};
    use super::*;

    fn cols_of(specs: &[(f32, bool)]) -> Vec<Track> {
        specs
            .iter()
            .map(|(px, hidden)| Track {
                px: *px,
                hidden: *hidden,
                style: None,
            })
            .collect()
    }

    /// Row tracks the way a dialect builds them: 1-based, with an unused slot at
    /// index 0 and one past the last row.
    fn rows_of(specs: &[(f32, bool)]) -> Vec<Track> {
        let mut rows = vec![Track::new(0.0)];
        rows.extend(cols_of(specs));
        rows.push(Track::new(0.0));
        rows
    }

    /// A grid handed straight to the emitter, so the row loop can be exercised
    /// with no markup in sight.
    struct Fixed {
        /// Rows of cells, 1-based to match the tracks.
        rows: Vec<Vec<Cell>>,
        r: usize,
    }

    impl Fixed {
        fn new(rows: Vec<Vec<Cell>>) -> Fixed {
            Fixed { rows, r: 0 }
        }
    }

    impl CellSource for Fixed {
        fn row(&mut self, r: u32) {
            self.r = r as usize;
        }

        fn cell(&mut self, c: usize) -> Cell {
            self.rows
                .get(self.r.wrapping_sub(1))
                .and_then(|row| row.get(c))
                .cloned()
                .unwrap_or_default()
        }
    }

    fn grid_html(layout: &Layout, merges: &[Merge], src: &mut dyn CellSource) -> String {
        let mut w = Writer::new(1 << 16);
        let mut classes = Classes::default();
        let mut hl = Marker::new();
        emit_rows(
            &mut w,
            layout,
            merges,
            src,
            Frozen { rows: 0, cols: 0 },
            &mut hl,
            &Terms::new(&[]),
            &mut classes,
        );
        w.finish()
    }

    // ── geometry ────────────────────────────────────────────────────────────

    fn three_by_three() -> Layout {
        // Column B and row 2 are hidden.
        let rows = rows_of(&[(20.0, false), (30.0, true), (40.0, false)]);
        build_geometry(cols_of(&[(64.0, false), (100.0, true), (50.0, false)]), rows, 3, 3)
    }

    #[test]
    fn build_geometry_gives_a_hidden_track_no_width() {
        let l = three_by_three();
        assert_eq!(l.vis_cols, vec![0, 2]);
        // B occupies no space, so C starts where B did.
        assert_eq!(l.grid.col_left, vec![0.0, 64.0, 64.0, 114.0]);
        // Row 1 at 0, rows 2 and 3 both at 20 (row 2 is hidden), end at 60.
        assert_eq!(l.grid.row_top, vec![0.0, 0.0, 20.0, 20.0, 60.0]);
        // The column table is trimmed to the emitted extent.
        assert_eq!(l.cols.len(), 3);
    }

    #[test]
    fn grid_lookups_past_the_last_track_clamp_to_the_far_edge() {
        let l = three_by_three();
        assert_eq!(l.grid.x(2), 64.0);
        assert_eq!(l.grid.x(99), 114.0);
        assert_eq!(l.grid.y(3), 20.0);
        assert_eq!(l.grid.y(99), 60.0);
    }

    #[test]
    fn the_frozen_split_is_clamped_to_the_grid_and_to_the_frozen_budget() {
        let f = Frozen::clamp(MAX_FROZEN + 5, 9, 100, 4);
        assert_eq!((f.rows, f.cols), (MAX_FROZEN, 4));
        assert!(f.on());
        assert!(!Frozen { rows: 0, cols: 0 }.on());
    }

    // ── emission ────────────────────────────────────────────────────────────

    #[test]
    fn a_cell_paints_exactly_what_it_asks_for() {
        let layout = build_geometry(
            cols_of(&[(64.0, false), (50.0, false), (40.0, false)]),
            rows_of(&[(20.0, false)]),
            1,
            3,
        );
        let mut src = Fixed::new(vec![vec![
            Cell {
                text: "(1,50)".to_string(),
                align: Some(Align::Right),
                style: Some(7),
                color: Some("#ff0000".to_string()),
                ..Default::default()
            },
            // A rotated cell needs the wrapper; a blank one gets no attributes at
            // all, and still gets its `<td>`.
            Cell {
                text: "café".to_string(),
                inner: true,
                ..Default::default()
            },
            Cell::default(),
        ]]);
        assert_eq!(
            grid_html(&layout, &[], &mut src),
            "<tbody><tr class=\"xh0\"><th class=\"xl-rh\">1</th>\
             <td class=\"xf7 xl-num xnc0 xl-t\">(1,50)</td>\
             <td class=\"xl-t\"><span class=\"xr\">café</span></td>\
             <td></td></tr></tbody>"
        );
    }

    #[test]
    fn a_colour_a_style_asks_for_has_to_be_a_colour() {
        let layout = build_geometry(cols_of(&[(64.0, false)]), rows_of(&[(20.0, false)]), 1, 1);
        let mut src = Fixed::new(vec![vec![Cell {
            text: "x".to_string(),
            color: Some("red;}body{display:none".to_string()),
            ..Default::default()
        }]]);
        let html = grid_html(&layout, &[], &mut src);
        assert!(!html.contains("xnc"), "{html}");
    }

    #[test]
    fn a_span_counts_emitted_tracks_and_suppresses_the_cells_it_covers() {
        // Row 1 and column A are hidden, so `A1:C3` is emitted at B2 and spans the
        // two rows and two columns the table actually has.
        let layout = build_geometry(
            cols_of(&[(64.0, true), (50.0, false), (40.0, false)]),
            rows_of(&[(20.0, true), (20.0, false), (20.0, false)]),
            3,
            3,
        );
        let mut merges = vec![Merge {
            r0: 1,
            r1: 3,
            c0: 0,
            c1: 2,
            ar: 1,
            ac: 0,
        }];
        resolve_anchors(&mut merges, &layout.rows, &layout.cols);
        assert_eq!((merges[0].ar, merges[0].ac), (2, 1));

        let mut src = Fixed::new(vec![
            vec![Cell::default(); 3],
            vec![
                Cell::default(),
                Cell {
                    text: "café".to_string(),
                    ..Default::default()
                },
                Cell::default(),
            ],
            vec![Cell::default(); 3],
        ]);
        assert_eq!(
            grid_html(&layout, &merges, &mut src),
            "<tbody>\
             <tr class=\"xh0\"><th class=\"xl-rh\">2</th>\
             <td class=\"xl-t\" rowspan=\"2\" colspan=\"2\">café</td></tr>\
             <tr class=\"xh0\"><th class=\"xl-rh\">3</th></tr>\
             </tbody>"
        );
    }
}
