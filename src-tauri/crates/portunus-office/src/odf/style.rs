//! ODF style cascade: the `style:style` graph resolved into property bags.
//!
//! An ODF document's appearance is the sum of, weakest first: the
//! `style:default-style` for the style's family → the `style:parent-style-name`
//! chain of the style it names, root first → whatever the element states through
//! a second, *automatic* style. Every link states only some properties, which is
//! why every field here is an `Option` and every merge overwrites nothing it has
//! no value for — "unstated" and "stated as the default" are different, and only
//! the former inherits.
//!
//! Two files, one keyspace. The named styles live in `styles.xml`
//! (`office:styles`, plus that file's own `office:automatic-styles` and
//! `office:master-styles`); the one-off styles a producer generates per element
//! live in `content.xml`'s `office:automatic-styles`. Both are keyed
//! `(style:family, style:name)` and a name collision resolves in favour of
//! `content.xml`, so a document's own automatic style can never be shadowed by a
//! stylesheet entry it cannot see.
//!
//! Units are CSS px throughout (see [`super::length`]) rather than the source's
//! own spelling, because ODF has no single scaling to defer: one attribute is
//! `0.6cm`, the next `10pt`, the next `45%`. The one property that cannot be a
//! pure attribute merge is `fo:font-size`, which may be a percentage of the
//! *parent's already-resolved* size — see [`merge_text`].
//!
//! Every ODF renderer resolves through here. The bags a text document does not
//! read — a shape's fill and stroke, a drawing page's background — are the two
//! later passes', and carry their own `allow(dead_code)` until then.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::super::cellstyle::Rotation;
use super::super::docshape::{Page, MAX_MARGIN_PX, MAX_PAGE_PX, MIN_COLUMN_PX, MIN_PAGE_PX};
use super::super::drawingml::color::Color;
use super::super::drawingml::fill::Fill;
use super::super::drawingml::line::{Cap, Dash, Join, Line};
use super::super::fonts;
use super::super::html::pt_to_px;
use super::super::model::{self, Align, Border, Borders, Caps, LineHeight, Para, Script, TextRun};
use super::super::xml::{attr_local, child, elems, truthy};
use super::length::{self, clamp_px, Measure};
use roxmltree::Node;

/// Size used when no link in the cascade states one. LibreOffice's own Writer
/// default; it only matters for a package whose styles state no size at all,
/// because every producer states one on the default paragraph style.
pub const DEFAULT_SIZE_PT: f32 = 12.0;

/// Point bounds for a font size: below 1pt the text is invisible, above 2000pt a
/// single line box would blow the preview's byte budget. Same reasoning as
/// docx's `w:sz` range.
const MIN_SIZE_PT: f32 = 1.0;
const MAX_SIZE_PT: f32 = 2000.0;

/// Percentage bounds for a relative font size, before it resolves.
const MAX_SIZE_PCT: f32 = 1000.0;

/// Styles a document may define, across both layers. Matches docx's ceiling; past
/// this the remainder is dropped rather than letting a generated file allocate
/// without bound.
const MAX_STYLES: usize = 4096;

/// `style:parent-style-name` links followed before the chain is abandoned. The
/// deepest chain in the corpus is 3, so this is pure hardening — and it is what
/// makes a cycle terminate even if the visited set below were wrong.
const MAX_PARENT: usize = 16;

/// Widest indent, margin or padding honoured, in px — about 22 inches, docx's own
/// limit. Larger is a corrupt value that would push the text column off the page.
const MAX_INDENT_PX: f32 = 2112.0;

/// Tab stops kept per paragraph style. The list is document XML, so it is bounded;
/// nothing here can honour more than the first few anyway (see [`ParaProps::tab_stops`]).
const MAX_TAB_STOPS: usize = 64;

/// Longest style / font name used as a map key. Keys are never echoed into
/// output, but an unbounded one out of document XML is still a way to make the
/// memo table hold megabytes.
const MAX_NAME_CHARS: usize = 128;

// ── families ─────────────────────────────────────────────────────────────────

/// The `style:family` values that carry properties this module reads.
///
/// A family is part of the key, not a property: `style:name="Standard"` names a
/// different style in the `paragraph` family than in `table-cell`, and a
/// `style:parent-style-name` never crosses families.
///
/// Families ODF defines but nothing here renders (`chart`, `ruby`,
/// `presentation-page-layout`) are not stored at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Text,
    Paragraph,
    Graphic,
    Presentation,
    DrawingPage,
    Table,
    TableColumn,
    TableRow,
    TableCell,
}

fn family_of(v: &str) -> Option<Family> {
    Some(match v {
        "text" => Family::Text,
        "paragraph" => Family::Paragraph,
        "graphic" => Family::Graphic,
        "presentation" => Family::Presentation,
        "drawing-page" => Family::DrawingPage,
        "table" => Family::Table,
        "table-column" => Family::TableColumn,
        "table-row" => Family::TableRow,
        "table-cell" => Family::TableCell,
        _ => return None,
    })
}

// ── shared value types ───────────────────────────────────────────────────────

/// `fo:font-size`, which ODF states either absolutely or as a percentage of the
/// enclosing size. The percentage cannot be resolved where it is parsed — only
/// the merge knows what it is a percentage *of*.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSize {
    Pt(f32),
    /// The number in front of the `%`: `75%` is `75.0`.
    Percent(f32),
}

/// A colour-valued property that a document can explicitly set to *nothing*.
///
/// [`ColorVal::Auto`] is a statement, not the absence of one:
/// `style:use-window-font-color="true"` overrides an inherited red back to the
/// reader's own text colour. It spells out as the model's `None`, but only after
/// it has won the merge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorVal {
    Auto,
    Set(Color),
}

impl ColorVal {
    pub fn color(self) -> Option<Color> {
        match self {
            ColorVal::Auto => None,
            ColorVal::Set(c) => Some(c),
        }
    }
}

/// `style:text-position`. `Baseline` is kept rather than folded into `None` so
/// that `0% 100%` can cancel an inherited superscript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Baseline,
    Super,
    Sub,
}

/// How tall a line is, in ODF's three mutually exclusive spellings. Resolved to
/// [`LineHeight`] by [`to_para`], which is the first place the paragraph's own
/// font size is known — `style:line-spacing` needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineSpec {
    /// `fo:line-height` as a percentage, as a ratio: `150%` is `1.5`, and `1.0`
    /// is single spacing.
    Proportional(f32),
    /// `fo:line-height` as a length: the line is exactly this tall, in px.
    Fixed(f32),
    /// `style:line-height-at-least`, in px.
    AtLeast(f32),
    /// `style:line-spacing`: px of leading *added* to a single line, not a total.
    Leading(f32),
}

/// One edge of a box, as `fo:border*` states it.
///
/// [`Edge::None`] is a statement — `fo:border="none"` cancels an inherited border
/// rather than leaving it alone — which is why this is not an
/// `Option<length::Border>`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edge {
    None,
    Set(length::Border),
}

/// The four `fo:border-*` edges. Each side is an `Option` of its own because ODF
/// states them independently and a stronger link that mentions only
/// `fo:border-top` must not erase an inherited bottom.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sides {
    pub top: Option<Edge>,
    pub right: Option<Edge>,
    pub bottom: Option<Edge>,
    pub left: Option<Edge>,
}

impl Sides {
    pub fn merge(&mut self, src: &Sides) {
        for (dst, s) in [
            (&mut self.top, src.top),
            (&mut self.right, src.right),
            (&mut self.bottom, src.bottom),
            (&mut self.left, src.left),
        ] {
            if s.is_some() {
                *dst = s;
            }
        }
    }

    fn is_none(&self) -> bool {
        *self == Sides::default()
    }
}

/// `fo:padding` and its four per-side spellings, in px. `None` inherits; what an
/// unstated side finally falls back to is the renderer's stylesheet, not this.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Padding {
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
}

impl Padding {
    pub fn merge(&mut self, src: &Padding) {
        for (dst, s) in [
            (&mut self.top, src.top),
            (&mut self.right, src.right),
            (&mut self.bottom, src.bottom),
            (&mut self.left, src.left),
        ] {
            if s.is_some() {
                *dst = s;
            }
        }
    }
}

/// `table:align` — where a narrower table sits in the text column, not how its
/// text aligns. `Margins` (stretch to both margins) has no [`Align`] spelling,
/// which is why this is its own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
    Margins,
}

/// `fo:break-before` / `fo:break-after`. [`BreakKind::Auto`] is a statement — it
/// cancels a break an ancestor style asked for — so it is a member here rather
/// than the absent case, which stays the surrounding `Option`'s `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakKind {
    Auto,
    Column,
    Page,
}

/// `style:wrap`: how the text flows past an anchored frame.
///
/// [`WrapMode::Left`] means the text runs down the frame's *left* side, i.e. the
/// frame sits at the right — the same by-elimination reading `wp:wrapSquare`'s
/// `@wrapText` needs in the docx path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    /// No text beside the frame at all: it gets a line of its own.
    None,
    Left,
    Right,
    /// Text on both sides (`parallel`), or on whichever side has room
    /// (`dynamic`) — either way the side comes from [`HPos`].
    Parallel,
    Dynamic,
    /// Behind or in front of the text.
    RunThrough,
}

/// `style:horizontal-pos`: which edge of its anchor a frame is aligned to.
/// `from-left` is an offset (`svg:x`) rather than an edge, which is why it is a
/// member of its own instead of collapsing onto [`HPos::Left`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HPos {
    Left,
    Center,
    Right,
    FromLeft,
}

// ── property bags ────────────────────────────────────────────────────────────

/// `style:text-properties` as one element states it.
///
/// The `-asian` / `-complex` twins (`style:font-size-asian`,
/// `style:font-weight-complex`, …) are deliberately not read: honouring them
/// needs per-script segmentation of the run's text, which nothing here does, and
/// picking one of the three arbitrarily is worse than picking the Western one the
/// document also always states.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextProps {
    /// `fo:font-size`, still relative if every link so far stated a percentage.
    pub size: Option<FontSize>,
    /// Already through [`fonts::css_font_stack`], i.e. safe inside a `style`
    /// attribute.
    pub font: Option<String>,
    /// The raw family name, kept for the symbol-font remap.
    pub font_raw: Option<String>,
    /// `fo:font-weight`: `bold`, or a numeric weight of 600 or more.
    pub bold: Option<bool>,
    /// `fo:font-style`: `italic` or `oblique`.
    pub italic: Option<bool>,
    /// `style:text-underline-style`; `none` is the off switch.
    pub underline: Option<bool>,
    /// `style:text-line-through-style`.
    pub strike: Option<bool>,
    /// `fo:text-transform="uppercase"`. Wins over [`TextProps::small_caps`] when
    /// both are on, as in CSS. `lowercase` and `capitalize` are not modelled —
    /// [`Caps`] has no member for either, and faking one would rewrite the text.
    pub caps: Option<bool>,
    /// `fo:font-variant="small-caps"`.
    pub small_caps: Option<bool>,
    /// `fo:color`, or [`ColorVal::Auto`] for `style:use-window-font-color`.
    pub color: Option<ColorVal>,
    /// `fo:background-color` — a marker band behind the text, which is what the
    /// model calls a highlight. `transparent` arrives as an alpha-0 colour and so
    /// cancels an inherited band.
    pub highlight: Option<Color>,
    /// `fo:letter-spacing`, points (may be negative). `normal` is `0`.
    pub letter_spacing_pt: Option<f32>,
    pub position: Option<Position>,
    /// `fo:language`, with `fo:country` appended when stated (`en`, `en-GB`).
    /// Carried for an HTML `lang`; nothing here hyphenates or spell-checks.
    pub language: Option<String>,
}

