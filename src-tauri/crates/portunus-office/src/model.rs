//! The format-neutral paragraph model, and its HTML.
//!
//! Between "parse a format's markup" and "write HTML" sits one vocabulary:
//! paragraphs of runs carrying *resolved* presentational values — a size in
//! points, a colour, a hanging indent in px — rather than any one format's
//! spelling of them. A pptx `a:p` and a docx `w:p` disagree about nearly every
//! attribute name and unit and agree about all of this, so resolving the
//! cascade stays with the format and emitting it is written once.
//!
//! Types and emission live together deliberately: the model exists to be
//! rendered, and keeping the two in one file means a new field cannot be added
//! without deciding what it paints.
//!
//! A renderer owns its stylesheet, so the class names arrive in [`HtmlStyle`]
//! and nothing here hard-codes a format's CSS.

use super::drawingml::color::Color;
use super::highlight::{Marker, Terms};
use super::html::{attr, attrs, fmt_px, fmt_ratio, pt_to_px, Style, Writer};

/// Line spacing stated as a percentage is relative to a single line of the
/// font, which is taller than the em box. Nothing here measures text, so this is
/// the multiplier that turns "100%" into a CSS `line-height`.
pub const SINGLE_LINE: f32 = 1.2;

// ── the model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    Justify,
}

impl Align {
    fn css(self) -> &'static str {
        match self {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
            Align::Justify => "justify",
        }
    }
}

/// How tall one line of the paragraph is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    /// Multiple of the font size, i.e. a CSS `line-height` number. A single
    /// line is [`SINGLE_LINE`], which is also the default.
    Multiple(f32),
    /// Exact height in px, whatever the font does.
    Exact(f32),
    /// Floor in px: the line is at least this tall, and taller if its content
    /// needs it (docx `w:line` with `lineRule="atLeast"`). Distinct from
    /// [`LineHeight::Exact`] because clamping to the stated value would clip a
    /// large run or an inline image the author sized the line *around*.
    AtLeast(f32),
}

impl Default for LineHeight {
    fn default() -> Self {
        LineHeight::Multiple(SINGLE_LINE)
    }
}

/// Capitalisation applied to a run's text without changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caps {
    All,
    Small,
}

/// Superscript / subscript. The offset itself is not modelled: no format states
/// a usable one (DrawingML's `baseline` is a percentage of an unstated line
/// height), and CSS `vertical-align` is what every renderer here emits anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Super,
    Sub,
}

/// The bullet or number in front of a paragraph, already resolved to the exact
/// characters it shows — the numbering state, the symbol-font remap and the
/// level's own glyph are all the parser's problem.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListMarker {
    pub label: String,
    pub color: Option<Color>,
    /// CSS font stack, or `None` to take the paragraph's.
    pub font: Option<String>,
    /// Absolute size in points, or `None` to take the paragraph's.
    pub size_pt: Option<f32>,
}

/// A styled span of text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextRun {
    pub text: String,
    /// Absolute, and never inherited: a run that states no size of its own
    /// carries the paragraph's, so the emitter can compare the two and write
    /// nothing when they agree.
    pub size_pt: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// `None` inherits the shape's or page's colour through CSS.
    pub color: Option<Color>,
    /// CSS font stack — already through the substitution table, i.e. safe to
    /// put in a `font-family`.
    pub font: Option<String>,
    pub caps: Option<Caps>,
    pub letter_spacing_pt: f32,
    pub script: Option<Script>,
    /// Text-marker colour behind the run (`w:highlight`, or a run-level `w:shd`
    /// fill). Separate from a paragraph's [`Para::shade`] because the two nest:
    /// a highlighted phrase inside a shaded paragraph paints both.
    pub highlight: Option<Color>,
    /// Destination of the link this run is inside, wrapping it in an `<a>`.
    ///
    /// **Already sanitized**: the producer decides what may become an `href` and
    /// hands over nothing else — see `docx::link::sanitize_href` for the scheme
    /// whitelist. The emitter escapes it as an attribute value but does not
    /// judge it, so a producer that skips the whitelist opens a hole here.
    pub link: Option<String>,
}

/// What a `w:br` interrupts. DrawingML has only the line kind (`a:br`); Word
/// also breaks the page and the column, neither of which a continuously
/// scrolling preview can honour, so both are drawn as a rule instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Break {
    Line,
    Page,
    Column,
}

