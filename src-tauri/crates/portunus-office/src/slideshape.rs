//! Slide-shape scaffolding: the canvas stylesheet, its size bounds, and the
//! error card — shared by the PresentationML and ODF slide renderers.
//!
//! The sibling of [`super::docshape`], and there for the same reason: a slide is a
//! fixed-size canvas of absolutely positioned boxes whichever dialect described it,
//! the frontend's `slide` variant scales that canvas with one transform, and the
//! frame's selection engine keys off `.pp-tb` and `.pp-doc` by name. Two copies of
//! this stylesheet would be two things to keep in step with `srcdoc.ts`.
//!
//! The `pp-` prefix predates the second caller and is deliberately kept: it is a
//! published contract with the frontend (`SELECTORS`) and with the selection
//! overlay, so renaming it would be a change to three files for no gain.

use super::emit;
use super::html::{fmt_px, Style};

/// Canvas used when the document states no usable slide size: 4:3 at 96dpi.
pub const DEFAULT_SLIDE: (f32, f32) = (960.0, 720.0);
pub const MIN_SLIDE_PX: f32 = 64.0;
pub const MAX_SLIDE_PX: f32 = 8192.0;

/// Structural stylesheet. Every selector is at most one type plus one class, so
/// the per-shape inline styles that carry the document's own geometry and paint
/// always win.
pub const BASE_CSS: &str = "\
.pp-doc{position:relative;overflow:hidden;background:#fff;color:#000;\
font-family:Carlito,Lato,sans-serif;font-size:18px;line-height:1.2;}
.pp-sp{position:absolute;box-sizing:border-box;}
/* The text box is where a drag selects rather than pans; see the frame's
   selection engine and the `slide` entry of its selector table. */
.pp-tb{position:absolute;display:flex;flex-direction:column;cursor:text;}
.pp-tbi{width:100%;}
.pp-p{margin:0;white-space:pre-wrap;overflow-wrap:break-word;}
/* A bulleted paragraph is a flex row: bullet, then the runs as a block that wraps
   at its own left edge. See the hanging-indent note in text.rs. */
.pp-li{display:flex;align-items:baseline;}
.pp-bu{display:inline-block;flex:none;white-space:pre;}
.pp-tx{flex:1 1 auto;min-width:0;}
.pp-img{position:absolute;display:block;}
.pp-bg{position:absolute;left:0;top:0;width:100%;height:100%;}
.pp-ph{position:absolute;display:flex;align-items:center;justify-content:center;\
box-sizing:border-box;border:1px dashed #b0b0b0;background:#f7f7f7;color:#6b6b6b;\
font-size:11px;text-align:center;}
.pp-tbl{border-collapse:collapse;table-layout:fixed;width:100%;height:100%;font-size:14px;}
.pp-tbl td{padding:3px 6px;overflow:hidden;cursor:text;}
.office-note{color:var(--fg-mute,#6b6b6b);font-size:11px;padding:6px 2px;}
";

/// A canvas-sized box, for the inline style that carries the slide's own extent.
pub fn canvas_css(natural: (f32, f32)) -> Style {
    let mut css = Style::new();
    css.push_opt("width", fmt_px(natural.0));
    css.push_opt("height", fmt_px(natural.1));
    css
}

/// The card a slide that could not be rendered leaves at the right size, so the
/// deck's geometry and its section strip still agree with the rest of the deck.
pub fn error_body(msg: &str, natural: (f32, f32)) -> String {
    let css = canvas_css(natural);
    emit::error_doc(BASE_CSS, "pp-doc", css.css(), "office-note", msg)
}

/// Whether a stated axis is a slide dimension rather than corrupt geometry.
pub fn in_range(v: f32) -> bool {
    v.is_finite() && (MIN_SLIDE_PX..=MAX_SLIDE_PX).contains(&v)
}