/// `style:paragraph-properties` as one element states it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParaProps {
    /// `fo:text-align`. `inside`/`outside` are page-relative and have no meaning
    /// in a single scrolling column, so they are dropped rather than guessed.
    pub align: Option<Align>,
    /// `fo:margin-top` / `fo:margin-bottom`, px.
    pub space_before_px: Option<f32>,
    pub space_after_px: Option<f32>,
    /// `fo:margin-left` / `fo:margin-right`, px.
    pub indent_px: Option<f32>,
    pub indent_end_px: Option<f32>,
    /// `fo:text-indent`, px, **signed as the model wants it**: negative hangs the
    /// first line left of the rest. ODF states the two directions in this one
    /// attribute, so unlike docx there is nothing to fold or negate.
    pub first_line_px: Option<f32>,
    /// `style:auto-text-indent`: the first-line indent is computed from the font
    /// rather than stated. See [`to_para`] for why that means *no* indent here.
    pub auto_text_indent: Option<bool>,
    pub line: Option<LineSpec>,
    /// `fo:background-color`.
    pub background: Option<Color>,
    pub borders: Sides,
    /// `style:contextual-spacing`: drop the before/after space between
    /// paragraphs of the same style. Whether a *neighbour* qualifies is the body
    /// walk's business, so it is only carried here.
    pub contextual_spacing: Option<bool>,
    /// `style:writing-mode="rl-tb"`. The vertical modes (`tb-rl`, `tb-lr`) are
    /// not modelled: the model's `rtl` is a direction, not a writing mode.
    pub rtl: Option<bool>,
    /// `fo:keep-with-next="always"`. Parsed and then ignored — this renderer does
    /// not paginate, so there is no break for a paragraph to be kept across.
    pub keep_with_next: Option<bool>,
    /// `fo:break-before` / `fo:break-after`: an *author-stated* break, unlike
    /// `text:soft-page-break`, which is the producer's own pagination and must be
    /// ignored. A renderer with no pages draws it as a rule.
    pub break_before: Option<BreakKind>,
    pub break_after: Option<BreakKind>,
    /// `style:tab-stop-distance`, px: the *default* tab width, which is the one
    /// tab measurement CSS can express (`tab-size`, see `docshape::page_css`).
    pub tab_stop_distance_px: Option<f32>,
    /// `style:tab-stops/style:tab-stop@style:position`, px, in document order.
    ///
    /// Positions only, and unused by the emitter: honouring an explicit stop
    /// means knowing where the text currently is, and nothing here measures text
    /// (the same reason `model::Run::Tab` documents). Carried so a renderer can
    /// use the first stop as a tab width instead of inventing one. A style's list
    /// *replaces* its parent's rather than merging into it, because ODF states it
    /// as one container element.
    pub tab_stops: Vec<f32>,
}

/// `style:table-cell-properties` as one element states it.
///
/// `style:diagonal-tl-br` / `-bl-tr` are not read: CSS has no cell diagonal, and
/// drawing one needs a per-cell gradient background.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CellProps {
    pub padding: Padding,
    pub borders: Sides,
    /// `fo:background-color`.
    pub background: Option<Color>,
    /// `style:vertical-align`, already in the shared
    /// [`super::super::cellstyle::AlignSpec`] vocabulary — ODF's `middle` is
    /// SpreadsheetML's `center`. `automatic` states nothing.
    pub v_align: Option<&'static str>,
    /// `fo:wrap-option`: `wrap` or `no-wrap`.
    pub wrap: Option<bool>,
    pub shrink_to_fit: Option<bool>,
    /// `style:rotation-angle`, degrees counter-clockwise.
    pub rotation: Option<Rotation>,
}

/// `style:table-column-properties`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnProps {
    /// `style:column-width`, px.
    pub width_px: Option<f32>,
    /// `style:use-optimal-column-width`: the width is the content's, not the
    /// stated one.
    pub optimal: Option<bool>,
}

/// `style:table-row-properties`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RowProps {
    /// `style:row-height`, px.
    pub height_px: Option<f32>,
    pub optimal: Option<bool>,
}

/// `style:table-properties`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TableProps {
    /// `style:width`, or `style:rel-width` for the percentage spelling.
    pub width: Option<Measure>,
    /// `table:display`: a hidden table renders nothing at all.
    pub display: Option<bool>,
    pub align: Option<TableAlign>,
    /// `fo:margin-left`, px: how far a `table:align="left"` table is indented from
    /// the text column's leading edge.
    pub indent_px: Option<f32>,
    /// `fo:break-before` / `fo:break-after`, exactly as on a paragraph: a table is
    /// a block and a producer states a page break on either kind.
    pub break_before: Option<BreakKind>,
    pub break_after: Option<BreakKind>,
}

/// What `draw:fill` names. The three that reference a *definition* elsewhere in
/// `office:styles` (`draw:gradient`, `draw:hatch`, `draw:fill-image`) collapse
/// onto [`FillKind::Unsupported`]: resolving one is a separate index this module
/// does not build, and inventing a flat colour for it would repaint the shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillKind {
    None,
    Solid,
    Unsupported,
}

/// The fill half of a `style:graphic-properties` / `style:drawing-page-properties`.
///
/// The three attributes stay separate through the cascade because that is how ODF
/// states them: an automatic style routinely restates only `draw:fill-color`, and
/// folding kind and colour together at parse time would let that turn an
/// inherited `draw:fill="none"` back on. [`FillProps::fill`] composes them once
/// the chain is done.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FillProps {
    pub kind: Option<FillKind>,
    /// `draw:fill-color`.
    pub color: Option<Color>,
    /// `draw:opacity`, 0..1.
    pub opacity: Option<f32>,
}

impl FillProps {
    pub fn merge(&mut self, src: &FillProps) {
        if src.kind.is_some() {
            self.kind = src.kind;
        }
        if src.color.is_some() {
            self.color = src.color;
        }
        if src.opacity.is_some() {
            self.opacity = src.opacity;
        }
    }

    /// The resolved fill, or `None` when nothing usable was stated — in which
    /// case the caller's own inherited fill is the better answer, the same
    /// contract `drawingml::fill::parse_fill_opt` has.
    ///
    /// A text document paints no shape bodies (a `draw:frame` there holds a
    /// picture or paragraphs), so the slide pass is the caller.
    #[allow(dead_code)]
    pub fn fill(&self) -> Option<Fill> {
        let paint = || {
            let c = self.color?;
            Some(Fill::Solid(match self.opacity {
                Some(a) => Color {
                    rgb: c.rgb,
                    alpha: c.alpha * a as f64,
                },
                None => c,
            }))
        };
        match self.kind {
            Some(FillKind::None) => Some(Fill::None),
            Some(FillKind::Solid) => paint(),
            Some(FillKind::Unsupported) => None,
            // A colour with no `draw:fill` beside it is how a producer restates
            // only the colour of a fill that is already solid.
            None => paint(),
        }
    }
}

/// What `draw:stroke` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeKind {
    None,
    Solid,
    /// `draw:stroke="dash"`. Which dash (`draw:stroke-dash` names a definition)
    /// is not resolved — CSS has one dashed border.
    Dash,
}

/// The outline half of a `style:graphic-properties`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StrokeProps {
    pub kind: Option<StrokeKind>,
    /// `svg:stroke-color`.
    pub color: Option<Color>,
    /// `svg:stroke-width`, px.
    pub width_px: Option<f32>,
    /// `svg:stroke-linecap`.
    pub cap: Option<Cap>,
}

impl StrokeProps {
    pub fn merge(&mut self, src: &StrokeProps) {
        if src.kind.is_some() {
            self.kind = src.kind;
        }
        if src.color.is_some() {
            self.color = src.color;
        }
        if src.width_px.is_some() {
            self.width_px = src.width_px;
        }
        if src.cap.is_some() {
            self.cap = src.cap;
        }
    }

    /// The resolved outline, in the shared [`Line`] type so that
    /// `drawingml::line::line_css` does the emitting.
    ///
    /// `draw:stroke="none"` is *not* `None` here: it comes back as a [`Line`]
    /// with no paint, which `line_css` spells `border:none;` — an explicit reset
    /// that can override an inherited border. `None` means nothing was stated.
    ///
    /// Unused by the odt renderer, for the reason [`FillProps::fill`] gives.
    #[allow(dead_code)]
    pub fn line(&self) -> Option<Line> {
        let kind = self.kind?;
        Some(Line {
            width_px: self.width_px.unwrap_or(1.0) as f64,
            fill: match kind {
                StrokeKind::None => Fill::None,
                // A stroke with no colour of its own is black, which is both
                // ODF's initial value and what `Line::stroke_color` falls back to.
                _ => Fill::Solid(self.color.unwrap_or(Color::from_rgb(0))),
            },
            dash: match kind {
                StrokeKind::Dash => Dash::Dashed,
                _ => Dash::Solid,
            },
            cap: self.cap.unwrap_or(Cap::Flat),
            // ODF's `draw:stroke-linejoin` is not read: CSS borders have no join,
            // and the SVG-drawing caller that would want one also wants the
            // geometry this module never sees.
            join: Join::Miter,
        })
    }
}

/// `style:graphic-properties` as one element states it — the frame box of an odt
/// `draw:frame` or an odp shape.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GraphicProps {
    pub fill: FillProps,
    pub stroke: StrokeProps,
    /// `draw:textarea-vertical-align` / `-horizontal-align`, in the shared
    /// [`super::super::cellstyle::AlignSpec`] vocabulary.
    pub text_v_align: Option<&'static str>,
    pub text_h_align: Option<&'static str>,
    pub auto_grow_height: Option<bool>,
    pub auto_grow_width: Option<bool>,
    pub padding: Padding,
    /// `fo:min-height` / `fo:min-width`, px.
    pub min_height_px: Option<f32>,
    pub min_width_px: Option<f32>,
    /// `fo:wrap-option`.
    pub wrap: Option<bool>,
    /// `style:wrap`: how the text flows past the frame. Only the *side* of it
    /// survives a reflowing column — see the odt frame emitter.
    pub wrap_mode: Option<WrapMode>,
    /// `style:horizontal-pos`, which is what names the side when the wrap itself
    /// does not (`parallel`, `dynamic`).
    pub h_pos: Option<HPos>,
}

/// `style:drawing-page-properties`: the slide's background, and nothing else.
///
/// `presentation:transition-*` and `smil:type` are not read — a transition is
/// between two slides a static preview shows one after the other — and neither
/// are the `presentation:display-header`/`-footer`/`-page-number` flags, because
/// the header, footer and page number they would show are themselves not
/// rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DrawPageProps {
    pub fill: FillProps,
}

/// One style's declarations — or a whole chain's, once [`merge`] has folded them
/// together, which is what [`Styles::resolve`] hands back.
///
/// Every family shares this one shape, because a `style:style` is free to carry
/// several property elements and the real corpus does: a `presentation` style
/// carries graphic *and* paragraph *and* text properties, and a `table-cell`
/// automatic style in a spreadsheet carries cell, paragraph and text. Which bags
/// a family actually fills:
///
/// | family | bags |
/// |---|---|
/// | `text` | [`Resolved::text`] |
/// | `paragraph` | `text`, [`Resolved::para`] |
/// | `graphic`, `presentation` | `text`, `para`, [`Resolved::graphic`] |
/// | `table-cell` | `text`, `para`, [`Resolved::cell`] |
/// | `table`, `table-column`, `table-row` | [`Resolved::table`], [`Resolved::column`], [`Resolved::row`] |
/// | `drawing-page` | [`Resolved::page`] |
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolved {
    pub text: TextProps,
    pub para: ParaProps,
    pub cell: CellProps,
    pub column: ColumnProps,
    pub row: RowProps,
    pub table: TableProps,
    pub graphic: GraphicProps,
    pub page: DrawPageProps,
}

// ── merges ───────────────────────────────────────────────────────────────────

/// Later wins, field by field, in every bag. Nothing `src` leaves unstated is
/// touched.
pub fn merge(dst: &mut Resolved, src: &Resolved) {
    merge_text(&mut dst.text, &src.text);
    merge_para(&mut dst.para, &src.para);
    merge_cell(&mut dst.cell, &src.cell);
    merge_column(&mut dst.column, &src.column);
    merge_row(&mut dst.row, &src.row);
    merge_table(&mut dst.table, &src.table);
    merge_graphic(&mut dst.graphic, &src.graphic);
    dst.page.fill.merge(&src.page.fill);
}

/// Later wins — except for the font size, which is the one property in ODF whose
/// value depends on what it is layered over.
///
/// `fo:font-size="75%"` is 75% of the enclosing size, so a percentage arriving
/// over an absolute resolves *here*, while a percentage arriving over another
/// percentage composes into one (a `50%` under a `200%` is `100%`) and stays
/// relative until some link states a length. That keeps the resolution honest for
/// a text style resolved on its own and then merged over a paragraph's — the
/// order the renderers use.
pub fn merge_text(dst: &mut TextProps, src: &TextProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(
        bold,
        italic,
        underline,
        strike,
        caps,
        small_caps,
        color,
        highlight,
        letter_spacing_pt,
        position,
        language
    );
    if let Some(s) = src.size {
        dst.size = Some(match (s, dst.size) {
            (FontSize::Percent(p), Some(FontSize::Pt(base))) => {
                FontSize::Pt(clamp_pt(base * p / 100.0))
            }
            (FontSize::Percent(p), Some(FontSize::Percent(q))) => {
                FontSize::Percent((p * q / 100.0).clamp(0.0, MAX_SIZE_PCT))
            }
            (v, _) => v,
        });
    }
    // The stack and the raw name are one statement in two fields: a link that
    // changes the face must not leave the previous one's raw name behind, or the
    // symbol remap keys off a family that is no longer in effect.
    if src.font.is_some() {
        dst.font = src.font.clone();
        dst.font_raw = src.font_raw.clone();
    }
}