/// Which side of the text column a wrapped graphic is pulled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A graphic and the box it occupies: an image, or the placeholder one degrades
/// to.
///
/// This is a *run* because that is what a document says it is — a `w:drawing`
/// sits inside a `w:r`, sharing the line with the text on either side of it, and
/// only the paragraph knows where that line is. A slide places its pictures
/// absolutely instead, which is why nothing here has a position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Graphic {
    /// A `data:` URI. `None` draws the placeholder instead — the box still
    /// appears, because it is where the document's layout expects a graphic.
    pub src: Option<String>,
    pub w_px: f32,
    pub h_px: f32,
    /// The placeholder's text, and the `alt` of an image that has a `src`.
    pub label: String,
    /// `Some` floats the box out of the line so the text wraps beside it. The
    /// only placement a reflowing column can honour — see `docx::draw::Wrap`.
    pub float: Option<Side>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Run {
    Text(TextRun),
    /// A break inside the paragraph (`a:br`, `w:br`).
    Break(Break),
    /// A tab as run content (`w:tab` inside a `w:r`). Explicit tab stops
    /// (`w:tabs`) are deliberately not modelled: honouring them means knowing
    /// where the text currently is, and nothing here measures text. So a tab
    /// advances to the next *default* stop, which the renderer's stylesheet
    /// states via `white-space:pre-wrap` plus `tab-size`.
    Tab,
    /// An image or the box one degrades to (`w:drawing`, `w:pict`, `w:object`).
    Graphic(Graphic),
    /// A link target inside the text (`w:bookmarkStart`, a note's reference
    /// marker): an empty element carrying the id a fragment lands on. Zero-width
    /// on purpose — it marks a position in the text rather than being text, so it
    /// must not affect where the runs around it break.
    ///
    /// **Already sanitized** by the producer, like [`TextRun::link`]: it reaches
    /// an `id` attribute.
    Anchor(String),
}

/// One edge of a paragraph's box (`w:pBdr`), already resolved to CSS terms.
///
/// `style` is a CSS `border-style` keyword, not an OOXML one: the format's own
/// vocabulary is translated by [`super::cellstyle::border_css`], which the table
/// renderers already share, so there is exactly one place that knows what
/// `dashDotDot` looks like. An empty `style` is read as `solid`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Border {
    pub width_px: f32,
    pub style: &'static str,
    /// `None` leaves the edge at `currentColor`, i.e. the text colour.
    pub color: Option<Color>,
    /// Gap between the border and the text it surrounds (`w:pBdr/*@space`), in
    /// px — emitted as padding on that side.
    pub space_px: f32,
}

impl Border {
    /// A border that would draw nothing. A zero or non-finite width is not an
    /// invisible border but an absent one: `border-top:0px solid` still resets
    /// whatever the stylesheet stated for that edge.
    fn is_blank(&self) -> bool {
        !self.width_px.is_finite() || self.width_px <= 0.0
    }
}

/// The four edges. All-`None` is the overwhelmingly common case, and costs one
/// pointer-free struct rather than an `Option<Box<…>>` indirection.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Borders {
    pub top: Option<Border>,
    pub right: Option<Border>,
    pub bottom: Option<Border>,
    pub left: Option<Border>,
}

/// One paragraph: its box, its marker, and the runs it holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Para {
    pub runs: Vec<Run>,
    /// The paragraph's own font size. It sizes an otherwise empty line and the
    /// marker, and is the size every run is measured against.
    pub size_pt: f32,
    /// `None` leaves the alignment to whatever the box states.
    pub align: Option<Align>,
    /// Indent of the whole paragraph from the box's leading edge, in px.
    pub indent_px: f32,
    /// Indent from the box's trailing edge, in px (`w:ind@right` / `@end`).
    pub indent_end_px: f32,
    /// First-line offset from `indent_px`, in px, as **one signed number**:
    /// negative hangs the first line to the left of the rest (the usual case for
    /// a list), positive pushes it right. Word spells the two directions as two
    /// mutually exclusive attributes — `w:ind@firstLine` and `w:ind@hanging` —
    /// so the docx parser must fold them, negating the latter, rather than
    /// growing a second field that the emitter would have to reconcile.
    pub first_line_px: f32,
    pub line: LineHeight,
    pub space_before_px: f32,
    pub space_after_px: f32,
    pub marker: Option<ListMarker>,
    pub rtl: bool,
    /// Fill behind the whole paragraph box (`w:shd`).
    pub shade: Option<Color>,
    pub borders: Borders,
    /// Heading level 1-6, which makes the paragraph an `h1`..`h6` instead of a
    /// `p` and changes nothing else about how it is painted. Real document
    /// structure rather than presentation: it is what a per-section jump list
    /// keys off. `None` is the overwhelmingly common case and emits exactly as it
    /// did before this field existed.
    pub heading: Option<u8>,
}

// ── html ─────────────────────────────────────────────────────────────────────

