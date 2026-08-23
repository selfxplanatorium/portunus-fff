//! The format-neutral spreadsheet grid model.
//!
//! Between "parse a spreadsheet's markup" and "write the table" sits one
//! vocabulary: tracks carrying a size in pixels, and cells carrying a *resolved*
//! display string plus the presentation painted around it. A SpreadsheetML `<c>`
//! and an ODF `table:table-cell` disagree about how a value is stored, where its
//! number format lives and how a merge is spelled, and agree about all of this —
//! so turning a stored value into a shown one stays with the format, and
//! emitting the grid is written once.
//!
//! Unlike the paragraph model in [`super::model`], the HTML is not here. A
//! paragraph paints itself; a cell is one `<td>` inside a table whose gutter,
//! colgroup, sticky panes and deduplicated class tables belong to the sheet
//! renderer, which is [`super::sheet`]. What lives here is what an adapter has to
//! fill in — and none of it names a format.

use super::model::Align;

/// One row or column of the grid.
///
/// Columns are indexed 0-based; rows are 1-based, matching the row numbers a
/// sheet shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Track {
    /// Size along the track's own axis — a column's width, a row's height — in
    /// px, already collapsed to the whole pixel the table will lay out.
    /// Fractional tracks are not reproducible, and they drift an overlay drawn
    /// from these offsets off the grid.
    pub px: f32,
    /// Not emitted at all: the track takes up no space and its cells produce no
    /// element.
    pub hidden: bool,
    /// Style for the cells of this track that state none of their own — a
    /// column's default format, a row's. See [`Cell::style`].
    pub style: Option<u32>,
}

impl Track {
    pub fn new(px: f32) -> Track {
        Track {
            px,
            hidden: false,
            style: None,
        }
    }
}

/// One cell of the grid, as it will be shown: the string the reader sees, plus
/// the presentation painted around it.
///
/// Nothing here says how the value was stored. Resolving a string out of a
/// shared pool, running a serial through a number format to get a date back, and
/// clipping the result to a length a grid cell could show at all are the
/// parser's problem; what arrives is the outcome.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    /// The displayed string, already formatted and clipped. Empty is a blank
    /// cell, which still gets an element: positions in a row are what keep the
    /// grid aligned with its headers.
    pub text: String,
    /// The "General" alignment: right for numbers, centre for booleans and
    /// errors, the table's own default for text. It follows the *value*, which
    /// is why it cannot live in the shared style rule.
    pub align: Option<Align>,
    /// Identity of the shared style rule this cell draws with, or `None` when
    /// its style asks for nothing worth a class. The renderer turns it into a
    /// class name and collects the set it has to emit rules for, so only
    /// equality matters — the numbering is the format's own.
    pub style: Option<u32>,
    /// The style needs a wrapper element inside the cell. A CSS transform does
    /// not apply to `display:table-cell`, so rotated text needs one — and only
    /// rotated cells pay for it.
    pub inner: bool,
    /// A text colour the *value* asks for, e.g. the `[Red]` of a negative number
    /// format. Value-dependent, so again not part of the style rule. It reaches
    /// the output as a CSS declaration, so the renderer validates it before
    /// emitting.
    pub color: Option<String>,
}

/// Where a renderer gets the grid's cells from — the one thing a dialect has to
/// implement, and the only place a format's markup is still in reach.
///
/// Cells are pulled one at a time rather than handed over a row at a time because
/// resolving one is not free: a number format to run, a string to clip, a note to
/// add about a broken string pool. The renderer knows which cells it will never
/// emit — a hidden column, a cell some merge covers, everything past the point the
/// output cap stops the grid — and asking for those would both cost the work and
/// report their damage.
pub trait CellSource {
    /// Position the source on row `r`. Called once per emitted row before any
    /// [`cell`](Self::cell) of it, with `r` increasing.
    fn row(&mut self, r: u32);
    /// The cell at column `c` of the row last positioned on.
    fn cell(&mut self, c: usize) -> Cell;
}

/// A merged rectangle: rows 1-based inclusive, columns 0-based inclusive,
/// already clamped to the emitted grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Merge {
    pub r0: u32,
    pub r1: u32,
    pub c0: usize,
    pub c1: usize,
    /// Where the span is actually emitted: the first *visible* row and column of
    /// the range. Usually `(r0, c0)`, but a merge can be anchored in a hidden row
    /// or column, and that cell is never emitted — see [`resolve_anchors`].
    pub ar: u32,
    pub ac: usize,
}

/// Move each merge's anchor to the first visible row/column of its range, and drop
/// merges with nothing visible at all.
///
/// The row loop emits a span at the anchor and suppresses every other cell of the
/// range. If the stored anchor sits in a hidden row or column that cell is never
/// emitted, so the row is one cell short of its headers and everything after it
/// slides left — for a merge hidden at the top of a sheet, that is the whole grid.
pub fn resolve_anchors(merges: &mut Vec<Merge>, rows: &[Track], cols: &[Track]) {
    merges.retain_mut(|m| {
        let Some(ar) = (m.r0..=m.r1).find(|r| !rows[*r as usize].hidden) else {
            return false;
        };
        let Some(ac) = (m.c0..=m.c1).find(|c| !cols[*c].hidden) else {
            return false;
        };
        m.ar = ar;
        m.ac = ac;
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(hidden: &[bool]) -> Vec<Track> {
        hidden
            .iter()
            .map(|h| Track {
                hidden: *h,
                ..Track::new(20.0)
            })
            .collect()
    }

    #[test]
    fn an_anchor_in_a_hidden_track_moves_to_the_first_visible_one() {
        // Row 1 and column 0 are hidden, so `A1:B3` is emitted at B2.
        let rows = tracks(&[false, true, false, false]);
        let cols = tracks(&[true, false]);
        let mut merges = vec![Merge {
            r0: 1,
            r1: 3,
            c0: 0,
            c1: 1,
            ar: 1,
            ac: 0,
        }];
        resolve_anchors(&mut merges, &rows, &cols);
        assert_eq!((merges[0].ar, merges[0].ac), (2, 1));
    }

    #[test]
    fn a_merge_with_nothing_visible_is_dropped() {
        let rows = tracks(&[false, true, true]);
        let cols = tracks(&[false, false]);
        let mut merges = vec![Merge {
            r0: 1,
            r1: 2,
            c0: 0,
            c1: 1,
            ar: 1,
            ac: 0,
        }];
        resolve_anchors(&mut merges, &rows, &cols);
        assert!(merges.is_empty());
    }
}