pub fn merge_para(dst: &mut ParaProps, src: &ParaProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f; } )* };
    }
    take!(
        align,
        space_before_px,
        space_after_px,
        indent_px,
        indent_end_px,
        first_line_px,
        auto_text_indent,
        line,
        background,
        contextual_spacing,
        rtl,
        keep_with_next,
        tab_stop_distance_px,
        break_before,
        break_after
    );
    dst.borders.merge(&src.borders);
    // One container element, one statement: a style that lists its own stops
    // replaces the inherited list rather than adding to it.
    if !src.tab_stops.is_empty() {
        dst.tab_stops = src.tab_stops.clone();
    }
}

pub fn merge_cell(dst: &mut CellProps, src: &CellProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f; } )* };
    }
    take!(background, v_align, wrap, shrink_to_fit, rotation);
    dst.padding.merge(&src.padding);
    dst.borders.merge(&src.borders);
}

pub fn merge_column(dst: &mut ColumnProps, src: &ColumnProps) {
    if src.width_px.is_some() {
        dst.width_px = src.width_px;
    }
    if src.optimal.is_some() {
        dst.optimal = src.optimal;
    }
}

pub fn merge_row(dst: &mut RowProps, src: &RowProps) {
    if src.height_px.is_some() {
        dst.height_px = src.height_px;
    }
    if src.optimal.is_some() {
        dst.optimal = src.optimal;
    }
}

pub fn merge_table(dst: &mut TableProps, src: &TableProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f; } )* };
    }
    take!(width, display, align, indent_px, break_before, break_after);
}

pub fn merge_graphic(dst: &mut GraphicProps, src: &GraphicProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f; } )* };
    }
    take!(
        text_v_align,
        text_h_align,
        auto_grow_height,
        auto_grow_width,
        min_height_px,
        min_width_px,
        wrap,
        wrap_mode,
        h_pos
    );
    dst.fill.merge(&src.fill);
    dst.stroke.merge(&src.stroke);
    dst.padding.merge(&src.padding);
}

fn clamp_pt(v: f32) -> f32 {
    clamp_px(v, MIN_SIZE_PT, MAX_SIZE_PT)
}

// ── odf → properties ─────────────────────────────────────────────────────────
//
// Every lookup goes through `attr_local`: ODF's prefixes are conventional, not
// guaranteed, and one property element mixes four namespaces (`fo:`, `style:`,
// `draw:`, `svg:`) that a producer is free to bind to any prefix it likes.

/// A length attribute in px, dropping a percentage — the properties below that
/// can resolve one ask for it by name.
fn len(n: Node, name: &str) -> Option<f32> {
    attr_local(n, name).and_then(length::parse_len)
}

/// A length attribute in px, bounded to what a text column can be indented or
/// spaced by. Signed: `fo:text-indent` and `fo:letter-spacing` both go negative.
fn len_bounded(n: Node, name: &str) -> Option<f32> {
    len(n, name).map(|v| v.clamp(-MAX_INDENT_PX, MAX_INDENT_PX))
}

/// A length attribute in px, floored at zero — spacing and padding cannot be
/// negative, and a producer that says otherwise means none.
fn len_pos(n: Node, name: &str) -> Option<f32> {
    len(n, name).map(|v| clamp_px(v, 0.0, MAX_INDENT_PX))
}

fn color(n: Node, name: &str) -> Option<Color> {
    attr_local(n, name).and_then(length::parse_color)
}

fn flag(n: Node, name: &str) -> Option<bool> {
    attr_local(n, name).map(truthy)
}

/// One `fo:break-before` / `fo:break-after`. An unrecognized value states nothing
/// rather than defaulting to a break the document did not ask for.
fn break_kind(n: Node, name: &str) -> Option<BreakKind> {
    match attr_local(n, name)?.trim() {
        "auto" => Some(BreakKind::Auto),
        "column" => Some(BreakKind::Column),
        "page" => Some(BreakKind::Page),
        _ => None,
    }
}

/// A name used as a map key, bounded and empty-rejected.
fn name_of(n: Node, attr: &str) -> Option<String> {
    let v = attr_local(n, attr)?.trim();
    if v.is_empty() {
        return None;
    }
    Some(v.chars().take(MAX_NAME_CHARS).collect())
}

/// Parses one `style:text-properties`. `faces` resolves a `style:font-name`
/// against `office:font-face-decls`.
pub fn parse_text_props(n: Node, faces: &HashMap<String, String>) -> TextProps {
    let mut p = TextProps::default();
    p.size = attr_local(n, "font-size")
        .and_then(length::parse_measure)
        .map(|m| match m {
            Measure::Px(px) => FontSize::Pt(clamp_pt(px_to_pt(px))),
            Measure::Percent(v) => FontSize::Percent(v.clamp(0.0, MAX_SIZE_PCT)),
        });
    // `style:font-name` points into the font-face declarations, which is where
    // the real family lives — `Liberation Serif1` is a declaration name, not a
    // typeface. `fo:font-family` states the family directly.
    if let Some(raw) = font_face(n, faces) {
        p.font = Some(fonts::css_font_stack(&raw));
        p.font_raw = Some(raw);
    }
    p.bold = attr_local(n, "font-weight").and_then(font_weight);
    p.italic = attr_local(n, "font-style").map(|v| matches!(v, "italic" | "oblique"));
    p.underline = attr_local(n, "text-underline-style").map(|v| v != "none");
    p.strike = attr_local(n, "text-line-through-style").map(|v| v != "none");
    p.caps = attr_local(n, "text-transform").and_then(|v| match v {
        "uppercase" => Some(true),
        "none" => Some(false),
        // `lowercase` and `capitalize` have no `Caps` member: both change which
        // glyphs are shown rather than how the stated ones are cased, and the
        // model deliberately does not rewrite a run's text.
        _ => None,
    });
    p.small_caps = attr_local(n, "font-variant").and_then(|v| match v {
        "small-caps" => Some(true),
        "normal" => Some(false),
        _ => None,
    });
    p.color = match color(n, "color") {
        Some(c) => Some(ColorVal::Set(c)),
        // The window's own font colour: a statement that cancels an inherited
        // colour rather than an absence of one.
        None => flag(n, "use-window-font-color")
            .filter(|v| *v)
            .map(|_| ColorVal::Auto),
    };
    p.highlight = color(n, "background-color");
    p.letter_spacing_pt = attr_local(n, "letter-spacing").and_then(|v| {
        if v.trim() == "normal" {
            return Some(0.0);
        }
        length::parse_len(v).map(|px| px_to_pt(px).clamp(-100.0, 100.0))
    });
    p.position = attr_local(n, "text-position").and_then(text_position);
    p.language = name_of(n, "language").map(|l| match name_of(n, "country") {
        Some(c) => format!("{l}-{c}"),
        None => l,
    });
    p
}

/// `fo:font-weight`: the two keywords plus the numeric scale. CSS calls 600 and
/// up bold, and so does every producer that writes a number here.
fn font_weight(v: &str) -> Option<bool> {
    match v.trim() {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        n => n.parse::<f32>().ok().map(|w| w >= 600.0),
    }
}

/// `style:text-position`: `super`, `sub`, or one or two percentages of which the
/// first is the raise. Zero is the baseline, which is a statement — it cancels an
/// inherited superscript.
fn text_position(v: &str) -> Option<Position> {
    let first = v.split_whitespace().next()?;
    match first {
        "super" => Some(Position::Super),
        "sub" => Some(Position::Sub),
        _ => {
            let pct = length::parse_percent(first)?;
            Some(if pct > 0.0 {
                Position::Super
            } else if pct < 0.0 {
                Position::Sub
            } else {
                Position::Baseline
            })
        }
    }
}

/// The typeface a `style:text-properties` names, resolved through the font-face
/// declarations when it names one of those instead.
fn font_face(n: Node, faces: &HashMap<String, String>) -> Option<String> {
    if let Some(name) = name_of(n, "font-name") {
        return Some(faces.get(&name).cloned().unwrap_or(name));
    }
    // A `fo:font-family` may be a quoted family or a whole CSS-ish list; the
    // first entry is the one a stack should start with.
    let raw = attr_local(n, "font-family")?;
    let first = raw.split(',').next()?.trim().trim_matches(['\'', '"']);
    (!first.is_empty()).then(|| first.chars().take(MAX_NAME_CHARS).collect())
}

/// Parses one `style:paragraph-properties`, including its `style:tab-stops`.
pub fn parse_para_props(n: Node) -> ParaProps {
    let mut p = ParaProps::default();
    p.align = attr_local(n, "text-align").and_then(text_align);
    // `fo:margin` is the four-in-one spelling; a per-side attribute overrides it.
    let all = len_bounded(n, "margin");
    p.space_before_px = len_pos(n, "margin-top").or_else(|| all.map(|v| v.max(0.0)));
    p.space_after_px = len_pos(n, "margin-bottom").or_else(|| all.map(|v| v.max(0.0)));
    p.indent_px = len_bounded(n, "margin-left").or(all);
    p.indent_end_px = len_bounded(n, "margin-right").or(all);
    p.first_line_px = len_bounded(n, "text-indent");
    p.auto_text_indent = flag(n, "auto-text-indent");
    p.line = line_spec(n);
    p.background = color(n, "background-color");
    p.borders = parse_sides(n);
    p.contextual_spacing = flag(n, "contextual-spacing");
    p.rtl = attr_local(n, "writing-mode").and_then(|v| match v {
        "rl-tb" => Some(true),
        "lr-tb" => Some(false),
        // `tb-rl`, `tb-lr` and `page` are writing modes, not directions.
        _ => None,
    });
    p.keep_with_next = attr_local(n, "keep-with-next").map(|v| v == "always");
    p.break_before = break_kind(n, "break-before");
    p.break_after = break_kind(n, "break-after");
    p.tab_stop_distance_px = len_pos(n, "tab-stop-distance");
    if let Some(stops) = child(n, "tab-stops") {
        p.tab_stops = elems(stops)
            .filter(|e| e.tag_name().name() == "tab-stop")
            .filter_map(|e| len_pos(e, "position"))
            .take(MAX_TAB_STOPS)
            .collect();
    }
    p
}

/// `fo:text-align`. ODF uses the logical spellings; `left`/`right` appear anyway
/// because CSS does.
fn text_align(v: &str) -> Option<Align> {
    Some(match v {
        "start" | "left" => Align::Left,
        "center" => Align::Center,
        "end" | "right" => Align::Right,
        "justify" => Align::Justify,
        _ => return None,
    })
}

/// The three line-height spellings, in the order ODF's own precedence puts them.
/// They are mutually exclusive in every producer's output; when a style somehow
/// carries two, the total (`fo:line-height`) beats the floor, which beats the
/// leading.
fn line_spec(n: Node) -> Option<LineSpec> {
    if let Some(v) = attr_local(n, "line-height") {
        if v.trim() == "normal" {
            return Some(LineSpec::Proportional(1.0));
        }
        return match length::parse_measure(v)? {
            Measure::Percent(p) => Some(LineSpec::Proportional((p / 100.0).clamp(0.1, 20.0))),
            Measure::Px(px) => Some(LineSpec::Fixed(clamp_px(px, 0.0, MAX_INDENT_PX))),
        };
    }
    if let Some(px) = len_pos(n, "line-height-at-least") {
        return Some(LineSpec::AtLeast(px));
    }
    // Leading, not a height: negative is legal and tightens the line.
    len_bounded(n, "line-spacing").map(LineSpec::Leading)
}

/// `fo:border` and its four per-side spellings. The shorthand seeds all four
/// edges and a per-side attribute then overrides its own, which is the order CSS
/// resolves them in too.
fn parse_sides(n: Node) -> Sides {
    let mut s = Sides::default();
    if let Some(all) = attr_local(n, "border").map(edge) {
        s = Sides {
            top: Some(all),
            right: Some(all),
            bottom: Some(all),
            left: Some(all),
        };
    }
    for (attr, dst) in [
        ("border-top", &mut s.top),
        ("border-right", &mut s.right),
        ("border-bottom", &mut s.bottom),
        ("border-left", &mut s.left),
    ] {
        if let Some(v) = attr_local(n, attr) {
            *dst = Some(edge(v));
        }
    }
    s
}

/// One `fo:border*` value. `none`/`hidden`/unusable is [`Edge::None`] rather than
/// a dropped declaration, because the attribute's presence is itself the
/// statement that this edge draws nothing.
fn edge(v: &str) -> Edge {
    match length::parse_border(v) {
        Some(b) => Edge::Set(b),
        None => Edge::None,
    }
}

/// `fo:padding` and its four per-side spellings, same precedence as the borders.
fn parse_padding(n: Node) -> Padding {
    let all = len_pos(n, "padding");
    Padding {
        top: len_pos(n, "padding-top").or(all),
        right: len_pos(n, "padding-right").or(all),
        bottom: len_pos(n, "padding-bottom").or(all),
        left: len_pos(n, "padding-left").or(all),
    }
}