/// How a renderer spells the model: its own class names, plus whether lengths
/// ride a CSS variable the frontend can shrink after layout.
pub struct HtmlStyle {
    pub para_class: &'static str,
    /// Class for a paragraph that has a marker — a flex row rather than a
    /// block, see the hanging-indent note in [`emit_para`].
    pub list_class: &'static str,
    pub marker_class: &'static str,
    /// Wraps the runs of a marked paragraph, so they wrap at their own left
    /// edge instead of under the marker.
    pub text_class: &'static str,
    /// Class for a page or column [`Break`], which cannot be paginated away in a
    /// scrolling frame and is drawn as a visible rule instead. Empty means the
    /// renderer has no such rule, and those breaks degrade to a plain `<br>` —
    /// which is what a format with no page concept (pptx) wants.
    pub break_class: &'static str,
    /// Class for an inline image. Empty for a renderer that builds no
    /// [`Graphic`] (pptx places its pictures itself), in which case the element
    /// carries no `class` at all rather than an empty one.
    pub img_class: &'static str,
    /// Class for the box a graphic the preview cannot draw degrades to.
    pub graphic_class: &'static str,
    /// Sizes are emitted as `calc(… * var(--af, 1))` so a fitting pass in the
    /// frame can scale the whole box. See [`scaled_px`].
    pub scalable: bool,
}

pub fn emit_paras(w: &mut Writer, paras: &[Para], st: &HtmlStyle, hl: &mut Marker, terms: &Terms) {
    for p in paras {
        if w.is_full() {
            break;
        }
        emit_para(w, p, st, hl, terms);
    }
}

fn emit_para(w: &mut Writer, p: &Para, st: &HtmlStyle, hl: &mut Marker, terms: &Terms) {
    let hang = p.first_line_px;
    let marked = p.marker.is_some();

    let mut s = Style::new();
    s.push_opt("text-align", p.align.map(|a| a.css().to_string()));
    if marked {
        // Hanging indent as a flex row rather than `text-indent`: the marker is
        // a fixed-width first item and the runs are a block that wraps at its
        // own left edge. With `text-indent` a first word wider than what is
        // left of the line moves to the next line *whole* instead of breaking,
        // which leaves the marker sitting alone on a line of its own.
        s.push_opt(
            "margin-left",
            Some((p.indent_px + hang).max(0.0))
                .filter(|v| *v != 0.0)
                .and_then(fmt_px),
        );
    } else {
        s.push_opt(
            "margin-left",
            Some(p.indent_px).filter(|v| *v != 0.0).and_then(fmt_px),
        );
        s.push_opt("text-indent", Some(hang).filter(|v| *v != 0.0).and_then(fmt_px));
    }
    s.push_opt(
        "margin-right",
        Some(p.indent_end_px).filter(|v| *v != 0.0).and_then(fmt_px),
    );
    s.push_opt(
        "margin-top",
        Some(p.space_before_px).filter(|v| *v > 0.0).and_then(fmt_px),
    );
    s.push_opt(
        "margin-bottom",
        Some(p.space_after_px).filter(|v| *v > 0.0).and_then(fmt_px),
    );
    s.push_opt("font-size", scaled_px(pt_to_px(p.size_pt), st.scalable));
    match p.line {
        LineHeight::Exact(px) => s.push_opt("line-height", scaled_px(px, st.scalable)),
        // A floor, so the em fallback has to stay in the declaration rather than
        // being resolved here — the px value wins only while it is the larger of
        // the two, and which that is depends on the font. WebKitGTK 4.1 has
        // `max()`, so CSS can decide at layout time.
        LineHeight::AtLeast(px) => s.push_opt(
            "line-height",
            scaled_px(px, st.scalable).map(|v| format!("max({}, {}em)", v, SINGLE_LINE)),
        ),
        // The default costs nothing to state and appears on every paragraph of
        // every deck, so it is left to the stylesheet.
        //
        // A **unitless** ratio, not a percentage: the two differ exactly when the
        // runs are not the paragraph's own size. A percentage computes to px against
        // the paragraph's `font-size` and inherits that px, so a 16px paragraph
        // holding a 96px title run gets 17px of leading and its lines write over one
        // another. A number inherits as a number and each run resolves it against
        // its own size, which is what a producer means by "150% line spacing".
        LineHeight::Multiple(m) => {
            if (m - SINGLE_LINE).abs() > 0.001 {
                s.push_opt("line-height", fmt_ratio(m));
            }
        }
    }
    if p.rtl {
        s.push("direction", "rtl");
    }
    if let Some(c) = p.shade.as_ref() {
        s.push("background-color", &c.css());
    }
    emit_borders(&mut s, &p.borders);
    let class = if marked { st.list_class } else { st.para_class };
    // The heading changes the *element* and nothing else: the class and every
    // style decision above are the paragraph's either way, so a renderer's
    // stylesheet does not have to restate them per level. A level outside 1..6
    // has no element and stays a `p`.
    let tag = match p.heading {
        Some(1) => "h1",
        Some(2) => "h2",
        Some(3) => "h3",
        Some(4) => "h4",
        Some(5) => "h5",
        Some(6) => "h6",
        _ => "p",
    };
    w.open(tag, &attrs(&[&attr("class", class), &s.to_attr()]));

    if let Some(m) = p.marker.as_ref() {
        emit_marker(w, m, st, hang);
        w.open("span", &attrs(&[&attr("class", st.text_class)]));
    }

    if p.runs.is_empty() {
        // A non-breaking space keeps the empty line's height.
        w.text("\u{00a0}");
    }
    for r in &p.runs {
        match r {
            Run::Break(Break::Line) => w.void("br", ""),
            Run::Break(_) if st.break_class.is_empty() => w.void("br", ""),
            Run::Break(_) => w.void("br", &attr("class", st.break_class)),
            // A literal tab character, not an entity or a run of spaces: the
            // stylesheet's `tab-size` is what positions it, and only a real
            // U+0009 under `white-space:pre-wrap` is subject to that.
            Run::Tab => w.text("\t"),
            Run::Graphic(g) => emit_graphic(w, g, st),
            Run::Anchor(id) => {
                w.open("span", &attr("id", id));
                w.close();
            }
            // A run with no text is not a blank line — only a paragraph with no
            // runs at all is — so it is dropped rather than spaced.
            Run::Text(t) if t.text.is_empty() => {}
            Run::Text(t) => emit_run(w, t, p.size_pt, st, hl, terms),
        }
    }
    if marked {
        w.close(); // text_class
    }
    w.close();
}

