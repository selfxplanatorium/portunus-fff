//! Doc-shape scaffolding (page geometry + page CSS) shared by docx and odt.
//!
//! The frontend's doc variant keys its selection engine and its page geometry off
//! `.of-page` (see the contract in `srcdoc.ts`), and that contract is one contract
//! however many word-processor dialects render into it. So the page box, the bounds
//! it is clamped to, and the stylesheet that states it live here — the alternative
//! is two dialects drifting apart on a selector the reader depends on.
//!
//! Parsing a page box is *not* here: `w:sectPr` and ODF's page-layout styles have
//! nothing in common but the six numbers they produce. A dialect measures, fills in
//! a [`Page`], and hands it back.

use super::html::{fmt_px, pt_to_px, Style};

/// US Letter at 96dpi with 1in margins: what a document with no usable page
/// definition gets. The same fallback the frontend's host CSS states, so the two
/// agree when neither has anything to go on.
const DEFAULT_PAGE_W: f32 = 816.0;
const DEFAULT_PAGE_H: f32 = 1056.0;
const DEFAULT_MARGIN: f32 = 96.0;

/// Sane page bounds. Below the minimum there is no column to read; above the
/// maximum the page is wider than any display and the reader can only pan.
pub const MIN_PAGE_PX: f32 = 96.0;
pub const MAX_PAGE_PX: f32 = 4096.0;
/// A margin never eats more than this, nor more than half the paper minus a
/// readable column — a document that states margins wider than its own page would
/// otherwise leave no text column at all.
pub const MAX_MARGIN_PX: f32 = 512.0;
pub const MIN_COLUMN_PX: f32 = 96.0;

/// Structural stylesheet. Every selector is at most one type plus one class, so
/// the per-paragraph inline styles that carry the document's own geometry always
/// win, and so do the document-derived rules appended after this block.
pub const BASE_CSS: &str = "\
.of-page{background:#fff;color:#000;white-space:pre-wrap;overflow-wrap:break-word;}
/* One class for a paragraph and for every heading level: `model::emit_para`
   changes the element and nothing else, so the heading's UA margins and weight
   have to be reset here or they fight the document's own spacing. The weight is
   inherited rather than normal because a heading style states its own boldness on
   the runs, which is where the emitter puts it. */
.of-p{margin:0;font-weight:inherit;}
/* A marked paragraph is a flex row: marker, then the runs as a block that wraps
   at its own left edge. See the hanging-indent note in `model::emit_para`. */
.of-li{display:flex;align-items:baseline;}
.of-bu{display:inline-block;flex:none;white-space:pre;}
.of-tx{flex:1 1 auto;min-width:0;}
/* A page or column break cannot be paginated away in a scrolling column, so it
   is drawn as the boundary it is: a full-width hairline with air around it. The
   `content` declaration is not decoration - WebKit builds a real box for a `br`
   only when its style has content (`HTMLBRElement::createElementRenderer`), and
   without one the border never paints. */