/// `fo:wrap-option`.
fn wrap_option(n: Node) -> Option<bool> {
    attr_local(n, "wrap-option").and_then(|v| match v {
        "wrap" => Some(true),
        "no-wrap" => Some(false),
        _ => None,
    })
}

/// Parses one `style:table-cell-properties`.
pub fn parse_cell_props(n: Node) -> CellProps {
    CellProps {
        padding: parse_padding(n),
        borders: parse_sides(n),
        background: color(n, "background-color"),
        v_align: attr_local(n, "vertical-align").and_then(|v| match v {
            "top" => Some("top"),
            // SpreadsheetML's spelling of the same intent, which is what
            // `cellstyle::align_css` takes.
            "middle" => Some("center"),
            "bottom" => Some("bottom"),
            // `automatic` defers to the cell's content.
            _ => None,
        }),
        wrap: wrap_option(n),
        shrink_to_fit: flag(n, "shrink-to-fit"),
        rotation: attr_local(n, "rotation-angle")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map(|deg| Rotation::Ccw(deg.rem_euclid(360.0))),
    }
}

/// Parses one `style:table-column-properties`.
pub fn parse_column_props(n: Node) -> ColumnProps {
    ColumnProps {
        width_px: len(n, "column-width").map(|v| clamp_px(v, 0.0, MAX_PAGE_PX)),
        optimal: flag(n, "use-optimal-column-width"),
    }
}

/// Parses one `style:table-row-properties`.
pub fn parse_row_props(n: Node) -> RowProps {
    RowProps {
        height_px: len(n, "row-height").map(|v| clamp_px(v, 0.0, MAX_PAGE_PX)),
        optimal: flag(n, "use-optimal-row-height"),
    }
}

/// Parses one `style:table-properties`.
pub fn parse_table_props(n: Node) -> TableProps {
    TableProps {
        // `style:width` is the length spelling and `style:rel-width` the
        // percentage one; a producer writes one or the other.
        width: attr_local(n, "width")
            .or_else(|| attr_local(n, "rel-width"))
            .and_then(length::parse_measure),
        display: flag(n, "display"),
        align: attr_local(n, "align").and_then(|v| {
            Some(match v {
                "left" => TableAlign::Left,
                "center" => TableAlign::Center,
                "right" => TableAlign::Right,
                "margins" => TableAlign::Margins,
                _ => return None,
            })
        }),
        indent_px: len_pos(n, "margin-left"),
        break_before: break_kind(n, "break-before"),
        break_after: break_kind(n, "break-after"),
    }
}

/// The `draw:fill` group, shared by a graphic style and a drawing page.
fn parse_fill_props(n: Node) -> FillProps {
    FillProps {
        kind: attr_local(n, "fill").and_then(|v| {
            Some(match v {
                "none" => FillKind::None,
                "solid" => FillKind::Solid,
                "gradient" | "hatch" | "bitmap" => FillKind::Unsupported,
                _ => return None,
            })
        }),
        color: color(n, "fill-color"),
        opacity: attr_local(n, "opacity")
            .and_then(length::parse_percent)
            .map(|v| (v / 100.0).clamp(0.0, 1.0)),
    }
}

/// Parses one `style:graphic-properties`.
pub fn parse_graphic_props(n: Node) -> GraphicProps {
    GraphicProps {
        fill: parse_fill_props(n),
        stroke: StrokeProps {
            kind: attr_local(n, "stroke").and_then(|v| {
                Some(match v {
                    "none" => StrokeKind::None,
                    "solid" => StrokeKind::Solid,
                    "dash" => StrokeKind::Dash,
                    _ => return None,
                })
            }),
            color: color(n, "stroke-color"),
            width_px: len_pos(n, "stroke-width"),
            cap: attr_local(n, "stroke-linecap").and_then(|v| {
                Some(match v {
                    "butt" => Cap::Flat,
                    "round" => Cap::Round,
                    "square" => Cap::Square,
                    _ => return None,
                })
            }),
        },
        text_v_align: attr_local(n, "textarea-vertical-align").and_then(|v| match v {
            "top" => Some("top"),
            "middle" => Some("center"),
            "bottom" => Some("bottom"),
            "justify" => Some("justify"),
            _ => None,
        }),
        text_h_align: attr_local(n, "textarea-horizontal-align").and_then(|v| match v {
            "left" => Some("left"),
            "center" => Some("center"),
            "right" => Some("right"),
            "justify" => Some("justify"),
            _ => None,
        }),
        auto_grow_height: flag(n, "auto-grow-height"),
        auto_grow_width: flag(n, "auto-grow-width"),
        padding: parse_padding(n),
        min_height_px: len_pos(n, "min-height"),
        min_width_px: len_pos(n, "min-width"),
        wrap: wrap_option(n),
        wrap_mode: attr_local(n, "wrap").and_then(|v| {
            Some(match v {
                "none" => WrapMode::None,
                "left" => WrapMode::Left,
                "right" => WrapMode::Right,
                "parallel" | "biggest" => WrapMode::Parallel,
                "dynamic" => WrapMode::Dynamic,
                "run-through" => WrapMode::RunThrough,
                _ => return None,
            })
        }),
        h_pos: attr_local(n, "horizontal-pos").and_then(|v| {
            Some(match v {
                "left" | "inside" => HPos::Left,
                "center" => HPos::Center,
                "right" | "outside" => HPos::Right,
                "from-left" | "from-inside" => HPos::FromLeft,
                _ => return None,
            })
        }),
    }
}

/// Every property element a `style:style` (or a `style:default-style`) carries,
/// dispatched by local name. One style may hold several — a `presentation` style
/// states graphic, paragraph and text properties in one element — so this is a
/// walk rather than a lookup per family.
fn parse_props(style: Node, faces: &HashMap<String, String>) -> Resolved {
    let mut r = Resolved::default();
    for e in elems(style) {
        match e.tag_name().name() {
            "text-properties" => r.text = parse_text_props(e, faces),
            "paragraph-properties" => r.para = parse_para_props(e),
            "table-cell-properties" => r.cell = parse_cell_props(e),
            "table-column-properties" => r.column = parse_column_props(e),
            "table-row-properties" => r.row = parse_row_props(e),
            "table-properties" => r.table = parse_table_props(e),
            "graphic-properties" => r.graphic = parse_graphic_props(e),
            "drawing-page-properties" => r.page = DrawPageProps { fill: parse_fill_props(e) },
            _ => {}
        }
    }
    r
}

fn px_to_pt(px: f32) -> f32 {
    // The inverse of `html::pt_to_px`, which is the one definition of the point.
    px * 72.0 / 96.0
}

// ── the store ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RawStyle {
    /// `style:parent-style-name`, within the same family.
    parent: Option<String>,
    /// `style:master-page-name`: which master page the block using this style
    /// starts. An attribute of the style rather than one of its property elements,
    /// so it rides here instead of in [`Resolved`].
    master: Option<String>,
    /// `style:data-style-name`: the `number:*-style` a cell's value is displayed
    /// through. Also an attribute of the style rather than of a property element.
    data_style: Option<String>,
    props: Resolved,
}

/// One `style:master-page`: what it points at, and whether it asks for the two
/// things this renderer has no page to put them on.
#[derive(Debug, Clone, Default)]
struct MasterPage {
    /// `style:page-layout-name`.
    layout: Option<String>,
    /// `draw:style-name` — the `drawing-page` style holding the slide background.
    drawing_page: Option<String>,
    has_header: bool,
    has_footer: bool,
}

/// A master page's geometry, plus the two facts about it that are not geometry.
///
/// Neither `Debug` nor `Clone` is derived, because `docshape::Page` derives
/// neither; the hand-written [`Clone`] below is the whole cost of that.
pub struct PageSetup {
    pub page: Page,
    /// `style:print-orientation="landscape"`.
    ///
    /// **Not applied to the geometry**, and not to be: `fo:page-width` and
    /// `fo:page-height` already come out in the orientation the document is in
    /// (LibreOffice's own landscape layouts state the wide extent as the width),
    /// and a reader that swaps them on top of that renders every landscape page
    /// sideways. Verified against a producer: given a portrait width/height *and*
    /// `style:print-orientation="landscape"`, LibreOffice lays the page out
    /// portrait — the attribute is a print hint. Exposed only so a caller can say
    /// so.
    pub landscape: bool,
    /// The master page defines a `style:header` that is not switched off. Nothing
    /// renders it — a header belongs to a page, and this is one scrolling column,
    /// the same limit the docx renderer has — so a caller surfaces an honest note
    /// instead.
    pub has_header: bool,
    pub has_footer: bool,
    /// `style:master-page@draw:style-name`, for [`Styles::resolve`] against
    /// [`Family::DrawingPage`].
    pub drawing_page_style: Option<String>,
    /// A page layout stated both extents, i.e. [`PageSetup::page`] is the
    /// document's own geometry rather than the Letter default.
    ///
    /// A text column can use the default and be roughly right; a *slide* cannot —
    /// a presentation laid out on 8.5×11in paper is the wrong shape, and the slide
    /// renderer has a 4:3 canvas of its own to fall back to. So the fallback has to
    /// be the caller's choice, which means the caller has to be able to tell.
    pub stated: bool,
}

impl Clone for PageSetup {
    fn clone(&self) -> PageSetup {
        PageSetup {
            page: Page {
                width: self.page.width,
                height: self.page.height,
                left: self.page.left,
                right: self.page.right,
                top: self.page.top,
                bottom: self.page.bottom,
            },
            landscape: self.landscape,
            has_header: self.has_header,
            has_footer: self.has_footer,
            drawing_page_style: self.drawing_page_style.clone(),
            stated: self.stated,
        }
    }
}

impl Default for PageSetup {
    fn default() -> PageSetup {
        PageSetup {
            page: Page::default(),
            landscape: false,
            has_header: false,
            has_footer: false,
            drawing_page_style: None,
            stated: false,
        }
    }
}

/// The parsed style layer of a document: `styles.xml` and `content.xml`'s
/// automatic styles, in one keyspace.
pub struct Styles {
    /// `(family, name)` → declarations, in two levels so a lookup costs no
    /// allocation.
    styles: HashMap<Family, HashMap<String, RawStyle>>,
    /// `style:default-style` per family — the root every chain in that family
    /// starts from.
    defaults: HashMap<Family, Resolved>,
    /// `office:font-face-decls`: declaration name → real family.
    faces: HashMap<String, String>,
    layouts: HashMap<String, PageSetup>,
    masters: HashMap<String, MasterPage>,
    /// Document order, so a body that names no master page (or names one that is
    /// not there) can still get the document's own geometry.
    first_master: Option<String>,
    /// Chains are walked per element and a document reuses a handful of styles
    /// thousands of times, so each resolved chain is computed once. `RefCell`
    /// rather than a `&mut self` API: resolution is logically a read, and the
    /// renderer holds `Styles` immutably while walking the body.
    memo: RefCell<HashMap<Family, HashMap<String, Rc<Resolved>>>>,
}