/// Per-side `border-*` shorthands plus the gap each side asks for. Written side
/// by side rather than as a single `border` shorthand: Word states the four
/// edges independently and a shorthand would have to invent values for the
/// three a paragraph did not mention.
fn emit_borders(s: &mut Style, b: &Borders) {
    for (side, edge) in [
        ("top", &b.top),
        ("right", &b.right),
        ("bottom", &b.bottom),
        ("left", &b.left),
    ] {
        let Some(e) = edge.as_ref().filter(|e| !e.is_blank()) else {
            continue;
        };
        let Some(w) = fmt_px(e.width_px) else { continue };
        let style = if e.style.is_empty() { "solid" } else { e.style };
        let mut v = format!("{} {}", w, style);
        if let Some(c) = e.color.as_ref() {
            v.push(' ');
            v.push_str(&c.css());
        }
        s.push(&format!("border-{}", side), &v);
        s.push_opt(
            &format!("padding-{}", side),
            Some(e.space_px).filter(|v| *v > 0.0).and_then(fmt_px),
        );
    }
}

/// The marker as the first item of the paragraph's flex row. Its width is the
/// hanging indent, so the text that follows lands exactly on the paragraph's
/// indent without anyone having to measure the glyph.
fn emit_marker(w: &mut Writer, m: &ListMarker, st: &HtmlStyle, hang: f32) {
    let mut s = Style::new();
    if hang < -0.5 {
        s.push_opt("width", fmt_px(-hang));
    } else {
        s.push("padding-right", "0.3em");
    }
    if let Some(c) = m.color.as_ref() {
        s.push("color", &c.css());
    }
    s.push_opt("font-family", m.font.clone());
    s.push_opt("font-size", m.size_pt.and_then(|pt| fmt_px(pt_to_px(pt))));
    w.open("span", &attrs(&[&attr("class", st.marker_class), &s.to_attr()]));
    w.text(&m.label);
    w.close();
}

fn emit_run(
    w: &mut Writer,
    t: &TextRun,
    base_pt: f32,
    st: &HtmlStyle,
    hl: &mut Marker,
    terms: &Terms,
) {
    let mut s = Style::new();
    if (t.size_pt - base_pt).abs() > 0.01 {
        s.push_opt("font-size", scaled_px(pt_to_px(t.size_pt), st.scalable));
    }
    if t.bold {
        s.push("font-weight", "700");
    }
    if t.italic {
        s.push("font-style", "italic");
    }
    let mut deco = String::new();
    if t.underline {
        deco.push_str("underline");
    }
    if t.strike {
        if !deco.is_empty() {
            deco.push(' ');
        }
        deco.push_str("line-through");
    }
    s.push("text-decoration", &deco);
    if let Some(c) = t.color.as_ref() {
        s.push("color", &c.css());
    }
    if let Some(c) = t.highlight.as_ref() {
        s.push("background-color", &c.css());
    }
    s.push_opt("font-family", t.font.clone());
    match t.caps {
        Some(Caps::All) => s.push("text-transform", "uppercase"),
        Some(Caps::Small) => s.push("font-variant", "small-caps"),
        None => {}
    }
    s.push_opt(
        "letter-spacing",
        Some(t.letter_spacing_pt)
            .filter(|v| *v != 0.0)
            .map(|v| fmt_px(pt_to_px(v)).unwrap_or_default()),
    );
    match t.script {
        Some(Script::Super) => s.push("vertical-align", "super"),
        Some(Script::Sub) => s.push("vertical-align", "sub"),
        None => {}
    }
    if t.script.is_some() {
        s.push("font-size", "0.65em");
    }
    // The `<a>` wraps the styled span rather than carrying the style itself: the
    // run's own colour and underline are what a link looks like here (see
    // `docx::body::text_run`), and a UA that paints its links blue must not win
    // over a document that stated a colour. `title` is the destination in text,
    // because the frame cannot navigate and the only thing a reader can do with a
    // link is read where it points.
    let linked = t.link.as_deref();
    if let Some(href) = linked {
        w.open("a", &attrs(&[&attr("href", href), &attr("title", href)]));
    }
    let html = hl.mark(&t.text, terms);
    if s.is_empty() {
        w.raw(&html);
    } else {
        w.open("span", &s.to_attr());
        w.raw(&html);
        w.close();
    }
    if linked.is_some() {
        w.close();
    }
}