br.of-pb{content:'';display:block;height:0;border-top:1px solid #c9c9c9;margin:16px 0;}
/* A table's columns come from the document's own `w:tblGrid` under a fixed
   layout, because nothing here measures text — see `table.rs`. `max-width` is
   what keeps a table wider than the text column from pushing the page open, and
   it has to sit on the class rather than inline so a document-stated `width`
   still wins. `of-tbl-auto` is the fallback for a table whose grid says nothing:
   a fixed layout with no column widths splits it equally, which is a geometry no
   document asked for. */
table.of-tbl{border-collapse:collapse;table-layout:fixed;max-width:100%;}
table.of-tbl-auto{table-layout:auto;}
/* Word's default cell margins are 0.08in on the leading and trailing edge and
   nothing vertical, and its default vertical alignment is top. Both live here so
   that the common cell needs no `style` attribute at all: a table in a long
   report is thousands of cells, and every repeated declaration is bytes against
   the writer's cap. */
td.of-tc{padding:0 7.2px;vertical-align:top;overflow-wrap:break-word;}
/* An image is a run, so it sits in the line the document put it in. `max-width`
   is the backstop for a box narrower than the text column — a table cell:
   `draw.rs` already scales every extent down to the column, so this only bites
   inside one, and then it is a squeeze rather than an overflowing page. */
img.of-img{max-width:100%;}
/* The box a graphic the preview cannot draw degrades to. Inline-flex, not the
   absolutely positioned block a slide's placeholder is: this one stands in the
   text flow between the runs around it, and a block element inside a `p` would
   split the paragraph. */
.of-gph{display:inline-flex;align-items:center;justify-content:center;\
box-sizing:border-box;vertical-align:bottom;overflow:hidden;max-width:100%;\
border:1px dashed #b0b0b0;background:#f7f7f7;color:#6b6b6b;font-size:11px;\
text-align:center;white-space:normal;}
/* A link's colour and underline are the run's own resolved properties (see
   `body::text_run`), so the UA's blue-and-underlined default has to be turned off
   or it wins over a document that stated a colour of its own. */
.of-page a{color:inherit;text-decoration:inherit;}
/* Footnotes and endnotes, collected at the end of the column: Word puts them at
   the foot of the page that references them, and this column has no pages. The
   hairline is the separator Word draws above them. */
.of-fnotes{margin-top:24px;border-top:1px solid #c9c9c9;padding-top:8px;}
.of-fn{margin-top:6px;}
/* The number in front of a note, which is also the link back to its marker. */
.of-fnb{vertical-align:super;font-size:0.7em;padding-right:4px;}
/* A text box's own paragraphs, hoisted out of the paragraph that anchored them
   because block content cannot sit inside a `p` — see `body::hoist`. The border is
   the only thing left saying a box is what this was, its real position being one
   this column cannot honour. */
.of-txbx{border:1px solid #c9c9c9;padding:6px 8px;margin:8px 0;}
.office-note{color:var(--fg-mute,#6b6b6b);font-size:11px;padding:6px 2px;}
";

/// The page box in px. Wider than [`super::OfficeDoc::page`] can carry, because the
/// four margins are only equal in most documents rather than in all of them.
pub struct Page {
    pub width: f32,
    pub height: f32,
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl Default for Page {
    fn default() -> Page {
        Page {
            width: DEFAULT_PAGE_W,
            height: DEFAULT_PAGE_H,
            left: DEFAULT_MARGIN,
            right: DEFAULT_MARGIN,
            top: DEFAULT_MARGIN,
            bottom: DEFAULT_MARGIN,
        }
    }
}

/// The document-derived half of the stylesheet: the page's default text, its tab
/// stop, and the padding when the three-tuple cannot carry it.
///
/// The default font and size arrive as the plain values they are rather than as a
/// dialect's resolved-style struct: those two disagree about everything except what
/// comes out of them.
pub fn page_css(
    page: &Page,
    default_font: Option<&str>,
    default_size_pt: f32,
    tab_px: f32,
) -> String {
    let mut s = Style::new();
    s.push_opt("font-family", default_font.map(str::to_string));
    s.push_opt("font-size", fmt_px(pt_to_px(default_size_pt)));
    // Tabs are literal U+0009 under `white-space:pre-wrap`, and `tab-size` is the
    // only thing that positions them — explicit tab stops are not modelled
    // (honouring one means knowing where the text currently is).
    s.push_opt("tab-size", fmt_px(tab_px));
    // So an empty document still looks like a page rather than a white strip.
    s.push_opt("min-height", fmt_px(page.height));
    let mut out = format!(".of-page{{{}}}\n", s.css());

    // `OfficeDoc::page` is a three-tuple — width, one x padding, one y padding —
    // so the host's `.of-page` rule can only state a symmetric box. When the
    // document's margins are not symmetric, the exact four-value padding is
    // emitted here instead: this stylesheet is written into the document *after*
    // the host's variant CSS and both rules are a single class, so equal
    // specificity means source order decides and this one wins.
    let asymmetric =
        (page.left - page.right).abs() > 0.5 || (page.top - page.bottom).abs() > 0.5;
    if asymmetric {
        let pad = [page.top, page.right, page.bottom, page.left]
            .iter()
            .map(|v| fmt_px(*v).unwrap_or_else(|| "0px".to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(".of-page{{padding:{pad};}}\n"));
    }
    out
}