impl Styles {
    /// No styles at all — what a package whose parts cannot be parsed gets.
    pub fn empty() -> Styles {
        Styles {
            styles: HashMap::new(),
            defaults: HashMap::new(),
            faces: HashMap::new(),
            layouts: HashMap::new(),
            masters: HashMap::new(),
            first_master: None,
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// Parses `styles.xml` (absent for a package that ships none) and
    /// `content.xml`.
    ///
    /// **Never fails**: a malformed part contributes nothing and the other one
    /// still lands, because a document whose stylesheet cannot be read still
    /// renders — with the defaults — and refusing to preview it at all is the
    /// worse answer. `styles.xml` is read first so that a `content.xml` automatic
    /// style wins a name collision, which is the layering ODF intends.
    pub fn parse(styles_xml: Option<&str>, content_xml: &str) -> Styles {
        let mut s = Styles::empty();
        // Font faces first: a `style:font-name` in either file resolves against
        // the declarations of both, so they have to be indexed before any
        // `style:text-properties` is read.
        let mut docs = Vec::new();
        for src in [styles_xml, Some(content_xml)] {
            match src.map(super::super::xml::parse) {
                Some(Ok(doc)) => docs.push(doc),
                _ => continue,
            }
        }
        for doc in &docs {
            s.load_faces(doc.root_element());
        }
        for doc in &docs {
            s.load(doc.root_element());
        }
        s
    }

    fn load_faces(&mut self, root: Node) {
        for container in elems(root).filter(|e| e.tag_name().name() == "font-face-decls") {
            for face in elems(container).filter(|e| e.tag_name().name() == "font-face") {
                let Some(name) = name_of(face, "name") else {
                    continue;
                };
                // The declaration's own name is the fallback family: producers do
                // write `style:font-face` with no `svg:font-family`, and the name
                // is then the family (with a disambiguating suffix at worst).
                let family = attr_local(face, "font-family")
                    .and_then(|v| {
                        let first = v.split(',').next()?.trim().trim_matches(['\'', '"']);
                        (!first.is_empty())
                            .then(|| first.chars().take(MAX_NAME_CHARS).collect::<String>())
                    })
                    .unwrap_or_else(|| name.clone());
                if self.faces.len() < MAX_STYLES {
                    self.faces.insert(name, family);
                }
            }
        }
    }

    /// The three style containers of one part. `office:styles` holds the named
    /// styles and the family defaults, `office:automatic-styles` the one-off
    /// styles and the page layouts, `office:master-styles` the master pages.
    fn load(&mut self, root: Node) {
        for container in elems(root) {
            match container.tag_name().name() {
                "styles" | "automatic-styles" => {
                    for e in elems(container) {
                        match e.tag_name().name() {
                            "style" => self.load_style(e),
                            "default-style" => self.load_default(e),
                            "page-layout" => self.load_layout(e),
                            // `text:list-style`, `number:*-style`, `draw:gradient`
                            // and the rest are other passes' business.
                            _ => {}
                        }
                    }
                }
                "master-styles" => {
                    for e in elems(container).filter(|e| e.tag_name().name() == "master-page") {
                        self.load_master(e);
                    }
                }
                _ => {}
            }
        }
    }

    fn count(&self) -> usize {
        self.styles.values().map(|m| m.len()).sum()
    }

    fn load_style(&mut self, e: Node) {
        if self.count() >= MAX_STYLES {
            return;
        }
        let Some(family) = attr_local(e, "family").and_then(family_of) else {
            return;
        };
        let Some(name) = name_of(e, "name") else { return };
        let raw = RawStyle {
            parent: name_of(e, "parent-style-name"),
            master: name_of(e, "master-page-name"),
            data_style: name_of(e, "data-style-name"),
            props: parse_props(e, &self.faces),
        };
        self.styles.entry(family).or_default().insert(name, raw);
    }

    fn load_default(&mut self, e: Node) {
        if let Some(family) = attr_local(e, "family").and_then(family_of) {
            let props = parse_props(e, &self.faces);
            self.defaults.insert(family, props);
        }
    }

    fn load_layout(&mut self, e: Node) {
        let Some(name) = name_of(e, "name") else { return };
        if self.layouts.len() >= MAX_STYLES {
            return;
        }
        let mut setup = PageSetup::default();
        if let Some(p) = child(e, "page-layout-properties") {
            setup.page = page_box(p);
            setup.stated = stated_extents(p);
            setup.landscape = attr_local(p, "print-orientation") == Some("landscape");
        }
        self.layouts.insert(name, setup);
    }

    fn load_master(&mut self, e: Node) {
        let Some(name) = name_of(e, "name") else { return };
        if self.masters.len() >= MAX_STYLES {
            return;
        }
        // A `style:display="false"` header is defined and switched off, which is
        // not something to note.
        let shown = |n: Node| flag(n, "display").unwrap_or(true);
        let mut m = MasterPage {
            layout: name_of(e, "page-layout-name"),
            drawing_page: name_of(e, "style-name"),
            has_header: false,
            has_footer: false,
        };
        for part in elems(e) {
            match part.tag_name().name() {
                "header" | "header-left" | "header-first" => m.has_header |= shown(part),
                "footer" | "footer-left" | "footer-first" => m.has_footer |= shown(part),
                _ => {}
            }
        }
        if self.first_master.is_none() {
            self.first_master = Some(name.clone());
        }
        self.masters.insert(name, m);
    }

    /// The family's `style:default-style` plus `name`'s whole
    /// `style:parent-style-name` chain, root first. An empty `name` — or one no
    /// style defines — resolves to the family default alone, which is what an
    /// element that states no style of its own gets.
    pub fn resolve(&self, family: Family, name: &str) -> Rc<Resolved> {
        if let Some(hit) = self
            .memo
            .borrow()
            .get(&family)
            .and_then(|m| m.get(name))
            .cloned()
        {
            return hit;
        }
        let mut out = self.defaults.get(&family).cloned().unwrap_or_default();
        for raw in self.chain(family, name) {
            merge(&mut out, &raw.props);
        }
        let rc = Rc::new(out);
        self.memo
            .borrow_mut()
            .entry(family)
            .or_default()
            .insert(name.to_string(), rc.clone());
        rc
    }

    /// The `style:parent-style-name` chain of `name`, **root first**. Cycle-safe:
    /// a style already on the chain ends the walk, and [`MAX_PARENT`] bounds it
    /// even for a pathological graph the visited check somehow misses (it cannot,
    /// but the cost of the belt is one comparison per link).
    fn chain(&self, family: Family, name: &str) -> Vec<&RawStyle> {
        let Some(table) = self.styles.get(&family) else {
            return Vec::new();
        };
        let mut out: Vec<&RawStyle> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        let mut cur = name;
        while !cur.is_empty() && out.len() < MAX_PARENT {
            if seen.contains(&cur) {
                break;
            }
            seen.push(cur);
            // A parent no style defines is not an error: the chain ends there and
            // the family default still applies.
            let Some(raw) = table.get(cur) else { break };
            out.push(raw);
            cur = raw.parent.as_deref().unwrap_or("");
        }
        out.reverse();
        out
    }

    /// The master page a paragraph style starts, from the nearest link of its
    /// chain that names one.
    ///
    /// Most specific first: an automatic style is where a producer writes
    /// `style:master-page-name`, and a named parent that happens to carry one too
    /// must not win over it. `None` means the style says nothing, which is the
    /// normal state — the document's first master page then applies (see
    /// [`Styles::page_setup`]).
    pub fn master_page_of(&self, name: &str) -> Option<&str> {
        self.chain(Family::Paragraph, name)
            .iter()
            .rev()
            .find_map(|raw| raw.master.as_deref())
    }

    /// The number style a cell style displays its value through, from the nearest
    /// link of its chain that names one.
    ///
    /// Only reached for a cell that states a value and no display text: ODF stores
    /// the producer's own formatting beside the value, so the *stored* text is what
    /// a grid shows, and this is the fallback for a cell that was never displayed.
    pub fn data_style_of(&self, name: &str) -> Option<&str> {
        self.chain(Family::TableCell, name)
            .iter()
            .rev()
            .find_map(|raw| raw.data_style.as_deref())
    }

    /// The real family behind a `style:font-name`, or `None` when the document
    /// declares no such face.
    pub fn font_family(&self, name: &str) -> Option<&str> {
        self.faces.get(name).map(String::as_str)
    }

    /// The geometry of a master page: the one it names, else the document's first
    /// master page, else the Letter-with-inch-margins default. A `style:page-layout`
    /// the master points at but the document does not define degrades the same
    /// way.
    pub fn page_setup(&self, master: Option<&str>) -> PageSetup {
        let m = master
            .and_then(|n| self.masters.get(n))
            .or_else(|| self.first_master.as_deref().and_then(|n| self.masters.get(n)));
        let Some(m) = m else {
            return PageSetup::default();
        };
        let mut setup = m
            .layout
            .as_deref()
            .and_then(|n| self.layouts.get(n))
            .cloned()
            .unwrap_or_default();
        setup.has_header = m.has_header;
        setup.has_footer = m.has_footer;
        setup.drawing_page_style = m.drawing_page.clone();
        setup
    }

    /// The master pages a document defines, in document order of the first one
    /// only — enough for the tests that check parsing rather than rendering.
    #[cfg(test)]
    fn first_master(&self) -> Option<&str> {
        self.first_master.as_deref()
    }
}

/// One `style:page-layout-properties` as a page box, clamped the way the docx
/// section geometry is: the same `docshape` bounds, so the two dialects cannot
/// disagree about what a page may be.
/// Whether a page layout states both extents in range, which is what makes
/// [`PageSetup::page`] the document's geometry rather than the default's.
fn stated_extents(p: Node) -> bool {
    ["page-width", "page-height"]
        .iter()
        .all(|n| len(p, n).is_some_and(|v| (MIN_PAGE_PX..=MAX_PAGE_PX).contains(&v)))
}

fn page_box(p: Node) -> Page {
    let mut page = Page::default();
    let extent = |name: &str| len(p, name).filter(|v| (MIN_PAGE_PX..=MAX_PAGE_PX).contains(v));
    // Both or neither: half a stated page size is not a page, and mixing one real
    // dimension with a Letter default gives an aspect nothing wrote.
    if let (Some(w), Some(h)) = (extent("page-width"), extent("page-height")) {
        page.width = w;
        page.height = h;
    }
    let limit = |extent: f32| ((extent - MIN_COLUMN_PX) / 2.0).clamp(0.0, MAX_MARGIN_PX);
    let all = len(p, "margin");
    let side = |name: &str, extent: f32, fallback: f32| {
        len(p, name)
            .or(all)
            .map(|v| v.clamp(0.0, limit(extent)))
            .unwrap_or(fallback)
    };
    page.left = side("margin-left", page.width, page.left);
    page.right = side("margin-right", page.width, page.right);
    page.top = side("margin-top", page.height, page.top);
    page.bottom = side("margin-bottom", page.height, page.bottom);
    page
}

// ── properties → model ───────────────────────────────────────────────────────

/// The size a text bag resolves to, in points, against the size it sits inside.
///
/// `base_pt` is what an unstated size inherits *and* what a percentage is a
/// percentage of. A caller with nothing to inherit from — the paragraph base
/// itself — passes [`DEFAULT_SIZE_PT`].
pub fn size_pt(t: &TextProps, base_pt: f32) -> f32 {
    match t.size {
        Some(FontSize::Pt(v)) => clamp_pt(v),
        Some(FontSize::Percent(p)) => clamp_pt(base_pt * p / 100.0),
        None => base_pt,
    }
}

/// The paragraph box, with no runs and no marker: the body walk fills those in,
/// and the list-style pass owns the marker.
///
/// `base_pt` is the size the paragraph's own text properties resolve to, which is
/// what sizes an empty line and what each run's size is compared against.
/// `heading` comes from `text:h@text:outline-level`, which is the body walk's to
/// read — a style does not know whether the element using it is a heading.
pub fn to_para(p: &ParaProps, base_pt: f32, heading: Option<u8>) -> Para {
    Para {
        runs: Vec::new(),
        size_pt: base_pt,
        align: p.align,
        indent_px: p.indent_px.unwrap_or(0.0),
        indent_end_px: p.indent_end_px.unwrap_or(0.0),
        // `style:auto-text-indent` asks for an indent derived from the font, and
        // nothing here measures text — the same reason explicit tab stops are not
        // honoured — so it drops the stated value rather than inventing an em.
        first_line_px: if p.auto_text_indent == Some(true) {
            0.0
        } else {
            p.first_line_px.unwrap_or(0.0)
        },
        line: line_height(p, base_pt),
        space_before_px: p.space_before_px.unwrap_or(0.0),
        space_after_px: p.space_after_px.unwrap_or(0.0),
        marker: None,
        rtl: p.rtl == Some(true),
        shade: p.background,
        borders: to_borders(&p.borders),
        heading,
    }
}

/// The three ODF line-height spellings as the model's one enum.
fn line_height(p: &ParaProps, base_pt: f32) -> LineHeight {
    match p.line {
        None => LineHeight::default(),
        // The model's multiplier is against the font's *line*, not the em box, so
        // the ratio carries `SINGLE_LINE` with it — as in the docx path.
        Some(LineSpec::Proportional(r)) => LineHeight::Multiple(r * model::SINGLE_LINE),
        Some(LineSpec::Fixed(px)) if px > 0.0 => LineHeight::Exact(px),
        Some(LineSpec::AtLeast(px)) if px > 0.0 => LineHeight::AtLeast(px),
        // Leading is added to a single line rather than being one, and this is the
        // first place the paragraph's own size is known, which is why the sum is
        // computed here instead of at parse time. Negative leading tightens the
        // line, so the total is floored at something a glyph can sit in.
        Some(LineSpec::Leading(px)) => {
            LineHeight::Exact((pt_to_px(base_pt) * model::SINGLE_LINE + px).max(1.0))
        }
        // A zero or negative height is not a line: fall back to single spacing.
        Some(_) => LineHeight::default(),
    }
}

/// The four edges as the model states them. [`Edge::None`] becomes the model's
/// absent border, which is the right answer either way — by the time this runs,
/// the cascade has already decided that this edge draws nothing.
pub fn to_borders(s: &Sides) -> Borders {
    if s.is_none() {
        return Borders::default();
    }
    let side = |e: Option<Edge>| match e {
        Some(Edge::Set(b)) => Some(Border {
            width_px: b.width_px,
            style: b.style,
            color: b.color,
            // ODF states the gap between a border and its text as `fo:padding`,
            // which is a property of the box rather than of the edge; the cell and
            // graphic bags carry it, and a renderer emits it as padding.
            space_px: 0.0,
        }),
        _ => None,
    };
    Borders {
        top: side(s.top),
        right: side(s.right),
        bottom: side(s.bottom),
        left: side(s.left),
    }
}

/// One run's text with its resolved properties. `base_pt` is the paragraph's
/// size, which an unstated or percentage-stated run size resolves against.
pub fn to_text_run(text: String, t: &TextProps, base_pt: f32) -> TextRun {
    TextRun {
        text,
        // The model has no "inherit": a run that states no size carries the
        // paragraph's, so the emitter can tell the two apart.
        size_pt: size_pt(t, base_pt),
        bold: t.bold == Some(true),
        italic: t.italic == Some(true),
        underline: t.underline == Some(true),
        strike: t.strike == Some(true),
        color: t.color.and_then(ColorVal::color),
        font: t.font.clone(),
        caps: if t.caps == Some(true) {
            Some(Caps::All)
        } else if t.small_caps == Some(true) {
            Some(Caps::Small)
        } else {
            None
        },
        letter_spacing_pt: t.letter_spacing_pt.unwrap_or(0.0),
        script: match t.position {
            Some(Position::Super) => Some(Script::Super),
            Some(Position::Sub) => Some(Script::Sub),
            _ => None,
        },
        highlight: t.highlight,
        // A link is a property of the `text:a` *around* the run, so the body walk
        // fills it in — and it must be sanitized before it gets here (see
        // `model::TextRun::link`).
        link: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every prefix a fixture below uses. Declared on the root because roxmltree
    /// rejects an undeclared one — and because the lookups are all by local name,
    /// the URIs only have to be present, not correct per attribute.
    const NS: &str = concat!(
        r##" xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""##,
        r##" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0""##,
        r##" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0""##,
        r##" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""##,
        r##" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0""##,
        r##" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0""##,
        r##" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""##,
    );

    fn wrap(body: &str) -> String {
        format!("<office:document{NS}>{body}</office:document>")
    }

    /// A `styles.xml` body, with an empty `content.xml` beside it.
    fn parse_styles(body: &str) -> Styles {
        Styles::parse(Some(&wrap(body)), &wrap(""))
    }

    /// Named styles only — the common case for these fixtures.
    fn named(inner: &str) -> Styles {
        parse_styles(&format!("<office:styles>{inner}</office:styles>"))
    }

    /// Named styles in `styles.xml`, automatic styles in `content.xml`.
    fn layered(named: &str, auto: &str) -> Styles {
        Styles::parse(
            Some(&wrap(&format!("<office:styles>{named}</office:styles>"))),
            &wrap(&format!("<office:automatic-styles>{auto}</office:automatic-styles>")),
        )
    }

    fn close(a: f32, want: f32) -> bool {
        (a - want).abs() < 0.01
    }

    /// The absolute size a bag resolved to. The pt → px → pt round trip through
    /// `length` (one parser for every unit, so `pt` has no shortcut) is exact only
    /// for multiples of 4/3 pt, which is why these compare with a tolerance.
    fn pt(size: Option<FontSize>) -> f32 {
        match size {
            Some(FontSize::Pt(v)) => v,
            other => panic!("expected an absolute size, got {other:?}"),
        }
    }

    // ── resolution ───────────────────────────────────────────────────────────

    #[test]
    fn a_three_deep_chain_merges_root_first() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Root">
                 <style:paragraph-properties fo:text-align="start" fo:margin-left="1cm"/>
                 <style:text-properties fo:font-size="10pt" fo:font-style="italic"/>
               </style:style>
               <style:style style:family="paragraph" style:name="Mid"
                            style:parent-style-name="Root">
                 <style:paragraph-properties fo:text-align="center"/>
                 <style:text-properties fo:font-size="14pt"/>
               </style:style>
               <style:style style:family="paragraph" style:name="Leaf"
                            style:parent-style-name="Mid">
                 <style:paragraph-properties fo:text-align="end"/>
               </style:style>"##,
        );
        let r = s.resolve(Family::Paragraph, "Leaf");
        // Leaf wins the alignment, Mid the size, Root keeps the indent and the
        // italic nobody overrode.
        assert_eq!(r.para.align, Some(Align::Right));
        assert!(close(pt(r.text.size), 14.0));
        assert!(close(r.para.indent_px.expect("indent survives"), 37.795));
        assert_eq!(r.text.italic, Some(true));
        // The memo hands back the same resolution, not a fresh one.
        assert!(Rc::ptr_eq(&r, &s.resolve(Family::Paragraph, "Leaf")));
        // A name no style defines is not an error, and the family is part of the
        // key: the same name in another family resolves to nothing.
        assert_eq!(s.resolve(Family::Paragraph, "café").para.align, None);
        assert_eq!(s.resolve(Family::Text, "Leaf").para.align, None);
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="A" style:parent-style-name="C">
                 <style:paragraph-properties fo:text-align="start"/></style:style>
               <style:style style:family="paragraph" style:name="B" style:parent-style-name="A">
                 <style:text-properties fo:font-weight="bold"/></style:style>
               <style:style style:family="paragraph" style:name="C" style:parent-style-name="B">
                 <style:paragraph-properties fo:text-align="center"/></style:style>
               <style:style style:family="paragraph" style:name="Self"
                            style:parent-style-name="Self">
                 <style:text-properties fo:font-style="italic"/></style:style>"##,
        );
        // Every entry point of the cycle resolves, and each style in it still
        // contributes exactly once.
        for name in ["A", "B", "C"] {
            let r = s.resolve(Family::Paragraph, name);
            assert_eq!(r.text.bold, Some(true), "{name} lost B");
            assert!(r.para.align.is_some(), "{name} lost the alignment");
        }
        // A self-parented style is the degenerate cycle.
        assert_eq!(s.resolve(Family::Paragraph, "Self").text.italic, Some(true));
    }