/// A graphic at its own size: the image, or the box it degrades to. Both state
/// the same geometry, so the text around them reflows identically either way and
/// a placeholder never moves the paragraph it sits in.
fn emit_graphic(w: &mut Writer, g: &Graphic, st: &HtmlStyle) {
    let mut s = Style::new();
    s.push_opt("width", fmt_px(g.w_px));
    s.push_opt("height", fmt_px(g.h_px));
    // A float is the whole of a wrapped anchor's placement (see the note on
    // [`Graphic::float`]); the margin is the air Word's default wrap distance
    // leaves between the box and the text beside it.
    match g.float {
        Some(Side::Left) => {
            s.push("float", "left");
            s.push("margin", "0 8px 4px 0");
        }
        Some(Side::Right) => {
            s.push("float", "right");
            s.push("margin", "0 0 4px 8px");
        }
        None => {}
    }
    match g.src.as_deref() {
        // `alt` is written even when empty: without the attribute a browser that
        // cannot decode the image falls back to showing the `src`, and a `data:`
        // URI is megabytes of base64.
        Some(src) => w.void(
            "img",
            &attrs(&[
                &class_attr(st.img_class),
                &attr("src", src),
                &attr("alt", &g.label),
                &s.to_attr(),
            ]),
        ),
        // A `span`, not the `div` `emit::placeholder` writes, and no absolute
        // placement: a block element inside a `p` is closed *before* the
        // paragraph by every HTML parser, which would split the paragraph in the
        // DOM, and there is no positioned parent to fill — this box is itself the
        // geometry.
        None => {
            w.open("span", &attrs(&[&class_attr(st.graphic_class), &s.to_attr()]));
            w.text(&g.label);
            w.close();
        }
    }
}

/// `class="…"`, or nothing when the renderer states no class for this box (so no
/// bare `class=""` litters the output).
fn class_attr(class: &str) -> String {
    if class.is_empty() {
        String::new()
    } else {
        attr("class", class)
    }
}

