//! Table-cell vocabulary shared by the office renderers: border styles and
//! alignment, as OOXML names them, mapped to CSS.
//!
//! SpreadsheetML `ST_BorderStyle` and WordprocessingML `w:tblBorders`/`w:pBdr`
//! use the same style names, and a docx table cell needs the same alignment
//! answers as a spreadsheet cell. Nothing here reads XML — callers pull the
//! values out of their own markup and hand them over, so the mapping stays one
//! copy no matter how many elements spell it.

use super::html::{fmt_deg, fmt_px, Style};

/// (width, style) for an `ST_BorderStyle`, or `None` for `none`.
pub fn border_css(style: &str) -> Option<(&'static str, &'static str)> {
    Some(match style {
        "none" => return None,
        // Hair is thinner than one device pixel; 1px is the thinnest CSS can draw.
        "hair" | "thin" => ("1px", "solid"),
        "medium" => ("2px", "solid"),
        "thick" => ("3px", "solid"),
        "double" => ("3px", "double"),
        "dotted" => ("1px", "dotted"),
        "dashed" | "dashDot" | "dashDotDot" | "slantDashDot" => ("1px", "dashed"),
        "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" => ("2px", "dashed"),
        _ => ("1px", "solid"),
    })
}

// ── alignment ────────────────────────────────────────────────────────────────

/// Text direction inside a cell, as an angle rather than any one format's
/// encoding of it (Excel's `textRotation`, docx's `w:textDirection`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rotation {
    /// Counter-clockwise degrees; negative turns clockwise.
    Ccw(f32),
    /// Glyphs upright, stacked down the cell.
    Stacked,
}

/// What a cell states about alignment, already read out of its own markup.
///
/// `horizontal` and `vertical` take the SpreadsheetML value vocabulary; a format
/// that spells the same intent differently (docx `w:jc` has `both`, `start`,
/// `end`) normalises on the way in rather than growing an arm here.
#[derive(Default)]
pub struct AlignSpec<'a> {
    pub horizontal: &'a str,
    pub vertical: Option<&'a str>,
    pub wrap: bool,
    /// Indent from the aligned edge, in px. Converting a format's own unit
    /// (Excel indent levels, docx twentieths of a point) is the caller's job.
    pub indent_px: f32,
    pub rotation: Option<Rotation>,
}

/// Alignment splits across two elements: everything that a `display:table-cell`
/// honours goes on the cell, and the rotation goes on an inner span.
#[derive(Default, Clone)]
pub struct Align {
    pub cell: String,
    pub inner: String,
}

pub fn align_css(spec: &AlignSpec) -> Align {
    let mut s = Style::new();
    let mut inner = Style::new();
    match spec.horizontal {
        // `general` is deliberately absent: it means "right for numbers, left for
        // text", which is per *cell value*, not per style, so the sheet's
        // `td.xl-num` / `td.xl-txt` classes carry it.
        "general" => {}
        "left" | "fill" => s.push("text-align", "left"),
        "center" | "centerContinuous" => s.push("text-align", "center"),
        "right" => s.push("text-align", "right"),
        "justify" | "distributed" => s.push("text-align", "justify"),
        _ => {}
    }
    match spec.vertical {
        Some("top") => s.push("vertical-align", "top"),
        Some("center") => s.push("vertical-align", "middle"),
        Some("bottom") => s.push("vertical-align", "bottom"),
        // justify/distributed spread lines over the cell height, which CSS table
        // cells cannot do; top is the closest single-value approximation.
        Some("justify") | Some("distributed") => s.push("vertical-align", "top"),
        _ => {}
    }
    if spec.wrap {
        // pre-wrap, not normal: runs of spaces inside a cell are content.
        s.push("white-space", "pre-wrap");
        s.push("overflow-wrap", "break-word");
    }
    if spec.indent_px > 0.0 {
        // Indent runs from whichever edge the text is aligned to.
        if spec.horizontal == "right" {
            s.push_opt("padding-right", fmt_px(spec.indent_px));
        } else {
            s.push_opt("padding-left", fmt_px(spec.indent_px));
        }
    }
    // The rotated box is not re-measured, so a rotated label can overflow its row
    // height — Excel grows the row instead.
    match spec.rotation {
        // writing-mode does apply to a table cell, so stacked text needs no
        // wrapper.
        Some(Rotation::Stacked) => {
            s.push("writing-mode", "vertical-rl");
            s.push("text-orientation", "upright");
        }
        Some(Rotation::Ccw(deg)) if deg != 0.0 => {
            inner.push("display", "inline-block");
            inner.push_opt("transform", fmt_deg(-deg).map(|d| format!("rotate({d})")));
        }
        _ => {}
    }
    Align {
        cell: s.css().to_string(),
        inner: inner.css().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_styles_map_to_widths_and_dash_patterns() {
        assert_eq!(border_css("none"), None);
        assert_eq!(border_css("thin"), Some(("1px", "solid")));
        assert_eq!(border_css("double"), Some(("3px", "double")));
        assert_eq!(border_css("mediumDashed"), Some(("2px", "dashed")));
        // An unknown style is still a border.
        assert_eq!(border_css("wobbly"), Some(("1px", "solid")));
    }

    #[test]
    fn alignment_splits_across_the_cell_and_an_inner_span() {
        let a = align_css(&AlignSpec {
            horizontal: "center",
            vertical: Some("top"),
            wrap: true,
            indent_px: 18.0,
            rotation: Some(Rotation::Ccw(45.0)),
        });
        assert!(a.cell.contains("text-align:center;"), "{}", a.cell);
        assert!(a.cell.contains("vertical-align:top;"), "{}", a.cell);
        assert!(a.cell.contains("white-space:pre-wrap;"), "{}", a.cell);
        assert!(a.cell.contains("padding-left:18px;"), "{}", a.cell);
        // Transforms do not apply to a table cell, so the angle lands on the span.
        assert!(!a.cell.contains("transform"), "{}", a.cell);
        assert_eq!(a.inner, "display:inline-block;transform:rotate(-45deg);");

        // Clockwise, and the indent following a right alignment.
        let a = align_css(&AlignSpec {
            horizontal: "right",
            indent_px: 9.0,
            rotation: Some(Rotation::Ccw(-30.0)),
            ..Default::default()
        });
        assert!(a.cell.contains("padding-right:9px;"), "{}", a.cell);
        assert!(a.inner.contains("rotate(30deg);"), "{}", a.inner);

        // Stacked text needs no wrapper.
        let a = align_css(&AlignSpec {
            rotation: Some(Rotation::Stacked),
            ..Default::default()
        });
        assert_eq!(a.cell, "writing-mode:vertical-rl;text-orientation:upright;");
        assert!(a.inner.is_empty());

        // `general` states nothing: the value's own class carries the alignment.
        let a = align_css(&AlignSpec {
            horizontal: "general",
            ..Default::default()
        });
        assert!(a.cell.is_empty() && a.inner.is_empty());
    }
}