    #[test]
    fn a_missing_parent_ends_the_chain_without_losing_the_style() {
        let s = named(
            r##"<style:default-style style:family="paragraph">
                 <style:text-properties fo:font-size="10pt"/></style:default-style>
               <style:style style:family="paragraph" style:name="Orphan"
                            style:parent-style-name="Gone">
                 <style:paragraph-properties fo:text-align="center"/></style:style>"##,
        );
        let r = s.resolve(Family::Paragraph, "Orphan");
        assert_eq!(r.para.align, Some(Align::Center));
        // The family default is still the root of the chain.
        assert!(close(pt(r.text.size), 10.0));
    }

    #[test]
    fn an_automatic_style_wins_a_name_collision() {
        let s = layered(
            r##"<style:style style:family="paragraph" style:name="P1">
                 <style:paragraph-properties fo:text-align="start" fo:margin-top="1cm"/>
               </style:style>
               <style:style style:family="paragraph" style:name="Base">
                 <style:text-properties fo:font-size="9pt"/></style:style>"##,
            r##"<style:style style:family="paragraph" style:name="P1"
                            style:parent-style-name="Base">
                 <style:paragraph-properties fo:text-align="center"/></style:style>"##,
        );
        let r = s.resolve(Family::Paragraph, "P1");
        // The automatic style replaces the named one outright — it is a different
        // style that happens to share a name, not an override of it — so the
        // margin the named one stated is gone and its own parent applies instead.
        assert_eq!(r.para.align, Some(Align::Center));
        assert_eq!(r.para.space_before_px, None);
        assert!(close(pt(r.text.size), 9.0));
    }

    #[test]
    fn a_default_style_roots_every_chain_of_its_family() {
        let s = named(
            r##"<style:default-style style:family="paragraph">
                 <style:paragraph-properties fo:margin-bottom="0.2cm"/>
                 <style:text-properties fo:font-size="11pt" style:font-name="Widget"/>
               </style:default-style>
               <style:default-style style:family="graphic">
                 <style:graphic-properties draw:fill="none"/></style:default-style>
               <style:style style:family="paragraph" style:name="Body">
                 <style:text-properties fo:font-size="13pt"/></style:style>"##,
        );
        // A style states the size; everything else comes from the default.
        let r = s.resolve(Family::Paragraph, "Body");
        assert!(close(pt(r.text.size), 13.0));
        assert!(close(r.para.space_after_px.expect("default spacing"), 7.559));
        assert_eq!(r.text.font_raw.as_deref(), Some("Widget"));
        // An element that names no style at all gets the default alone.
        let d = s.resolve(Family::Paragraph, "");
        assert!(close(pt(d.text.size), 11.0));
        assert_eq!(d.para.align, None);
        // Defaults are per family: the graphic one must not reach a paragraph.
        assert_eq!(d.graphic.fill.kind, None);
        assert_eq!(
            s.resolve(Family::Graphic, "").graphic.fill.kind,
            Some(FillKind::None)
        );
    }