/// A length a fitting pass in the frame can shrink. PowerPoint's own autofit
/// scale is computed against *its* font metrics; with a substituted family the
/// same text needs a different scale, so the box re-fits itself after layout and
/// every size inside it rides on `--af`.
fn scaled_px(px: f32, scalable: bool) -> Option<String> {
    let v = fmt_px(px)?;
    Some(if scalable {
        format!("calc({} * var(--af, 1))", v)
    } else {
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ST: HtmlStyle = HtmlStyle {
        para_class: "p",
        list_class: "p li",
        marker_class: "bu",
        text_class: "tx",
        break_class: "",
        img_class: "",
        graphic_class: "",
        scalable: false,
    };

    fn run(text: &str) -> Run {
        Run::Text(TextRun {
            text: text.to_string(),
            size_pt: 18.0,
            ..Default::default()
        })
    }

    fn para(runs: Vec<Run>) -> Para {
        Para {
            runs,
            size_pt: 18.0,
            ..Default::default()
        }
    }

    fn html(paras: &[Para], st: &HtmlStyle) -> String {
        let mut w = Writer::new(1 << 16);
        let mut hl = Marker::new();
        emit_paras(&mut w, paras, st, &mut hl, &Terms::new(&[]));
        w.finish()
    }

    #[test]
    fn a_plain_paragraph_states_only_its_size() {
        assert_eq!(
            html(&[para(vec![run("café")])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\">café</p>"
        );
    }

    #[test]
    fn run_formatting_lands_on_a_span_and_defaults_do_not() {
        let mut t = TextRun {
            text: "naïve".to_string(),
            size_pt: 24.0,
            bold: true,
            italic: true,
            underline: true,
            strike: true,
            color: Some(Color::from_rgb(0x00FF00)),
            font: Some("Widget Sans, sans-serif".to_string()),
            caps: Some(Caps::All),
            letter_spacing_pt: 1.5,
            script: Some(Script::Super),
            highlight: None,
            link: None,
        };
        let out = html(&[para(vec![Run::Text(t.clone())])], &ST);
        assert!(out.contains("font-size:32px;"), "{out}");
        assert!(out.contains("font-weight:700;font-style:italic;"), "{out}");
        assert!(out.contains("text-decoration:underline line-through;"), "{out}");
        assert!(out.contains("color:#00ff00;"), "{out}");
        assert!(out.contains("text-transform:uppercase;"), "{out}");
        assert!(out.contains("letter-spacing:2px;"), "{out}");
        // Super/subscript is an alignment *and* a size.
        assert!(out.contains("vertical-align:super;font-size:0.65em;"), "{out}");

        // A run that states nothing but its (inherited) size needs no span at all.
        t = TextRun {
            text: "café".to_string(),
            size_pt: 18.0,
            ..Default::default()
        };
        assert_eq!(
            html(&[para(vec![Run::Text(t)])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\">café</p>"
        );
    }

    #[test]
    fn a_marker_hangs_in_the_first_line_indent() {
        let p = Para {
            marker: Some(ListMarker {
                label: "1.".to_string(),
                color: Some(Color::from_rgb(0xFF0000)),
                font: Some("Widget Sans, sans-serif".to_string()),
                size_pt: Some(9.0),
            }),
            indent_px: 36.0,
            first_line_px: -36.0,
            ..para(vec![run("café")])
        };
        let out = html(&[p.clone()], &ST);
        // marL + a full hang cancel out, so the block starts at the box edge and
        // the marker occupies the hang.
        assert!(out.starts_with("<p class=\"p li\" style=\"font-size:24px;\">"), "{out}");
        assert!(
            out.contains("<span class=\"bu\" style=\"width:36px;color:#ff0000;\
font-family:Widget Sans, sans-serif;font-size:12px;\">1.</span>"),
            "{out}"
        );
        assert!(out.contains("<span class=\"tx\">café</span>"), "{out}");

        // No hang: the marker cannot be given a width, so it takes a gap.
        let flat = Para {
            first_line_px: 0.0,
            indent_px: 0.0,
            ..p
        };
        assert!(
            html(&[flat], &ST).contains("style=\"padding-right:0.3em;"),
            "no fallback gap"
        );
    }

    #[test]
    fn an_unmarked_paragraph_spells_its_indent_as_margin_and_text_indent() {
        let p = Para {
            indent_px: 48.0,
            first_line_px: -24.0,
            ..para(vec![run("café")])
        };
        assert!(
            html(&[p], &ST).contains("margin-left:48px;text-indent:-24px;"),
            "indent not split"
        );
    }

    #[test]
    fn line_height_states_only_what_differs_from_a_single_line() {
        let with = |line| html(&[Para { line, ..para(vec![run("x")]) }], &ST);
        assert!(!with(LineHeight::default()).contains("line-height"));
        // Unitless, so a run larger than the paragraph resolves the ratio against
        // its own size instead of inheriting the paragraph's computed px — a 96px
        // title in a 16px paragraph writes over itself otherwise.
        assert!(with(LineHeight::Multiple(1.8)).contains("line-height:1.8;"));
        assert!(!with(LineHeight::Multiple(1.8)).contains("180%"));
        assert!(with(LineHeight::Exact(40.0)).contains("line-height:40px;"));
    }

    #[test]
    fn a_ratio_line_height_survives_a_run_larger_than_its_paragraph() {
        // The shape of the odp title bug: the paragraph resolves to one size and
        // the run inside it to another.
        let big = TextRun {
            text: "Google Trends".to_string(),
            size_pt: 72.0,
            ..Default::default()
        };
        let out = html(
            &[Para {
                line: LineHeight::Multiple(1.068),
                size_pt: 12.0,
                ..para(vec![Run::Text(big)])
            }],
            &ST,
        );
        // Two decimals is the writer's precision for every CSS number; on a 96px
        // run that is a quarter of a pixel.
        assert!(out.contains("line-height:1.07;"), "{out}");
        assert!(out.contains("font-size:96px"), "{out}");
    }

    #[test]
    fn scalable_sizes_ride_the_fitting_variable() {
        let st = HtmlStyle { scalable: true, ..ST };
        let p = Para {
            line: LineHeight::Exact(30.0),
            ..para(vec![Run::Text(TextRun {
                text: "café".to_string(),
                size_pt: 36.0,
                ..Default::default()
            })])
        };
        let out = html(&[p], &st);
        assert!(out.contains("font-size:calc(24px * var(--af, 1));"), "{out}");
        assert!(out.contains("line-height:calc(30px * var(--af, 1));"), "{out}");
        assert!(out.contains("font-size:calc(48px * var(--af, 1));"), "{out}");
    }

    #[test]
    fn an_empty_paragraph_keeps_its_height_but_an_empty_run_does_not() {
        assert!(html(&[para(vec![])], &ST).contains('\u{00a0}'));
        // A paragraph whose runs are all empty is not a blank line: it holds no
        // text *and* no filler.
        assert_eq!(
            html(&[para(vec![run("")])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\"></p>"
        );
        assert_eq!(
            html(&[para(vec![Run::Break(Break::Line)])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\"><br></p>"
        );
    }

    #[test]
    fn alignment_and_direction_reach_the_paragraph_box() {
        let p = Para {
            align: Some(Align::Justify),
            rtl: true,
            space_before_px: 8.0,
            space_after_px: 12.0,
            ..para(vec![run("café")])
        };
        let out = html(&[p], &ST);
        assert!(out.contains("text-align:justify;"), "{out}");
        assert!(out.contains("margin-top:8px;margin-bottom:12px;"), "{out}");
        assert!(out.contains("direction:rtl;"), "{out}");
    }

    #[test]
    fn a_tab_run_emits_a_literal_tab() {
        // Not `&#9;` and not spaces: `tab-size` only acts on U+0009.
        assert_eq!(
            html(&[para(vec![run("café"), Run::Tab, run("naïve")])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\">café\tnaïve</p>"
        );
    }

    #[test]
    fn page_and_column_breaks_take_a_class_and_degrade_without_one() {
        let st = HtmlStyle { break_class: "pb", ..ST };
        let out = html(
            &[para(vec![Run::Break(Break::Page), Run::Break(Break::Column)])],
            &st,
        );
        assert_eq!(
            out,
            "<p class=\"p\" style=\"font-size:24px;\"><br class=\"pb\"><br class=\"pb\"></p>"
        );
        // A line break never takes the class, and a renderer with no rule for
        // pagination gets a plain break for all three kinds.
        assert!(html(&[para(vec![Run::Break(Break::Line)])], &st).contains("<br>"));
        assert!(html(&[para(vec![Run::Break(Break::Page)])], &ST).contains("<br>"));
    }

    #[test]
    fn a_run_highlight_paints_behind_the_span() {
        let t = TextRun {
            text: "Widget".to_string(),
            size_pt: 18.0,
            highlight: Some(Color::from_rgb(0xFFFF00)),
            ..Default::default()
        };
        assert_eq!(
            html(&[para(vec![Run::Text(t)])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\">\
<span style=\"background-color:#ffff00;\">Widget</span></p>"
        );
    }

    #[test]
    fn paragraph_shade_and_borders_paint_the_box() {
        let edge = Border {
            width_px: 2.0,
            style: "dashed",
            color: Some(Color::from_rgb(0x0000FF)),
            space_px: 4.0,
        };
        let p = Para {
            shade: Some(Color::from_rgb(0xEEEEEE)),
            borders: Borders {
                top: Some(edge),
                // No colour: the edge follows the text colour. No space either.
                bottom: Some(Border { color: None, space_px: 0.0, ..edge }),
                ..Default::default()
            },
            ..para(vec![run("café")])
        };
        let out = html(&[p], &ST);
        assert!(out.contains("background-color:#eeeeee;"), "{out}");
        assert!(out.contains("border-top:2px dashed #0000ff;padding-top:4px;"), "{out}");
        assert!(out.contains("border-bottom:2px dashed;"), "{out}");
        assert!(!out.contains("padding-bottom"), "{out}");
        assert!(!out.contains("border-left") && !out.contains("border-right"), "{out}");
    }

    #[test]
    fn a_zero_or_unmeasurable_border_paints_nothing() {
        // `border-top:0px solid` would still reset the stylesheet's edge, so an
        // absent border must emit no declaration at all — as must a poisoned one.
        let with = |b: Border| {
            html(
                &[Para {
                    borders: Borders { top: Some(b), ..Default::default() },
                    ..para(vec![run("café")])
                }],
                &ST,
            )
        };
        let base = Border { width_px: 1.0, style: "solid", color: None, space_px: 8.0 };
        assert!(!with(Border { width_px: 0.0, ..base }).contains("border"));
        assert!(!with(Border { width_px: -1.0, ..base }).contains("border"));
        assert!(!with(Border { width_px: f32::NAN, ..base }).contains("border"));
        // An unstated style is still a border.
        assert!(with(Border { style: "", ..base }).contains("border-top:1px solid;padding-top:8px;"));
    }

    #[test]
    fn at_least_line_height_keeps_the_single_line_floor_in_css() {
        let with = |line| html(&[Para { line, ..para(vec![run("x")]) }], &ST);
        assert!(with(LineHeight::AtLeast(30.0)).contains("line-height:max(30px, 1.2em);"));
        // Unmeasurable heights drop the declaration rather than emitting `max(NaNpx…)`,
        // which CSS would discard along with every other pair in the attribute.
        assert!(!with(LineHeight::AtLeast(f32::NAN)).contains("line-height"));
        let st = HtmlStyle { scalable: true, ..ST };
        assert!(html(&[Para { line: LineHeight::AtLeast(30.0), ..para(vec![run("x")]) }], &st)
            .contains("line-height:max(calc(30px * var(--af, 1)), 1.2em);"));
    }

    #[test]
    fn a_trailing_indent_becomes_a_right_margin() {
        let p = Para {
            indent_px: 48.0,
            indent_end_px: 24.0,
            ..para(vec![run("café")])
        };
        assert!(
            html(&[p], &ST).contains("margin-left:48px;margin-right:24px;"),
            "trailing indent missing"
        );
        // Zero states nothing, like the leading indent.
        assert!(!html(&[para(vec![run("café")])], &ST).contains("margin-right"));
    }

    #[test]
    fn a_heading_changes_the_element_and_nothing_else() {
        let base = Para {
            align: Some(Align::Center),
            ..para(vec![run("café")])
        };
        // Every class and style decision is the paragraph's either way, so the
        // only difference from a `p` is the tag.
        let plain = html(&[base.clone()], &ST);
        assert_eq!(plain, "<p class=\"p\" style=\"text-align:center;font-size:24px;\">café</p>");
        for lvl in 1..=6u8 {
            let out = html(&[Para { heading: Some(lvl), ..base.clone() }], &ST);
            assert_eq!(out, plain.replacen("<p ", &format!("<h{lvl} "), 1).replace("</p>", &format!("</h{lvl}>")));
        }
        // Levels with no element stay a `p`, and so does `None` — pptx output must
        // not move.
        assert_eq!(html(&[Para { heading: Some(7), ..base.clone() }], &ST), plain);
        assert_eq!(html(&[Para { heading: Some(0), ..base }], &ST), plain);
    }

    #[test]
    fn a_graphic_run_is_an_image_in_the_line() {
        let st = HtmlStyle { img_class: "im", graphic_class: "gp", ..ST };
        let g = Graphic {
            src: Some("data:image/png;base64,AAAA".to_string()),
            w_px: 96.0,
            h_px: 48.0,
            label: "café \"naïve\"".to_string(),
            float: None,
        };
        let out = html(&[para(vec![run("a "), Run::Graphic(g.clone())])], &st);
        assert!(out.contains("<img class=\"im\" src=\"data:image/png;base64,AAAA\""), "{out}");
        // The label is the alt text, escaped as an attribute value.
        assert!(out.contains("alt=\"café &quot;naïve&quot;\""), "{out}");
        assert!(out.contains("style=\"width:96px;height:48px;\">"), "{out}");

        // No `src`: the same box as a dashed span, carrying the label as text.
        let ph = Graphic { src: None, ..g };
        let out = html(&[para(vec![Run::Graphic(ph)])], &st);
        assert!(
            out.contains("<span class=\"gp\" style=\"width:96px;height:48px;\">café \"naïve\"</span>"),
            "{out}"
        );
    }

    #[test]
    fn a_floated_graphic_states_its_side_and_a_gap() {
        let g = |float| {
            Run::Graphic(Graphic {
                src: Some("data:image/png;base64,AAAA".to_string()),
                w_px: 64.0,
                h_px: 64.0,
                label: String::new(),
                float,
            })
        };
        let left = html(&[para(vec![g(Some(Side::Left))])], &ST);
        assert!(left.contains("float:left;margin:0 8px 4px 0;"), "{left}");
        let right = html(&[para(vec![g(Some(Side::Right))])], &ST);
        assert!(right.contains("float:right;margin:0 0 4px 8px;"), "{right}");
        // A renderer with no class for the box emits no `class` attribute at all.
        assert!(html(&[para(vec![g(None)])], &ST).contains("<img src="), "{right}");
        assert!(!html(&[para(vec![g(None)])], &ST).contains("class=\"\""), "{right}");
    }

    #[test]
    fn a_link_wraps_its_run_and_states_where_it_points() {
        let t = TextRun {
            text: "example.org".to_string(),
            size_pt: 18.0,
            underline: true,
            color: Some(Color::from_rgb(0x0563C1)),
            link: Some("https://example.org/?a=1&b=2".to_string()),
            ..Default::default()
        };
        let out = html(&[para(vec![Run::Text(t.clone())])], &ST);
        // The destination is escaped as an attribute value in both places, and the
        // styled span sits *inside* the anchor.
        assert!(
            out.contains(
                "<a href=\"https://example.org/?a=1&amp;b=2\" \
title=\"https://example.org/?a=1&amp;b=2\"><span style=\""
            ),
            "{out}"
        );
        assert!(out.contains(">example.org</span></a>"), "{out}");

        // A run with no style of its own still closes its anchor.
        let plain = TextRun { underline: false, color: None, ..t };
        let out = html(&[para(vec![Run::Text(plain)])], &ST);
        assert!(out.ends_with("\">example.org</a></p>"), "{out}");
        assert!(!out.contains("<span"), "{out}");

        // An anchor is a position, not text: nothing but the id.
        assert_eq!(
            html(&[para(vec![Run::Anchor("of-bm-Widget".to_string())])], &ST),
            "<p class=\"p\" style=\"font-size:24px;\"><span id=\"of-bm-Widget\"></span></p>"
        );
    }

    #[test]
    fn query_terms_are_marked_inside_the_runs() {
        let mut w = Writer::new(1 << 16);
        let mut hl = Marker::new();
        let terms = Terms::new(&["café".to_string()]);
        emit_paras(&mut w, &[para(vec![run("a café here")])], &ST, &mut hl, &terms);
        let out = w.finish();
        assert!(out.contains("<mark class=\"preview-hl\""), "{out}");
        assert!(hl.best_mark_id().is_some());
    }
}