    #[test]
    fn percentage_font_sizes_resolve_against_the_parents_resolved_size() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Root">
                 <style:text-properties fo:font-size="20pt"/></style:style>
               <style:style style:family="paragraph" style:name="Mid"
                            style:parent-style-name="Root">
                 <style:text-properties fo:font-size="150%"/></style:style>
               <style:style style:family="paragraph" style:name="Leaf"
                            style:parent-style-name="Mid">
                 <style:text-properties fo:font-size="50%"/></style:style>"##,
        );
        // 20pt → 150% of it → 50% of that. Nested, and resolved inside the
        // cascade rather than at emission.
        assert!(close(pt(s.resolve(Family::Paragraph, "Mid").text.size), 30.0));
        assert!(close(pt(s.resolve(Family::Paragraph, "Leaf").text.size), 15.0));

        // With no absolute size anywhere, the percentages compose and stay
        // relative — a text style is resolved on its own and only then merged over
        // the paragraph's size, and resolving early would lose that base.
        let rel = named(
            r##"<style:style style:family="text" style:name="Big">
                 <style:text-properties fo:font-size="200%"/></style:style>
               <style:style style:family="text" style:name="Half"
                            style:parent-style-name="Big">
                 <style:text-properties fo:font-size="50%"/></style:style>"##,
        );
        let t = rel.resolve(Family::Text, "Half");
        assert_eq!(t.text.size, Some(FontSize::Percent(100.0)));
        assert_eq!(size_pt(&t.text, 18.0), 18.0);
        assert_eq!(size_pt(&rel.resolve(Family::Text, "Big").text, 18.0), 36.0);
        // And an unstated size is the base itself.
        assert_eq!(size_pt(&TextProps::default(), 18.0), 18.0);
    }

    #[test]
    fn the_style_count_and_chain_length_caps_hold() {
        let mut xml = String::new();
        for i in 0..(MAX_STYLES + 8) {
            xml.push_str(&format!(
                r##"<style:style style:family="paragraph" style:name="S{i}">
                     <style:paragraph-properties fo:text-align="center"/></style:style>"##
            ));
        }
        let s = named(&xml);
        assert_eq!(s.resolve(Family::Paragraph, "S0").para.align, Some(Align::Center));
        assert_eq!(
            s.resolve(Family::Paragraph, &format!("S{MAX_STYLES}")).para.align,
            None,
            "the cap must drop the remainder"
        );

        // A chain longer than the cap keeps the links nearest the leaf: the 16th
        // ancestor still contributes and the 17th does not.
        let mut xml = String::new();
        for i in 0..20 {
            let parent = if i == 0 {
                String::new()
            } else {
                format!(r##" style:parent-style-name="L{}""##, i - 1)
            };
            let props = match i {
                3 => r##"<style:paragraph-properties fo:text-align="center"/>"##,
                4 => r##"<style:text-properties fo:font-style="italic"/>"##,
                _ => "",
            };
            xml.push_str(&format!(
                r##"<style:style style:family="paragraph" style:name="L{i}"{parent}>{props}</style:style>"##
            ));
        }
        let s = named(&xml);
        let r = s.resolve(Family::Paragraph, "L19");
        assert_eq!(r.text.italic, Some(true), "L4 is the 16th link and must count");
        assert_eq!(r.para.align, None, "L3 is past the cap");
    }

    #[test]
    fn a_malformed_part_degrades_without_taking_the_other_one_with_it() {
        let s = Styles::parse(
            Some("not xml at all"),
            &wrap(
                r##"<office:automatic-styles>
                     <style:style style:family="paragraph" style:name="P1">
                       <style:paragraph-properties fo:text-align="center"/></style:style>
                   </office:automatic-styles>"##,
            ),
        );
        assert_eq!(s.resolve(Family::Paragraph, "P1").para.align, Some(Align::Center));
        // And the other way round, plus the no-styles-at-all case.
        let s = Styles::parse(
            Some(&wrap(
                r##"<office:styles><style:style style:family="text" style:name="T1">
                     <style:text-properties fo:font-weight="bold"/></style:style></office:styles>"##,
            )),
            "<office:document",
        );
        assert_eq!(s.resolve(Family::Text, "T1").text.bold, Some(true));
        assert_eq!(*Styles::empty().resolve(Family::Text, "T1"), Resolved::default());
        // A style with no name, or in a family nothing renders, is skipped rather
        // than poisoning the table.
        let s = named(
            r##"<style:style style:family="paragraph"><style:text-properties fo:font-weight="bold"/></style:style>
               <style:style style:family="chart" style:name="C1">
                 <style:text-properties fo:font-weight="bold"/></style:style>"##,
        );
        assert_eq!(s.resolve(Family::Paragraph, "").text.bold, None);
    }

    // ── property bags ────────────────────────────────────────────────────────

    #[test]
    fn text_properties_reach_the_model_and_merge_field_by_field() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Base">
                 <style:text-properties fo:font-size="12pt" fo:font-weight="bold"
                   fo:color="#2f5496" fo:background-color="#ffff00"
                   style:text-underline-style="solid" style:text-line-through-style="solid"
                   fo:letter-spacing="1.5pt" style:text-position="33% 58%"
                   fo:text-transform="uppercase" fo:language="en" fo:country="GB"/>
               </style:style>
               <style:style style:family="paragraph" style:name="Quiet"
                            style:parent-style-name="Base">
                 <style:text-properties fo:font-weight="normal"
                   style:text-underline-style="none" fo:text-transform="none"
                   fo:font-variant="small-caps" style:use-window-font-color="true"
                   fo:letter-spacing="normal" style:text-position="0% 100%"/>
               </style:style>"##,
        );
        let t = &s.resolve(Family::Paragraph, "Base").text;
        assert_eq!(t.bold, Some(true));
        assert_eq!(t.underline, Some(true));
        assert_eq!(t.strike, Some(true));
        assert_eq!(t.caps, Some(true));
        assert_eq!(t.color, Some(ColorVal::Set(Color::from_rgb(0x2f_5496))));
        assert_eq!(t.highlight, Some(Color::from_rgb(0xff_ff_00)));
        assert_eq!(t.position, Some(Position::Super));
        assert_eq!(t.language.as_deref(), Some("en-GB"));
        assert!(close(t.letter_spacing_pt.expect("spacing"), 1.5));
        let run = to_text_run("café".to_string(), t, 10.0);
        assert_eq!(run.size_pt, 12.0);
        assert!(run.bold && run.underline && run.strike);
        assert_eq!(run.caps, Some(Caps::All));
        assert_eq!(run.script, Some(Script::Super));
        assert_eq!(run.color, Some(Color::from_rgb(0x2f_5496)));

        // Every off switch is a statement that cancels the inherited on, and the
        // properties nobody restated survive.
        let t = &s.resolve(Family::Paragraph, "Quiet").text;
        assert_eq!(t.bold, Some(false));
        assert_eq!(t.underline, Some(false));
        assert_eq!(t.strike, Some(true), "the untouched decoration must survive");
        assert_eq!(t.caps, Some(false));
        assert_eq!(t.small_caps, Some(true));
        assert_eq!(t.color, Some(ColorVal::Auto));
        assert_eq!(t.position, Some(Position::Baseline));
        assert!(close(pt(t.size), 12.0));
        let run = to_text_run("naïve".to_string(), t, 10.0);
        assert!(!run.bold && !run.underline && run.strike);
        assert_eq!(run.caps, Some(Caps::Small));
        // `use-window-font-color` is the reader's colour, which the model spells
        // as no colour at all.
        assert_eq!(run.color, None);
        assert_eq!(run.script, None);
        assert_eq!(run.letter_spacing_pt, 0.0);

        // `transparent` is a stated band that cancels an inherited one.
        let s = named(
            r##"<style:style style:family="text" style:name="Clear">
                 <style:text-properties fo:background-color="transparent"/></style:style>"##,
        );
        let t = &s.resolve(Family::Text, "Clear").text;
        assert_eq!(t.highlight.map(|c| c.alpha), Some(0.0));
    }

    #[test]
    fn paragraph_properties_reach_the_model_box() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Body">
                 <style:paragraph-properties fo:margin-top="0.2cm" fo:margin-bottom="0.4cm"
                   fo:margin-left="1cm" fo:margin-right="0.5cm" fo:text-indent="-0.5cm"
                   fo:text-align="justify" fo:background-color="#eeeeee"
                   style:contextual-spacing="true" fo:keep-with-next="always"
                   style:tab-stop-distance="1.25cm">
                   <style:tab-stops>
                     <style:tab-stop style:position="2cm" style:type="center"/>
                     <style:tab-stop style:position="4cm"/>
                   </style:tab-stops>
                 </style:paragraph-properties>
               </style:style>
               <style:style style:family="paragraph" style:name="Tight"
                            style:parent-style-name="Body">
                 <style:paragraph-properties fo:margin-top="0cm"/></style:style>"##,
        );
        let p = &s.resolve(Family::Paragraph, "Body").para;
        assert_eq!(p.align, Some(Align::Justify));
        assert_eq!(p.contextual_spacing, Some(true));
        assert_eq!(p.keep_with_next, Some(true));
        assert_eq!(p.tab_stops.len(), 2);
        assert!(close(p.tab_stops[0], 75.5906));
        assert!(close(p.tab_stop_distance_px.expect("default stop"), 47.2441));
        let para = to_para(p, 12.0, Some(2));
        assert!(close(para.space_before_px, 7.5591));
        assert!(close(para.space_after_px, 15.1181));
        assert!(close(para.indent_px, 37.7953));
        assert!(close(para.indent_end_px, 18.8976));
        // ODF states a hanging indent as a negative `fo:text-indent`, which is
        // already the model's sign convention — nothing to fold or negate.
        assert!(close(para.first_line_px, -18.8976));
        assert_eq!(para.shade, Some(Color::from_rgb(0xee_ee_ee)));
        assert_eq!(para.size_pt, 12.0);
        assert_eq!(para.heading, Some(2));

        // A child that restates one margin keeps the rest.
        let p = &s.resolve(Family::Paragraph, "Tight").para;
        assert_eq!(p.space_before_px, Some(0.0));
        assert!(close(p.space_after_px.expect("inherited"), 15.1181));
        assert_eq!(p.tab_stops.len(), 2, "the inherited stop list survives");

        // `style:auto-text-indent` drops the stated indent: the automatic value is
        // font-metric-derived and nothing here measures text.
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Auto">
                 <style:paragraph-properties fo:text-indent="1cm"
                   style:auto-text-indent="true"/></style:style>"##,
        );
        let p = &s.resolve(Family::Paragraph, "Auto").para;
        assert_eq!(to_para(p, 12.0, None).first_line_px, 0.0);
    }

    #[test]
    fn line_height_reads_all_three_spellings() {
        let with = |attrs: &str| {
            let s = named(&format!(
                r##"<style:style style:family="paragraph" style:name="P">
                     <style:paragraph-properties {attrs}/></style:style>"##
            ));
            let r = s.resolve(Family::Paragraph, "P");
            to_para(&r.para, 12.0, None).line
        };
        // A percentage is a multiple of a single line.
        assert_eq!(
            with(r##"fo:line-height="150%""##),
            LineHeight::Multiple(1.5 * model::SINGLE_LINE)
        );
        assert_eq!(
            with(r##"fo:line-height="normal""##),
            LineHeight::Multiple(model::SINGLE_LINE)
        );
        // A length is the whole line.
        match with(r##"fo:line-height="0.5cm""##) {
            LineHeight::Exact(px) => assert!(close(px, 18.8976)),
            other => panic!("{other:?}"),
        }
        // A floor, which the emitter keeps as a `max()`.
        match with(r##"style:line-height-at-least="0.6cm""##) {
            LineHeight::AtLeast(px) => assert!(close(px, 22.6772)),
            other => panic!("{other:?}"),
        }
        // Leading is *added* to a single line, so the total needs the paragraph's
        // own size — 12pt at 1.2 lines is 19.2px, plus 0.2cm.
        match with(r##"style:line-spacing="0.2cm""##) {
            LineHeight::Exact(px) => assert!(close(px, 19.2 + 7.5591), "{px}"),
            other => panic!("{other:?}"),
        }
        // Negative leading tightens rather than inverting the line.
        match with(r##"style:line-spacing="-0.1cm""##) {
            LineHeight::Exact(px) => assert!(close(px, 19.2 - 3.7795), "{px}"),
            other => panic!("{other:?}"),
        }
        // Nothing stated, and a nonsensical zero, are both single spacing.
        assert_eq!(with(""), LineHeight::default());
        assert_eq!(with(r##"fo:line-height="0cm""##), LineHeight::default());
    }

    #[test]
    fn a_border_shorthand_seeds_four_edges_and_a_side_overrides_one() {
        let s = named(
            r##"<style:style style:family="paragraph" style:name="Boxed">
                 <style:paragraph-properties fo:border="0.5pt solid #000000"
                   fo:border-bottom="1.76pt dashed #ff0000"/></style:style>
               <style:style style:family="paragraph" style:name="Open"
                            style:parent-style-name="Boxed">
                 <style:paragraph-properties fo:border-top="none"/></style:style>"##,
        );
        let b = to_borders(&s.resolve(Family::Paragraph, "Boxed").para.borders);
        for side in [b.top, b.right, b.left] {
            let e = side.expect("the shorthand seeds every edge");
            assert_eq!((e.width_px, e.style, e.color), (1.0, "solid", Some(Color::from_rgb(0))));
            assert_eq!(e.space_px, 0.0);
        }
        let bottom = b.bottom.expect("per-side override");
        assert_eq!(bottom.style, "dashed");
        assert_eq!(bottom.color, Some(Color::from_rgb(0xff_00_00)));
        assert!(close(bottom.width_px, 2.3467));

        // `none` is a statement: it cancels the inherited edge and leaves the
        // other three alone.
        let sides = &s.resolve(Family::Paragraph, "Open").para.borders;
        assert_eq!(sides.top, Some(Edge::None));
        let b = to_borders(sides);
        assert!(b.top.is_none());
        assert!(b.right.is_some() && b.bottom.is_some() && b.left.is_some());
        // No border anywhere costs no allocation and no declaration.
        assert_eq!(to_borders(&Sides::default()), Borders::default());
    }

    #[test]
    fn a_writing_mode_of_rl_tb_is_a_right_to_left_paragraph() {
        let with = |mode: &str| {
            let s = named(&format!(
                r##"<style:style style:family="paragraph" style:name="P">
                     <style:paragraph-properties style:writing-mode="{mode}"/></style:style>"##
            ));
            let r = s.resolve(Family::Paragraph, "P");
            (r.para.rtl, to_para(&r.para, 12.0, None).rtl)
        };
        assert_eq!(with("rl-tb"), (Some(true), true));
        assert_eq!(with("lr-tb"), (Some(false), false));
        // A vertical mode is not a direction, so it states nothing here.
        assert_eq!(with("tb-rl"), (None, false));
    }

    #[test]
    fn a_font_name_resolves_through_the_face_declarations() {
        let s = parse_styles(
            r##"<office:font-face-decls>
                 <style:font-face style:name="Liberation Serif1" svg:font-family="'Liberation Serif'"/>
                 <style:font-face style:name="Widget Sans"/>
               </office:font-face-decls>
               <office:styles>
                 <style:style style:family="text" style:name="Named">
                   <style:text-properties style:font-name="Liberation Serif1"/></style:style>
                 <style:style style:family="text" style:name="Bare">
                   <style:text-properties style:font-name="Widget Sans"/></style:style>
                 <style:style style:family="text" style:name="Direct">
                   <style:text-properties fo:font-family="&apos;Widget Serif&apos;, serif"/></style:style>
                 <style:style style:family="text" style:name="Unknown">
                   <style:text-properties style:font-name="Nothing Declared"/></style:style>
               </office:styles>"##,
        );
        assert_eq!(s.font_family("Liberation Serif1"), Some("Liberation Serif"));
        // A declaration with no `svg:font-family` is its own family.
        assert_eq!(s.font_family("Widget Sans"), Some("Widget Sans"));
        assert_eq!(s.font_family("café"), None);

        let raw = |name: &str| s.resolve(Family::Text, name).text.font_raw.clone();
        assert_eq!(raw("Named").as_deref(), Some("Liberation Serif"));
        assert_eq!(raw("Bare").as_deref(), Some("Widget Sans"));
        // `fo:font-family` states the family itself, quotes and list and all.
        assert_eq!(raw("Direct").as_deref(), Some("Widget Serif"));
        // An undeclared name is still a name — better a family that may not be
        // installed than no font at all.
        assert_eq!(raw("Unknown").as_deref(), Some("Nothing Declared"));
        // The stack is what reaches CSS.
        let font = s.resolve(Family::Text, "Named").text.font.clone();
        assert_eq!(font, Some(fonts::css_font_stack("Liberation Serif")));
    }

    #[test]
    fn cell_column_row_and_table_properties_merge() {
        let s = named(
            r##"<style:style style:family="table-cell" style:name="Cell">
                 <style:table-cell-properties fo:padding="0.1cm" fo:padding-left="0.3cm"
                   fo:border="0.5pt solid #000000" fo:background-color="#dddddd"
                   style:vertical-align="middle" fo:wrap-option="wrap"
                   style:shrink-to-fit="true" style:rotation-angle="90"/>
               </style:style>
               <style:style style:family="table-cell" style:name="Plain"
                            style:parent-style-name="Cell">
                 <style:table-cell-properties fo:background-color="#ffffff"/></style:style>
               <style:style style:family="table-column" style:name="Col">
                 <style:table-column-properties style:column-width="2.5cm"
                   style:use-optimal-column-width="true"/></style:style>
               <style:style style:family="table-row" style:name="Row">
                 <style:table-row-properties style:row-height="0.5cm"
                   style:use-optimal-row-height="false"/></style:style>
               <style:style style:family="table" style:name="Tbl">
                 <style:table-properties style:width="16cm" table:display="true"
                   table:align="margins"/></style:style>"##,
        );
        let c = s.resolve(Family::TableCell, "Cell").cell;
        assert!(close(c.padding.top.expect("shorthand"), 3.7795));
        assert!(close(c.padding.left.expect("override"), 11.3386));
        assert_eq!(c.v_align, Some("center"));
        assert_eq!(c.wrap, Some(true));
        assert_eq!(c.shrink_to_fit, Some(true));
        assert_eq!(c.rotation, Some(Rotation::Ccw(90.0)));
        assert_eq!(c.background, Some(Color::from_rgb(0xdd_dd_dd)));
        assert!(c.borders.top.is_some());

        // Only the fill changes; the padding, borders and alignment are inherited.
        let c = s.resolve(Family::TableCell, "Plain").cell;
        assert_eq!(c.background, Some(Color::from_rgb(0xff_ff_ff)));
        assert_eq!(c.v_align, Some("center"));
        assert!(c.padding.left.is_some() && c.borders.left.is_some());

        let col = s.resolve(Family::TableColumn, "Col").column;
        assert!(close(col.width_px.expect("width"), 94.4882));
        assert_eq!(col.optimal, Some(true));
        let row = s.resolve(Family::TableRow, "Row").row;
        assert!(close(row.height_px.expect("height"), 18.8976));
        assert_eq!(row.optimal, Some(false));
        let t = s.resolve(Family::Table, "Tbl").table;
        assert!(matches!(t.width, Some(Measure::Px(v)) if close(v, 604.7244)));
        assert_eq!(t.display, Some(true));
        assert_eq!(t.align, Some(TableAlign::Margins));
    }

    #[test]
    fn graphic_and_page_fills_become_drawingml_values() {
        use super::super::super::drawingml::fill::fill_css;
        use super::super::super::drawingml::line::line_css;

        let s = named(
            r##"<style:style style:family="graphic" style:name="Box">
                 <style:graphic-properties draw:fill="solid" draw:fill-color="#729fcf"
                   draw:opacity="50%" draw:stroke="dash" svg:stroke-color="#3465a4"
                   svg:stroke-width="0.08cm" svg:stroke-linecap="round"
                   draw:textarea-vertical-align="middle" draw:textarea-horizontal-align="center"
                   draw:auto-grow-height="true" draw:auto-grow-width="false"
                   fo:padding-left="0.25cm" fo:min-height="1cm" fo:min-width="2cm"
                   fo:wrap-option="no-wrap"/>
               </style:style>
               <style:style style:family="presentation" style:name="Recolour"
                            style:parent-style-name="Box">
                 <style:graphic-properties draw:fill-color="#ff0000"/></style:style>
               <style:style style:family="graphic" style:name="Empty">
                 <style:graphic-properties draw:fill="none" draw:stroke="none"/></style:style>
               <style:style style:family="graphic" style:name="Gradient">
                 <style:graphic-properties draw:fill="gradient"
                   draw:fill-gradient-name="Gradient_20_1"/></style:style>
               <style:style style:family="drawing-page" style:name="Mdp1">
                 <style:drawing-page-properties draw:fill="solid"
                   draw:fill-color="#191b0e"/></style:style>"##,
        );
        let g = s.resolve(Family::Graphic, "Box").graphic;
        // The fill lands as a `drawingml::Fill`, so `fill_css` does the emitting
        // and there is no second fill emitter here.
        match g.fill.fill() {
            Some(Fill::Solid(c)) => {
                assert_eq!(c.rgb, 0x72_9f_cf);
                assert!((c.alpha - 0.5).abs() < 0.001);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fill_css(&g.fill.fill().expect("fill")),
            "background-color:rgba(114, 159, 207, 0.5);"
        );
        let line = g.stroke.line().expect("stroke");
        assert_eq!(line.dash, Dash::Dashed);
        assert_eq!(line.cap, Cap::Round);
        assert!((line.width_px - 3.0236).abs() < 0.001);
        assert_eq!(line_css(&line), "border:3.02px dashed #3465a4;");
        assert_eq!(g.text_v_align, Some("center"));
        assert_eq!(g.text_h_align, Some("center"));
        assert_eq!((g.auto_grow_height, g.auto_grow_width), (Some(true), Some(false)));
        assert!(close(g.padding.left.expect("padding"), 9.4488));
        assert!(close(g.min_height_px.expect("min height"), 37.7953));
        assert!(close(g.min_width_px.expect("min width"), 75.5906));
        assert_eq!(g.wrap, Some(false));

        // A style that restates only the colour keeps the kind and the opacity:
        // the three attributes merge independently, so a bare colour cannot turn
        // an inherited `draw:fill="none"` back on.
        let g = s.resolve(Family::Presentation, "Recolour").graphic;
        assert!(matches!(g.fill.fill(), Some(Fill::Solid(c)) if c.rgb == 0xff_00_00));
        let off = s.resolve(Family::Graphic, "Empty").graphic;
        assert_eq!(off.fill.fill(), Some(Fill::None));
        // A stated `none` stroke is an explicit reset, not an absent one.
        assert_eq!(line_css(&off.stroke.line().expect("none is stated")), "border:none;");
        assert_eq!(StrokeProps::default().line(), None);

        // A gradient names a definition this pass does not index, so it reports
        // absence and the caller's own fill wins rather than being painted flat.
        assert_eq!(s.resolve(Family::Graphic, "Gradient").graphic.fill.fill(), None);

        // A drawing page carries the slide background and nothing else.
        let p = s.resolve(Family::DrawingPage, "Mdp1").page;
        assert_eq!(p.fill.fill(), Some(Fill::Solid(Color::from_rgb(0x19_1b_0e))));
    }

    // ── page geometry ────────────────────────────────────────────────────────

    #[test]
    fn page_geometry_comes_from_the_master_pages_layout() {
        let s = parse_styles(
            r##"<office:automatic-styles>
                 <style:page-layout style:name="PM1">
                   <style:page-layout-properties fo:page-width="21cm" fo:page-height="29.7cm"
                     fo:margin-top="2cm" fo:margin-bottom="2cm" fo:margin-left="1cm"
                     fo:margin-right="1cm" style:print-orientation="portrait"/>
                 </style:page-layout>
               </office:automatic-styles>
               <office:master-styles>
                 <style:master-page style:name="Standard" style:page-layout-name="PM1"
                                    draw:style-name="Mdp1">
                   <style:header><text:p>café</text:p></style:header>
                   <style:footer style:display="false"/>
                 </style:master-page>
                 <style:master-page style:name="Bare"/>
               </office:master-styles>"##,
        );
        let p = s.page_setup(Some("Standard"));
        assert!(close(p.page.width, 793.7008), "{}", p.page.width);
        assert!(close(p.page.height, 1122.5197));
        assert!(close(p.page.top, 75.5906) && close(p.page.left, 37.7953));
        assert!(!p.landscape);
        // A header is defined and cannot be rendered, so the fact is exposed; a
        // `style:display="false"` footer is switched off and is not a note.
        assert!(p.has_header);
        assert!(!p.has_footer);
        assert_eq!(p.drawing_page_style.as_deref(), Some("Mdp1"));

        // A master page naming no layout, and a name no master page defines, both
        // degrade to the default box rather than to nothing.
        assert!(close(s.page_setup(Some("Bare")).page.width, Page::default().width));
        assert_eq!(s.first_master(), Some("Standard"));
        // An unknown name falls back to the document's first master page, which is
        // also what a body that names none gets.
        assert!(close(s.page_setup(Some("café")).page.width, 793.7008));
        assert!(close(s.page_setup(None).page.width, 793.7008));
        assert!(s.page_setup(None).has_header);
        // No master pages at all: the Letter-with-inch-margins default.
        let bare = Styles::empty().page_setup(None);
        assert!(close(bare.page.width, Page::default().width));
        assert!(!bare.has_header && !bare.has_footer);
    }

    #[test]
    fn a_landscape_layout_states_its_own_wide_extent() {
        // Corpus shape: a landscape page layout already carries the wide extent as
        // `fo:page-width`, so `style:print-orientation` must **not** swap the two.
        // A producer agrees: given a portrait width/height beside
        // `print-orientation="landscape"`, LibreOffice lays the page out portrait.
        let s = parse_styles(
            r##"<office:automatic-styles>
                 <style:page-layout style:name="PM1">
                   <style:page-layout-properties fo:page-width="28cm" fo:page-height="15.75cm"
                     style:print-orientation="landscape" fo:margin-top="0cm"
                     fo:margin-bottom="0cm" fo:margin-left="0cm" fo:margin-right="0cm"/>
                 </style:page-layout>
                 <style:page-layout style:name="PM2">
                   <style:page-layout-properties fo:page-width="21cm" fo:page-height="29.7cm"
                     style:print-orientation="landscape"/>
                 </style:page-layout>
               </office:automatic-styles>
               <office:master-styles>
                 <style:master-page style:name="Slide" style:page-layout-name="PM1"/>
                 <style:master-page style:name="Odd" style:page-layout-name="PM2"/>
               </office:master-styles>"##,
        );
        let p = s.page_setup(Some("Slide"));
        assert!(p.landscape);
        assert!(p.page.width > p.page.height, "the stated extents are kept as stated");
        assert!(close(p.page.width, 1058.2677) && close(p.page.height, 595.2756));
        assert_eq!((p.page.left, p.page.top), (0.0, 0.0));
        // The attribute is a print hint, so a contradiction resolves in favour of
        // the geometry rather than of the orientation.
        let p = s.page_setup(Some("Odd"));
        assert!(p.landscape);
        assert!(p.page.height > p.page.width);
    }

    #[test]
    fn absurd_page_geometry_is_clamped_to_the_shared_bounds() {
        let with = |props: &str| {
            let s = parse_styles(&format!(
                r##"<office:automatic-styles>
                     <style:page-layout style:name="PM1">
                       <style:page-layout-properties {props}/></style:page-layout>
                   </office:automatic-styles>
                   <office:master-styles>
                     <style:master-page style:name="S" style:page-layout-name="PM1"/>
                   </office:master-styles>"##
            ));
            s.page_setup(Some("S")).page
        };
        // Both or neither: a page too small to hold a column keeps the default.
        let p = with(r##"fo:page-width="0.5cm" fo:page-height="29.7cm""##);
        assert!(close(p.width, Page::default().width) && close(p.height, Page::default().height));
        let p = with(r##"fo:page-width="200cm" fo:page-height="200cm""##);
        assert!(close(p.width, Page::default().width));
        // A margin never eats the whole page, nor more than `MAX_MARGIN_PX`.
        let p = with(r##"fo:page-width="21cm" fo:page-height="29.7cm" fo:margin="40cm""##);
        assert!(close(p.left, (793.7008 - MIN_COLUMN_PX) / 2.0), "{}", p.left);
        assert!(p.top <= MAX_MARGIN_PX);
        // A negative margin is no margin, and `fo:margin` seeds all four sides.
        let p = with(r##"fo:page-width="21cm" fo:page-height="29.7cm" fo:margin="-3cm""##);
        assert_eq!((p.left, p.right, p.top, p.bottom), (0.0, 0.0, 0.0, 0.0));
        let p = with(r##"fo:page-width="21cm" fo:page-height="29.7cm" fo:margin="1cm" fo:margin-left="2cm""##);
        assert!(close(p.left, 75.5906) && close(p.right, 37.7953));
    }
}
