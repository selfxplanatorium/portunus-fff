//! `office:presentation` → one slide per call: a fixed-size canvas of absolutely
//! positioned boxes, exactly the shape the pptx renderer produces.
//!
//! The frontend's `slide` variant lays the canvas out once and scales it with a
//! single transform, so both dialects have to agree on the canvas: `slideshape`
//! owns the stylesheet and the size bounds, and this file only decides geometry and
//! paint. `.pp-tb` in particular is a contract — the frame's selection engine
//! treats it as the place a drag selects text rather than pans the slide.
//!
//! What ODF makes easier than PresentationML, and what it makes harder.
//!
//! Easier: a slide is a `draw:page` in `content.xml`, so there is no relationship
//! graph to resolve and no per-slide part to read — every slide of the deck is
//! already parsed, and flipping between them costs nothing but a re-render.
//! Placeholder *inheritance* is also absent by construction: Impress writes the
//! resolved text into the page's own shapes, so there is no layout-then-master
//! lookup for a shape's properties (`presentation:class` survives as a label, not
//! as a pointer).
//!
//! Harder: the master page's shapes and the page's own live in different documents
//! (`styles.xml` and `content.xml`), and a master carries both the deck's furniture
//! — a logo, a rule, a background picture — and the empty prompt boxes Impress
//! shows only while editing. Drawing the second kind would put "Click to add title"
//! across a rendered slide, so [`master_shape`] separates them.

use super::super::drawingml::fill::{push_fill, Fill};
use super::super::drawingml::line::line_css;
use super::super::emit::{self, Notes};
use super::super::highlight::{Marker as Highlight, Terms};
use super::super::html::{attr, attrs, fmt_px, Style, Writer};
use super::super::media::{Media, MediaBudget, MediaCache};
use super::super::model::HtmlStyle;
use super::super::slideshape;
use super::super::xml::{self, attr_local, child, elems, inner_text};
use super::super::{OfficeDoc, Shape};
use super::length;
use super::list::Lists;
use super::pkg::Package;
use super::style::{Family, GraphicProps, Resolved, Styles};
use super::text::{self, Classes, Ctx};
use roxmltree::Node;

/// Byte cap for one slide's HTML. A slide is a canvas rather than a column, so it
/// matches the pptx renderer's rather than the page's.
pub const HTML_CAP: usize = 6 * 1024 * 1024;

/// The slide-shape class names, i.e. `slideshape::BASE_CSS`'s.
const SLIDE: Classes = Classes {
    html: HtmlStyle {
        para_class: "pp-p",
        list_class: "pp-p pp-li",
        marker_class: "pp-bu",
        text_class: "pp-tx",
        // A slide has no pages and no columns, so a break is only ever a line
        // break — the same reasoning the pptx path states.
        break_class: "",
        // A slide's pictures are shapes placed absolutely by this file, so a
        // `Graphic` run is only ever built for a picture *inside* a text box.
        img_class: "pp-img-i",
        graphic_class: "pp-ph-i",
        scalable: false,
    },
    table: "pp-tbl",
    table_auto: "pp-tbl",
    // The stylesheet styles `.pp-tbl td` by tag: a slide's table is one box, so
    // there is no per-cell hook to publish.
    cell: "",
};

const MAX_SLIDES: usize = 500;

/// Shapes drawn per slide, across the master and the page. Bounds the walk; the
/// writer's byte cap is the real backstop.
const MAX_SHAPES: usize = 400;

/// Nesting of `draw:g` groups. Impress writes none in the corpus at all, so this is
/// pure hardening against a generated file.
const MAX_GROUP_DEPTH: usize = 8;

const MAX_TITLE_CHARS: usize = 72;

/// Characters of a shape's description kept, for a placeholder's label.
const MAX_LABEL_CHARS: usize = 200;

/// Ceiling on either axis of a shape's box, past the canvas's own bounds: a shape
/// may legitimately hang off the slide, but not by a mile.
const MAX_SHAPE_PX: f32 = 16_384.0;

/// `presentation:class` values that name an *editing prompt* rather than content.
/// A master's copy of one is the empty box Impress shows while editing; the page's
/// own copy holds the real text and is drawn like any other shape.
const PROMPT_CLASSES: &[&str] = &[
    "title",
    "outline",
    "subtitle",
    "text",
    "notes",
    "page-number",
    "date-time",
    "header",
    "footer",
];

const NOTE_TRANSFORM: &str = "Some shapes drawn upright";
const NOTE_OBJECT: &str = "Embedded objects not drawn";
const NOTE_SHAPES: &str = "Some shapes not shown";
const NOTE_CUSTOM: &str = "Custom shapes drawn as rectangles";
const NOTE_BODY: &str = "Slides unreadable";

/// A shape's box in CSS px. Both extents are required — a shape with no size has
/// nowhere to be drawn — so this is only built once they resolve.
#[derive(Debug, Clone, Copy)]
struct Box {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// `draw:transform`'s rotation in radians, counter-clockwise as ODF states it.
    rotate: Option<f32>,
}

impl Box {
    fn css(&self) -> Style {
        let mut s = Style::new();
        s.push_opt("left", fmt_px(self.x));
        s.push_opt("top", fmt_px(self.y));
        s.push_opt("width", fmt_px(self.w));
        s.push_opt("height", fmt_px(self.h));
        if let Some(rad) = self.rotate {
            // ODF measures counter-clockwise, CSS clockwise. The origin is the
            // shape's own centre in both.
            let deg = -rad.to_degrees();
            if let Some(v) = super::super::html::fmt_deg(deg) {
                s.push("transform", &format!("rotate({v})"));
            }
        }
        s
    }
}

pub fn render(
    package: Package,
    notes: Notes,
    section: Option<u32>,
    terms: &[String],
) -> Result<OfficeDoc, String> {
    render_with(package, notes, section, terms, HTML_CAP, MediaBudget::new())
}

/// The byte cap and the image budget are parameters only so tests can reach the
/// refusal paths without generating megabytes; [`render`] passes the real ones.
fn render_with(
    package: Package,
    mut notes: Notes,
    section: Option<u32>,
    terms: &[String],
    html_cap: usize,
    mut mb: MediaBudget,
) -> Result<OfficeDoc, String> {
    // Destructured immediately: held whole, `&package.content` and
    // `&mut package.zip` are conflicting borrows and no media could be read while
    // the tree is alive.
    let Package {
        mut zip,
        mut budget,
        entries,
        content,
        styles: styles_xml,
        ..
    } = package;

    let styles = Styles::parse(styles_xml.as_deref(), &content);
    let mut lists = Lists::parse(styles_xml.as_deref(), &content);

    let parsed = xml::parse(&content)?;
    let root = parsed.root_element();
    let pages = pages(root);
    if pages.is_empty() {
        // Not a degradation: a deck with no slides has nothing this renderer can
        // show, and the frontend's fallback path says more than an empty canvas.
        return Err(NOTE_BODY.to_string());
    }

    let last = pages.len().saturating_sub(1) as u32;
    let idx = section.map(|s| s.min(last)).unwrap_or(0);
    // Every slide is already parsed, so the strip can carry each one's own name
    // without reading anything more — unlike the pptx path, which would have to
    // open every slide part to do the same.
    let sections = page_names(&pages);

    let page = pages[idx as usize];
    let setup = styles.page_setup(attr_local(page, "master-page-name"));
    let natural = if setup.stated && slideshape::in_range(setup.page.width) && slideshape::in_range(setup.page.height) {
        (setup.page.width, setup.page.height)
    } else {
        // A presentation laid out on the page default would be the wrong *shape*,
        // which a 4:3 canvas at least is not.
        slideshape::DEFAULT_SLIDE
    };

    // The master's shapes live in `styles.xml`, which `Styles` consumed as a
    // string. Parsed again here rather than threaded through the cascade: the nodes
    // have to outlive the walk, `Styles` keeps resolved properties rather than
    // markup, and a master page is the one thing a slide needs the *elements* of.
    let master_doc = styles_xml.as_deref().and_then(|s| xml::parse(s).ok());
    let master = master_doc.as_ref().and_then(|d| {
        master_page(
            d.root_element(),
            attr_local(page, "master-page-name").unwrap_or(""),
        )
    });

    let query = Terms::new(terms);
    let mut hl = Highlight::new();
    let mut media = MediaCache::new();
    let mut w = Writer::new(html_cap);

    let mut css = slideshape::canvas_css(natural);
    if let Some(f) = background(&styles, &setup, page) {
        push_fill(&mut css, &f);
    }
    w.open("div", &attrs(&[&attr("class", "pp-doc"), &css.to_attr()]));

    let mut ctx = Ctx {
        zip: &mut zip,
        budget: &mut budget,
        media: &mut media,
        mb: &mut mb,
        entries: &entries,
        classes: &SLIDE,
        // Replaced per shape: a picture inside a text box is fitted to the box it
        // sits in, not to the slide.
        column_px: natural.0,
        images: 0,
        styles: &styles,
        lists: &mut lists,
        default_font: None,
        multi_col: Default::default(),
        terms: &query,
        marker: &mut hl,
        notes: &mut notes,
        pending: Vec::new(),
        boxes: 0,
        box_depth: 0,
        bookmarks: 0,
        paras: 0,
        cells: 0,
        note_refs: Vec::new(),
        in_note: false,
        prev_style: None,
    };
    let mut shapes = 0usize;
    // Back to front: the master's furniture, then the slide's own content.
    if let Some(m) = master {
        walk(&mut ctx, &mut w, m, natural, 0, &mut shapes, true);
    }
    walk(&mut ctx, &mut w, page, natural, 0, &mut shapes, false);
    w.close();

    for n in mb.notes() {
        notes.add(n);
    }

    let truncated = w.truncated();
    Ok(OfficeDoc {
        html: emit::wrap_style(slideshape::BASE_CSS, "", w.finish()),
        shape: Shape::Slide,
        sections,
        section: idx,
        natural: Some(natural),
        // A canvas has no page box.
        page: None,
        best_mark_id: hl.best_mark_id(),
        truncated,
        notes: notes.into_vec(),
    })
}

// ── the deck ─────────────────────────────────────────────────────────────────
//
// `pages` and `page_names` are the seam the content index shares with the
// renderer: the index stores one row per slide and hands the slide number back,
// so both sides must enumerate the deck the same way. One function each, used by
// both, rather than two matching loops.

/// The deck's slides, in body order.
pub(super) fn pages<'a>(root: Node<'a, 'a>) -> Vec<Node<'a, 'a>> {
    child(root, "body")
        .and_then(|b| child(b, "presentation"))
        .map(|p| {
            elems(p)
                .filter(|n| n.tag_name().name() == "page")
                .take(MAX_SLIDES)
                .collect()
        })
        .unwrap_or_default()
}

/// Slide names for the section strip.
pub(super) fn page_names(pages: &[Node]) -> Vec<String> {
    pages
        .iter()
        .enumerate()
        .map(|(i, p)| page_name(*p, i))
        .collect()
}

/// One slide's shape text as flat text for the content index.
///
/// The master's furniture is not indexed even though the slide draws it: it is the
/// same text on every slide of the deck, so indexing it would make every slide
/// match a query for a footer.
pub(super) fn page_text(page: Node) -> String {
    let mut out = String::new();
    xml::odf_walk_skipping(page, super::NOT_DRAWN, &mut out);
    xml::normalize(&out)
}

/// One slide's name for the section strip: its own title text, else a name the
/// author chose, else its position.
///
/// Every slide gets a real title here, which the pptx path cannot manage — it would
/// have to open all sixty slide parts on every flip, so it numbers them instead.
/// A whole ODF deck is one already-parsed document, so the titles are free.
/// `draw:name` comes second because a producer writes `page3` into it for a slide
/// nobody named, and a position is more use than that.
fn page_name(page: Node, i: usize) -> String {
    title(page)
        .or_else(|| {
            attr_local(page, "name")
                .map(str::trim)
                .filter(|s| !s.is_empty() && !is_default_name(s))
                .map(|s| s.chars().take(MAX_TITLE_CHARS).collect())
        })
        .unwrap_or_else(|| format!("Slide {}", i + 1))
}

/// Whether a `draw:name` is the producer's own `page12` rather than something a
/// person typed.
fn is_default_name(s: &str) -> bool {
    s.strip_prefix("page")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The text of the shape the document labels as its title.
fn title(page: Node) -> Option<String> {
    let shape = elems(page).find(|n| {
        matches!(
            attr_local(*n, "class"),
            Some("title") | Some("subtitle") | Some("outline")
        )
    })?;
    let mut s = String::new();
    inner_text(shape, &mut s);
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(MAX_TITLE_CHARS).collect();
    (!s.is_empty()).then_some(s)
}

/// The `style:master-page` a slide names, from the parsed `styles.xml`.
fn master_page<'d>(root: Node<'d, 'd>, name: &str) -> Option<Node<'d, 'd>> {
    let masters = child(root, "master-styles")?;
    let mut first = None;
    for m in elems(masters).filter(|e| e.tag_name().name() == "master-page") {
        if attr_local(m, "name") == Some(name) {
            return Some(m);
        }
        first = first.or(Some(m));
    }
    // A page that names no master, or names one the document does not define,
    // still gets the deck's furniture.
    first
}

/// The canvas fill: the master's `drawing-page` style, with the slide's own laid
/// over it.
fn background(styles: &Styles, setup: &super::style::PageSetup, page: Node) -> Option<Fill> {
    let mut props = setup
        .drawing_page_style
        .as_deref()
        .map(|n| styles.resolve(Family::DrawingPage, n).page);
    if let Some(own) = attr_local(page, "style-name") {
        let own = styles.resolve(Family::DrawingPage, own).page;
        // A slide that states its own fill replaces the master's rather than
        // blending with it; one that states nothing keeps the master's.
        if own.fill.kind.is_some() || own.fill.color.is_some() {
            props = Some(own);
        }
    }
    props.and_then(|p| p.fill.fill())
}

// ── shapes ───────────────────────────────────────────────────────────────────

/// Draws the shape children of a `draw:page`, a `style:master-page` or a `draw:g`.
fn walk<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    parent: Node<'a, 'a>,
    canvas: (f32, f32),
    depth: usize,
    count: &mut usize,
    master: bool,
) {
    if depth > MAX_GROUP_DEPTH {
        return;
    }
    for n in elems(parent) {
        if w.is_full() {
            return;
        }
        if *count >= MAX_SHAPES {
            ctx.notes.add(NOTE_SHAPES);
            return;
        }
        let name = n.tag_name().name();
        // A shape the document hides. `attr_local` is namespace-agnostic on
        // purpose here: the standard spells this `draw:display` and LibreOffice
        // writes `drawooo:display` for the same thing, and a hidden shape that
        // states a fill would otherwise paint a rectangle over the slide.
        if attr_local(n, "display") == Some("none") {
            continue;
        }
        // A master's prompt boxes are what Impress shows while editing, and the
        // page's own copy of the same placeholder carries the real text.
        if master && !master_shape(n) {
            continue;
        }
        match name {
            // A group states its own box and a child coordinate system. Nothing in
            // the corpus uses one, so the children are drawn in the page's
            // coordinates rather than mapped through the group's — which is right
            // whenever the two agree, and is what `svg:x` on the child means when
            // they do not.
            "g" => {
                walk(ctx, w, n, canvas, depth + 1, count, master);
            }
            "frame" | "custom-shape" | "rect" | "ellipse" | "circle" | "polygon"
            | "regular-polygon" | "path" | "caption" | "measure" => {
                if let Some(b) = box_of(ctx, n, canvas) {
                    *count += 1;
                    emit_shape(ctx, w, n, b, name);
                }
            }
            // A line or a connector is two points rather than a box. Its stroke is
            // the whole of it, so it is drawn as the thin box between its ends.
            "line" | "connector" | "polyline" => {
                if let Some(b) = line_box(n) {
                    *count += 1;
                    emit_stroke(ctx, w, n, b);
                }
            }
            // A thumbnail of another slide, a form control, and the notes page:
            // none is content this canvas can draw.
            "page-thumbnail" | "control" | "notes" | "forms" => {}
            _ => {}
        }
    }
}

/// `draw:type` values a rectangle *is* rather than degrades to, so the honest-note
/// footer does not claim a loss that did not happen.
///
/// Both spellings occur: `rectangle` is ODF's own, `ooxml-rect` is what LibreOffice
/// writes for a rectangle it imported from PresentationML. A deck converted from
/// pptx is mostly these, and noting every one of them would bury the note that
/// matters — the shapes whose outline really was thrown away.
const RECT_TYPES: &[&str] = &["rectangle", "ooxml-rect", "ooxml-rectangle"];

/// Whether drawing this custom shape as a rectangle loses its outline.
fn is_degraded_outline(n: Node) -> bool {
    if child(n, "enhanced-geometry").is_none() {
        return false;
    }
    let ty = child(n, "enhanced-geometry")
        .and_then(|g| attr_local(g, "type"))
        .or_else(|| attr_local(n, "type"));
    match ty {
        Some(t) => !RECT_TYPES.contains(&t),
        // No stated type: the outline is whatever its path says, which is not a
        // rectangle unless it happens to be one.
        None => true,
    }
}

/// Whether a master's shape is the deck's furniture rather than an editing prompt.
///
/// Three tests, because one spelling does not cover the corpus. `presentation:class`
/// and `presentation:placeholder` are what Impress writes; a deck converted from
/// PowerPoint carries **neither**, and its master's prompt boxes are ordinary
/// `draw:frame`s named "Title 1" or "Footer Placeholder 4". Naming is not something
/// to match on, so the last test is structural: a prompt is a frame with an empty
/// text box, and a frame with a picture, an object or real text is content. Shapes
/// that are not frames — a rule, a rectangle, a logo's outline — are furniture by
/// construction, since a prompt is always a framed text box.
fn master_shape(n: Node) -> bool {
    if attr_local(n, "placeholder") == Some("true") {
        return false;
    }
    if attr_local(n, "class").is_some_and(|c| PROMPT_CLASSES.contains(&c)) {
        return false;
    }
    if n.tag_name().name() != "frame" {
        return true;
    }
    let holds_media = elems(n).any(|c| {
        matches!(
            c.tag_name().name(),
            "image" | "object" | "object-ole" | "plugin" | "applet" | "floating-frame"
        )
    });
    if holds_media {
        return true;
    }
    let mut text = String::new();
    inner_text(n, &mut text);
    !text.trim().is_empty()
}

/// One shape: its box, its paint, and whatever it holds.
fn emit_shape<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    n: Node<'a, 'a>,
    b: Box,
    name: &str,
) {
    let props = shape_props(ctx, n);
    let gp = props.graphic;

    // A picture is the box: no wrapper, so the image itself carries the geometry
    // and there is nothing between it and the canvas.
    if let Some(img) = child(n, "image") {
        emit_image(ctx, w, img, b, &label_of(n));
        return;
    }
    if let Some(obj) = elems(n).find(|c| {
        matches!(
            c.tag_name().name(),
            "object" | "object-ole" | "plugin" | "applet" | "floating-frame"
        )
    }) {
        // A replacement image is the object's own rendering, which beats a box
        // saying what it stands for. Sub-documents are never rendered recursively;
        // see the module note in `odf`.
        let _ = obj;
        if let Some(img) = child(n, "image") {
            emit_image(ctx, w, img, b, &label_of(n));
        } else {
            ctx.notes.add(NOTE_OBJECT);
            emit_placeholder(w, b, "embedded object");
        }
        return;
    }

    if name == "custom-shape" && is_degraded_outline(n) {
        // Its outline is a path grammar with its own equations; the fill, the
        // stroke and the text are what a rectangle can still carry honestly.
        ctx.notes.add(NOTE_CUSTOM);
    }

    let mut css = b.css();
    paint(&mut css, &gp);
    w.open("div", &attrs(&[&attr("class", "pp-sp"), &css.to_attr()]));
    w.close();
    emit_text(ctx, w, n, b, &gp);
}

/// A line or connector: the stroke's own colour on one edge of a flat box.
fn emit_stroke<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, n: Node<'a, 'a>, b: Box) {
    let gp = shape_props(ctx, n).graphic;
    let mut css = b.css();
    // One edge, not four: `line_css` states a border on every side, and the box a
    // line is drawn in is the rectangle between its ends.
    let stroke = gp.stroke.line().and_then(|l| {
        let color = l.stroke_color();
        fmt_px(l.width_px.max(1.0) as f32).map(|w| format!("{w} solid {color}"))
    });
    // A connector with no stated stroke is still a line the reader can see.
    css.push("border-top", stroke.as_deref().unwrap_or("1px solid #808080"));
    w.open("div", &attrs(&[&attr("class", "pp-sp"), &css.to_attr()]));
    w.close();
}

/// The resolved properties of one shape. Impress writes a `presentation`-family
/// style on a placeholder and a `graphic`-family one on everything else, and both
/// spellings appear on the same slide.
fn shape_props(ctx: &Ctx, n: Node) -> Resolved {
    let mut out = Resolved::default();
    for (family, attr) in [
        (Family::Presentation, "style-name"),
        (Family::Graphic, "style-name"),
    ] {
        if let Some(name) = attr_local(n, attr) {
            let r = ctx.styles.resolve(family, name);
            super::style::merge(&mut out, &r);
        }
    }
    out
}

/// The shape's fill and stroke, through the value-level DrawingML emitters so
/// there is one place that turns a fill into CSS.
fn paint(css: &mut Style, gp: &GraphicProps) {
    if let Some(f) = gp.fill.fill() {
        push_fill(css, &f);
    }
    if let Some(l) = gp.stroke.line() {
        // `line_css` emits a whole declaration, borders on all four sides.
        css.push_decl(&line_css(&l));
    }
}

/// The shape's text, in a box inset by its own padding and anchored the way the
/// style asks.
fn emit_text<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    n: Node<'a, 'a>,
    b: Box,
    gp: &GraphicProps,
) {
    // A frame holds its text in a `draw:text-box`; a shape holds it directly.
    let content = child(n, "text-box").unwrap_or(n);
    let blocks = elems(content).filter(|c| {
        matches!(
            c.tag_name().name(),
            "p" | "h" | "list" | "table" | "section"
        )
    });
    let has_grid = elems(content).any(|c| c.tag_name().name() == "table");
    if blocks.count() == 0 {
        return;
    }
    // A producer writes an empty `text:p` into every shape it draws, and a
    // decorative bar with one space in it is not a text box: emitting one would put
    // a selectable, hit-testable box over the shape for no content. A table is
    // structure rather than text, so it survives an empty cell.
    if !has_grid {
        let mut text = String::new();
        inner_text(content, &mut text);
        if text.trim().is_empty() {
            return;
        }
    }

    let mut css = b.css();
    // The text box is placed against the canvas rather than inside the shape's
    // box, because the shape div is empty by design: a rotated shape would
    // otherwise rotate its text twice.
    let pad = |v: Option<f32>| v.filter(|v| v.is_finite() && *v >= 0.0);
    css.push_opt("padding-left", pad(gp.padding.left).and_then(fmt_px));
    css.push_opt("padding-right", pad(gp.padding.right).and_then(fmt_px));
    css.push_opt("padding-top", pad(gp.padding.top).and_then(fmt_px));
    css.push_opt("padding-bottom", pad(gp.padding.bottom).and_then(fmt_px));
    css.push("justify-content", v_anchor(gp.text_v_align));
    w.open("div", &attrs(&[&attr("class", "pp-tb"), &css.to_attr()]));
    w.open("div", &attr("class", "pp-tbi"));
    // A picture inside the text is fitted to this box, not to the slide.
    let outer = ctx.column_px;
    ctx.column_px = b.w.max(1.0);
    // The block walk, so a slide's lists and tables are the ones the page renderer
    // already draws — ODF spells them the same way wherever they sit.
    ctx.prev_style = None;
    text::walk(ctx, w, content, 0, 0);
    ctx.column_px = outer;
    w.close();
    w.close();
}

/// `draw:textarea-vertical-align` as a flex main-axis alignment.
fn v_anchor(v: Option<&'static str>) -> &'static str {
    match v {
        Some("bottom") => "flex-end",
        Some("center") | Some("middle") => "center",
        // ODF's `justify` distributes the paragraphs; `top` is the default.
        Some("justify") => "space-between",
        _ => "flex-start",
    }
}

// ── pictures ─────────────────────────────────────────────────────────────────

fn emit_image<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, img: Node<'a, 'a>, b: Box, label: &str) {
    let part = attr_local(img, "href")
        .and_then(|h| ctx.entries.resolve_href(h))
        .map(str::to_string);
    let Some(part) = part else {
        // An href the package does not hold: a linked picture, whose bytes live
        // outside the document. A preview does not reach out for them.
        emit_placeholder(w, b, "image unavailable");
        return;
    };
    let want = b.w.max(1.0).min(MAX_SHAPE_PX).round() as u32;
    match ctx.media.get(ctx.zip, ctx.budget, ctx.mb, &part, want) {
        Media::DataUri(uri) => {
            let css = b.css();
            w.void(
                "img",
                &attrs(&[
                    &attr("class", "pp-img"),
                    &attr("src", uri.as_str()),
                    &attr("alt", label),
                    &css.to_attr(),
                ]),
            );
        }
        // The class of failure is explained once in the footer, by the note
        // `MediaBudget` already owns.
        Media::Placeholder(reason) => emit_placeholder(w, b, reason),
    }
}

fn emit_placeholder(w: &mut Writer, b: Box, label: &str) {
    let css = b.css();
    emit::placeholder(w, "pp-ph", css.css(), label);
}

/// What the shape says it shows: `svg:desc`, then `svg:title`, then the producer's
/// own `draw:name`.
fn label_of(n: Node) -> String {
    let mut s = String::new();
    for name in ["desc", "title"] {
        if let Some(c) = child(n, name) {
            inner_text(c, &mut s);
            if !s.trim().is_empty() {
                break;
            }
            s.clear();
        }
    }
    if s.trim().is_empty() {
        s = attr_local(n, "name").unwrap_or("").to_string();
    }
    s.trim().chars().take(MAX_LABEL_CHARS).collect()
}

// ── geometry ─────────────────────────────────────────────────────────────────

/// One shape's box. Both extents are required: a shape with no size has nowhere to
/// be drawn, and guessing one would put a wrongly-shaped box over the slide.
fn box_of(ctx: &mut Ctx, n: Node, canvas: (f32, f32)) -> Option<Box> {
    let len = |name: &str| attr_local(n, name).and_then(length::parse_len);
    let (w, h) = (len("width")?, len("height")?);
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return None;
    }
    let t = transform(ctx, n);
    Some(Box {
        // A shape may hang off the canvas, which `.pp-doc`'s `overflow:hidden`
        // clips; what it may not be is unbounded.
        x: (len("x").unwrap_or(0.0) + t.dx).clamp(-MAX_SHAPE_PX, MAX_SHAPE_PX),
        y: (len("y").unwrap_or(0.0) + t.dy).clamp(-MAX_SHAPE_PX, MAX_SHAPE_PX),
        w: w.min(MAX_SHAPE_PX).min(canvas.0 * 4.0),
        h: h.min(MAX_SHAPE_PX).min(canvas.1 * 4.0),
        rotate: t.rotate,
    })
}

/// A line's box: the rectangle between its two ends, flattened to the top edge the
/// stroke is drawn on.
fn line_box(n: Node) -> Option<Box> {
    let len = |name: &str| attr_local(n, name).and_then(length::parse_len);
    let (x1, y1, x2, y2) = (len("x1")?, len("y1")?, len("x2")?, len("y2")?);
    if ![x1, y1, x2, y2].iter().all(|v| v.is_finite()) {
        return None;
    }
    let (w, h) = ((x2 - x1).abs(), (y2 - y1).abs());
    Some(Box {
        x: x1.min(x2).clamp(-MAX_SHAPE_PX, MAX_SHAPE_PX),
        y: y1.min(y2).clamp(-MAX_SHAPE_PX, MAX_SHAPE_PX),
        w: w.max(1.0).min(MAX_SHAPE_PX),
        // A diagonal line is drawn as the horizontal one between its ends: the
        // alternative is a rotation whose angle depends on the rendered aspect,
        // which is a lie about a different thing.
        h: h.max(1.0).min(MAX_SHAPE_PX),
        rotate: None,
    })
}

/// What a `draw:transform` contributes.
#[derive(Default)]
struct Transform {
    dx: f32,
    dy: f32,
    rotate: Option<f32>,
}

/// `draw:transform="rotate (0.4) translate (2cm 3cm)"`.
///
/// Only the two functions that survive a canvas of absolutely positioned boxes are
/// read. `skewX`/`skewY`/`matrix` need a real transform stack, and a shape carrying
/// one is drawn upright with a note rather than at a guessed position — two shapes
/// in the whole corpus state a transform at all.
fn transform(ctx: &mut Ctx, n: Node) -> Transform {
    let Some(raw) = attr_local(n, "transform") else {
        return Transform::default();
    };
    let mut t = Transform::default();
    let mut rest = raw;
    let mut unsupported = false;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(')').trim();
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        let args: Vec<&str> = rest[open + 1..open + close].split_whitespace().collect();
        match name {
            "translate" => {
                t.dx += args.first().and_then(|v| length::parse_len(v)).unwrap_or(0.0);
                t.dy += args.get(1).and_then(|v| length::parse_len(v)).unwrap_or(0.0);
            }
            // The angle is a plain number in radians, not a length.
            "rotate" => {
                t.rotate = args
                    .first()
                    .and_then(|v| v.parse::<f32>().ok())
                    .filter(|v| v.is_finite() && *v != 0.0);
            }
            "" => {}
            _ => unsupported = true,
        }
        rest = &rest[open + close + 1..];
    }
    if unsupported {
        ctx.notes.add(NOTE_TRANSFORM);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odf::text::tests::{png_bytes, Fixture, NS};

    /// A presentation package. `auto` holds `content.xml`'s automatic styles,
    /// `pages` its slides, `named` the named styles, and `master` the master page's
    /// body — the last because a master's shapes live in `styles.xml`.
    fn deck(tag: &str, named: &str, master: &str, auto: &str, pages: &str) -> Fixture {
        deck_media(tag, named, master, auto, pages, &[])
    }

    fn deck_media(
        tag: &str,
        named: &str,
        master: &str,
        auto: &str,
        pages: &str,
        media: &[(&str, Vec<u8>)],
    ) -> Fixture {
        let styles = format!(
            "<office:document-styles {NS} \
             xmlns:presentation=\"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0\">\
             <office:styles>{named}</office:styles>\
             <office:automatic-styles>\
             <style:page-layout style:name=\"pm1\"><style:page-layout-properties \
             fo:page-width=\"10in\" fo:page-height=\"7.5in\"/></style:page-layout>\
             </office:automatic-styles>\
             <office:master-styles>\
             <style:master-page style:name=\"Default\" style:page-layout-name=\"pm1\" \
             draw:style-name=\"dp1\">{master}</style:master-page>\
             </office:master-styles></office:document-styles>"
        );
        let content = format!(
            "<office:document-content {NS} \
             xmlns:presentation=\"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0\">\
             <office:automatic-styles>{auto}</office:automatic-styles>\
             <office:body><office:presentation>{pages}\
             </office:presentation></office:body></office:document-content>"
        );
        let mut parts: Vec<(&str, Vec<u8>)> = vec![
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.presentation".to_vec(),
            ),
            ("styles.xml", styles.into_bytes()),
            ("content.xml", content.into_bytes()),
        ];
        parts.extend(media.iter().map(|(n, b)| (*n, b.clone())));
        Fixture::new(tag, &parts)
    }

    /// One slide holding `shapes`.
    fn page(name: &str, shapes: &str) -> String {
        format!(
            "<draw:page draw:name=\"{name}\" draw:master-page-name=\"Default\">{shapes}\
             </draw:page>"
        )
    }

    /// A framed text box at a stated box.
    fn text_frame(attrs: &str, text: &str) -> String {
        format!(
            "<draw:frame svg:x=\"1in\" svg:y=\"1in\" svg:width=\"4in\" svg:height=\"2in\" \
             {attrs}><draw:text-box><text:p>{text}</text:p></draw:text-box></draw:frame>"
        )
    }

    #[test]
    fn a_deck_renders_one_slide_onto_a_canvas_of_the_stated_size() {
        let f = deck("canvas", "", "", "", &page("First", &text_frame("", "café")));
        let doc = f.doc();
        assert_eq!(doc.shape, Shape::Slide);
        // 10 × 7.5in at 96dpi.
        assert_eq!(doc.natural, Some((960.0, 720.0)));
        assert!(doc.page.is_none(), "a canvas has no page box");
        assert_eq!(doc.section, 0);
        assert!(doc.html.contains("class=\"pp-doc\""), "{}", doc.html);
        assert!(doc.html.contains("width:960px;height:720px"), "{}", doc.html);
        assert!(doc.html.contains("café"), "{}", doc.html);
    }

    #[test]
    fn a_presentation_with_no_page_layout_falls_back_to_a_slide_shaped_canvas() {
        // No `style:page-layout` at all: the page default is Letter *portrait*,
        // which is the wrong shape for a deck, so the 4:3 canvas wins.
        let styles = format!(
            "<office:document-styles {NS}><office:styles/></office:document-styles>"
        );
        let content = format!(
            "<office:document-content {NS}><office:body><office:presentation>\
             {}</office:presentation></office:body></office:document-content>",
            page("One", &text_frame("", "café"))
        );
        let f = Fixture::new(
            "nolayout",
            &[
                ("styles.xml", styles.into_bytes()),
                ("content.xml", content.into_bytes()),
            ],
        );
        assert_eq!(f.doc().natural, Some(slideshape::DEFAULT_SLIDE));
    }

    #[test]
    fn every_slide_in_the_strip_gets_its_own_title() {
        let pages = format!(
            "{}{}{}",
            page(
                "page1",
                "<draw:frame presentation:class=\"title\" svg:x=\"0in\" svg:y=\"0in\" \
                 svg:width=\"4in\" svg:height=\"1in\"><draw:text-box>\
                 <text:p>Café  naïve</text:p></draw:text-box></draw:frame>"
            ),
            page("Widget", &text_frame("", "second")),
            page("page3", &text_frame("", "third")),
        );
        let doc = deck("titles", "", "", "", &pages).doc();
        // The title's own text, whitespace collapsed; then a name a person typed;
        // then a position, because `page3` is the producer's own.
        assert_eq!(
            doc.sections,
            vec![
                "Café naïve".to_string(),
                "Widget".to_string(),
                "Slide 3".to_string()
            ]
        );
    }

    #[test]
    fn a_section_past_the_end_clamps_to_the_last_slide() {
        let pages = format!(
            "{}{}",
            page("One", &text_frame("", "first")),
            page("Two", &text_frame("", "second"))
        );
        let f = deck("clamp", "", "", "", &pages);
        let doc = super::super::render(f.path(), Some(99), &[]).expect("render");
        assert_eq!(doc.section, 1);
        assert!(doc.html.contains("second"), "{}", doc.html);
        assert!(!doc.html.contains("first"), "{}", doc.html);
    }

    #[test]
    fn a_deck_with_no_slides_is_an_error_rather_than_an_empty_canvas() {
        let f = deck("empty", "", "", "", "");
        assert!(super::super::render(f.path(), None, &[]).is_err());
    }

    #[test]
    fn a_shape_carries_its_own_geometry_fill_and_stroke() {
        let named = "<style:style style:name=\"gr1\" style:family=\"graphic\">\
             <style:graphic-properties draw:fill=\"solid\" draw:fill-color=\"#ff0000\" \
             draw:stroke=\"solid\" svg:stroke-color=\"#0000ff\" svg:stroke-width=\"2pt\"/>\
             </style:style>";
        let shapes = "<draw:custom-shape draw:style-name=\"gr1\" svg:x=\"2in\" svg:y=\"1in\" \
             svg:width=\"3in\" svg:height=\"1.5in\"><text:p>café</text:p></draw:custom-shape>";
        let html = deck("paint", named, "", "", &page("One", shapes)).body_html();
        assert!(html.contains("left:192px;top:96px;"), "{html}");
        assert!(html.contains("width:288px;height:144px;"), "{html}");
        assert!(html.contains("background-color:#ff0000"), "{html}");
        // One declaration from `line_css`, not a property name pasted twice. 2pt
        // is 2.67px: the width is the document's, unrounded.
        assert!(html.contains("border:2.67px solid #0000ff;"), "{html}");
        assert!(!html.contains("border:border"), "{html}");
    }

    #[test]
    fn a_hidden_shape_is_not_drawn_in_either_spelling() {
        for attr in ["draw:display=\"none\"", "drawooo:display=\"none\""] {
            let shapes = format!(
                "<draw:custom-shape {attr} svg:x=\"0in\" svg:y=\"0in\" svg:width=\"1in\" \
                 svg:height=\"1in\"><text:p>hidden</text:p></draw:custom-shape>{}",
                text_frame("", "shown")
            );
            let content = format!(
                "<office:document-content {NS} \
                 xmlns:drawooo=\"http://openoffice.org/2010/draw\">\
                 <office:body><office:presentation>{}\
                 </office:presentation></office:body></office:document-content>",
                page("One", &shapes)
            );
            let f = Fixture::new(
                "hidden",
                &[
                    ("mimetype", b"application/vnd.oasis.opendocument.presentation".to_vec()),
                    ("content.xml", content.into_bytes()),
                ],
            );
            let html = f.body_html();
            assert!(!html.contains("hidden"), "{attr}: {html}");
            assert!(html.contains("shown"), "{attr}: {html}");
        }
    }

    #[test]
    fn a_picture_is_the_box_it_is_framed_at() {
        let shapes = "<draw:frame svg:x=\"1in\" svg:y=\"0.5in\" svg:width=\"2in\" \
             svg:height=\"1in\"><svg:desc>a naïve café</svg:desc>\
             <draw:image xlink:href=\"Pictures/1.png\"/></draw:frame>";
        let f = deck_media(
            "pic",
            "",
            "",
            "",
            &page("One", shapes),
            &[("Pictures/1.png", png_bytes(48, 24))],
        );
        let html = f.body_html();
        assert!(html.contains("<img class=\"pp-img\" src=\"data:image/"), "{html}");
        assert!(html.contains("left:96px;top:48px;width:192px;height:96px;"), "{html}");
        // The author's own description is the alt text.
        assert!(html.contains("alt=\"a naïve café\""), "{html}");
    }

    #[test]
    fn an_unresolvable_href_and_an_embedded_object_leave_labelled_boxes() {
        let shapes = "<draw:frame svg:x=\"0in\" svg:y=\"0in\" svg:width=\"2in\" \
             svg:height=\"1in\"><draw:image xlink:href=\"../secret.png\"/></draw:frame>\
             <draw:frame svg:x=\"3in\" svg:y=\"0in\" svg:width=\"2in\" svg:height=\"1in\">\
             <draw:object xlink:href=\"./Object 1\"/></draw:frame>";
        let doc = deck("boxes", "", "", "", &page("One", shapes)).doc();
        assert!(!doc.html.contains("data:image/"), "{}", doc.html);
        assert!(!doc.html.contains("secret"), "{}", doc.html);
        assert!(doc.html.contains("image unavailable"), "{}", doc.html);
        assert!(doc.html.contains("embedded object"), "{}", doc.html);
        assert!(doc.notes.iter().any(|n| n == NOTE_OBJECT), "{:?}", doc.notes);
    }

    #[test]
    fn a_masters_furniture_is_drawn_and_its_prompt_boxes_are_not() {
        let master = "<draw:custom-shape svg:x=\"0in\" svg:y=\"0in\" svg:width=\"10in\" \
             svg:height=\"0.5in\" draw:style-name=\"bar\"/>\
             <draw:frame presentation:class=\"title\" svg:x=\"1in\" svg:y=\"1in\" \
             svg:width=\"4in\" svg:height=\"1in\"><draw:text-box>\
             <text:p>Click to add title</text:p></draw:text-box></draw:frame>\
             <draw:frame draw:name=\"Footer Placeholder 4\" svg:x=\"1in\" svg:y=\"6in\" \
             svg:width=\"4in\" svg:height=\"0.5in\"><draw:text-box><text:p> </text:p>\
             </draw:text-box></draw:frame>\
             <draw:frame draw:name=\"Logo\" svg:x=\"8in\" svg:y=\"0in\" svg:width=\"1in\" \
             svg:height=\"1in\"><draw:text-box><text:p>Widget Inc</text:p>\
             </draw:text-box></draw:frame>";
        let named = "<style:style style:name=\"bar\" style:family=\"graphic\">\
             <style:graphic-properties draw:fill=\"solid\" draw:fill-color=\"#123456\"/>\
             </style:style>";
        let html = deck("master", named, master, "", &page("One", &text_frame("", "slide text")))
            .body_html();
        // Furniture: a filled bar, and a frame with real text in it.
        assert!(html.contains("background-color:#123456"), "{html}");
        assert!(html.contains("Widget Inc"), "{html}");
        // Prompts, in both spellings: the labelled one, and the pptx-converted one
        // that carries no `presentation:class` at all and only whitespace.
        assert!(!html.contains("Click to add title"), "{html}");
        assert!(html.contains("slide text"), "{html}");
    }

    #[test]
    fn an_empty_text_box_gets_no_selectable_box_over_the_shape() {
        let shapes = "<draw:custom-shape svg:x=\"0in\" svg:y=\"0in\" svg:width=\"2in\" \
             svg:height=\"1in\"><text:p> </text:p></draw:custom-shape>";
        let html = deck("emptytb", "", "", "", &page("One", shapes)).body_html();
        // The shape itself is drawn; the text box a producer writes into every
        // shape is not, or it would sit over the shape catching drags.
        assert!(html.contains("class=\"pp-sp\""), "{html}");
        assert!(!html.contains("class=\"pp-tb\""), "{html}");
    }

    #[test]
    fn text_is_anchored_the_way_the_style_asks() {
        let named = "<style:style style:name=\"mid\" style:family=\"graphic\">\
             <style:graphic-properties draw:textarea-vertical-align=\"middle\"/></style:style>\
             <style:style style:name=\"bot\" style:family=\"graphic\">\
             <style:graphic-properties draw:textarea-vertical-align=\"bottom\"/></style:style>";
        let shapes = format!(
            "{}{}{}",
            text_frame("draw:style-name=\"mid\"", "centre"),
            text_frame("draw:style-name=\"bot\"", "bottom"),
            text_frame("", "top"),
        );
        let html = deck("anchor", named, "", "", &page("One", &shapes)).body_html();
        assert!(html.contains("justify-content:center"), "{html}");
        assert!(html.contains("justify-content:flex-end"), "{html}");
        assert!(html.contains("justify-content:flex-start"), "{html}");
    }

    #[test]
    fn a_slide_draws_lists_and_tables_through_the_same_walk_as_a_page() {
        let named = "<text:list-style style:name=\"L1\">\
             <text:list-level-style-bullet text:level=\"1\" text:bullet-char=\"•\"/>\
             </text:list-style>";
        let shapes = "<draw:frame svg:x=\"1in\" svg:y=\"1in\" svg:width=\"4in\" \
             svg:height=\"3in\"><draw:text-box>\
             <text:list text:style-name=\"L1\"><text:list-item><text:p>café</text:p>\
             </text:list-item></text:list>\
             <table:table><table:table-row><table:table-cell><text:p>cell</text:p>\
             </table:table-cell></table:table-row></table:table>\
             </draw:text-box></draw:frame>";
        let html = deck("blocks", named, "", "", &page("One", shapes)).body_html();
        // The slide's own class names, not the page column's.
        assert!(html.contains("class=\"pp-p pp-li\""), "{html}");
        assert!(html.contains("class=\"pp-bu\""), "{html}");
        assert!(html.contains("class=\"pp-tbl"), "{html}");
        assert!(!html.contains("of-p"), "{html}");
        assert!(!html.contains("of-tbl"), "{html}");
        // The cell is styled by tag on a slide, so it publishes no class — and an
        // empty `class=""` would be litter.
        assert!(!html.contains("class=\"\""), "{html}");
    }

    #[test]
    fn a_rotation_reaches_the_box_and_a_skew_is_noted_instead() {
        let shapes = "<draw:custom-shape draw:transform=\"rotate (0.5) translate (1in 2in)\" \
             svg:width=\"2in\" svg:height=\"1in\"><text:p>turned</text:p></draw:custom-shape>";
        let doc = deck("rot", "", "", "", &page("One", shapes)).doc();
        // ODF states radians counter-clockwise; CSS turns clockwise.
        assert!(doc.html.contains("transform:rotate(-28.6"), "{}", doc.html);
        // `translate` is where the shape is, since a box has no transform origin
        // to slide from.
        assert!(doc.html.contains("left:96px;top:192px;"), "{}", doc.html);
        assert!(doc.notes.iter().all(|n| n != NOTE_TRANSFORM), "{:?}", doc.notes);

        let skewed = "<draw:custom-shape draw:transform=\"skewX (0.2)\" svg:width=\"2in\" \
             svg:height=\"1in\"><text:p>skewed</text:p></draw:custom-shape>";
        let doc = deck("skew", "", "", "", &page("One", skewed)).doc();
        assert!(doc.notes.iter().any(|n| n == NOTE_TRANSFORM), "{:?}", doc.notes);
    }

    #[test]
    fn a_custom_outline_is_noted_only_when_a_rectangle_loses_something() {
        let rect = "<draw:custom-shape svg:width=\"2in\" svg:height=\"1in\">\
             <draw:enhanced-geometry draw:type=\"ooxml-rect\"/></draw:custom-shape>";
        let doc = deck("rect", "", "", "", &page("One", rect)).doc();
        // A rectangle drawn as a rectangle lost nothing, and a note on every shape
        // of a converted deck would bury the ones that matter.
        assert!(doc.notes.iter().all(|n| n != NOTE_CUSTOM), "{:?}", doc.notes);

        let star = "<draw:custom-shape svg:width=\"2in\" svg:height=\"1in\">\
             <draw:enhanced-geometry draw:type=\"star5\"/></draw:custom-shape>";
        let doc = deck("star", "", "", "", &page("One", star)).doc();
        assert!(doc.notes.iter().any(|n| n == NOTE_CUSTOM), "{:?}", doc.notes);
    }

    #[test]
    fn a_line_is_the_stroke_between_its_ends() {
        let named = "<style:style style:name=\"ln\" style:family=\"graphic\">\
             <style:graphic-properties draw:stroke=\"solid\" svg:stroke-color=\"#00ff00\" \
             svg:stroke-width=\"3pt\"/></style:style>";
        let shapes = "<draw:line draw:style-name=\"ln\" svg:x1=\"1in\" svg:y1=\"2in\" \
             svg:x2=\"4in\" svg:y2=\"2in\"/>";
        let html = deck("line", named, "", "", &page("One", shapes)).body_html();
        // One edge, not four: a border on every side would draw a box.
        assert!(html.contains("border-top:4px solid #00ff00"), "{html}");
        assert!(!html.contains("border:4px"), "{html}");
        assert!(html.contains("left:96px;top:192px;width:288px"), "{html}");
    }

    #[test]
    fn the_canvas_takes_the_masters_background_and_the_slides_own_overrides_it() {
        let named = "<style:style style:name=\"dp1\" style:family=\"drawing-page\">\
             <style:drawing-page-properties draw:fill=\"solid\" draw:fill-color=\"#101010\"/>\
             </style:style>\
             <style:style style:name=\"dp2\" style:family=\"drawing-page\">\
             <style:drawing-page-properties draw:fill=\"solid\" draw:fill-color=\"#202020\"/>\
             </style:style>";
        let html = deck("bg", named, "", "", &page("One", &text_frame("", "x"))).body_html();
        assert!(html.contains("background-color:#101010"), "{html}");

        let own = "<draw:page draw:name=\"One\" draw:master-page-name=\"Default\" \
             draw:style-name=\"dp2\">"
            .to_string()
            + &text_frame("", "x")
            + "</draw:page>";
        let html = deck("bg2", named, "", "", &own).body_html();
        assert!(html.contains("background-color:#202020"), "{html}");
        assert!(!html.contains("#101010"), "{html}");
    }

    #[test]
    fn speaker_notes_are_not_drawn_and_not_noted() {
        let shapes = format!(
            "{}<presentation:notes><draw:frame svg:width=\"4in\" svg:height=\"2in\">\
             <draw:text-box><text:p>only for the speaker</text:p></draw:text-box>\
             </draw:frame></presentation:notes>",
            text_frame("", "on the slide")
        );
        let doc = deck("notes", "", "", "", &page("One", &shapes)).doc();
        assert!(!doc.html.contains("only for the speaker"), "{}", doc.html);
        assert!(doc.html.contains("on the slide"), "{}", doc.html);
        // Deliberately *not* noted: every slide of every deck has a notes page, so
        // a footer line about it is on every slide and tells the reader nothing.
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
    }

    #[test]
    fn terms_are_marked_on_the_slide_that_holds_them() {
        let f = deck("terms", "", "", "", &page("One", &text_frame("", "the runner was running")));
        let doc = super::super::render(f.path(), None, &["run".to_string()]).expect("render");
        assert!(doc.html.contains("<mark class=\"preview-hl\""), "{}", doc.html);
        assert!(doc.best_mark_id.is_some());
    }

    #[test]
    fn the_shape_count_is_capped_and_says_so() {
        let one = "<draw:custom-shape svg:x=\"0in\" svg:y=\"0in\" svg:width=\"1in\" \
             svg:height=\"1in\"><text:p>x</text:p></draw:custom-shape>";
        let shapes: String = std::iter::repeat_n(one, MAX_SHAPES + 20).collect();
        let doc = deck("shapecap", "", "", "", &page("One", &shapes)).doc();
        let body = doc.html.split_once("</style>").map(|(_, b)| b).unwrap_or("");
        assert_eq!(body.matches("class=\"pp-sp\"").count(), MAX_SHAPES);
        assert!(doc.notes.iter().any(|n| n == NOTE_SHAPES), "{:?}", doc.notes);
    }

    #[test]
    fn a_shape_with_no_size_is_not_placed_at_all() {
        let shapes = "<draw:custom-shape svg:x=\"1in\" svg:y=\"1in\" svg:width=\"2in\">\
             <text:p>half a box</text:p></draw:custom-shape>";
        let html = deck("nosize", "", "", "", &page("One", shapes)).body_html();
        // Both extents or neither: a guessed height puts a wrongly-shaped box on
        // the slide.
        assert!(!html.contains("half a box"), "{html}");
        assert!(!html.contains("class=\"pp-sp\""), "{html}");
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn indexed_sections_are_the_slides_in_body_order() {
        let f = deck(
            "index-deck",
            "",
            "",
            "",
            &format!(
                "{}{}",
                page("One", &text_frame("", "café first")),
                page("Two", &text_frame("", "naïve second")),
            ),
        );
        let sections = crate::odf::extract_sections(f.path()).expect("extract");
        // The index and the section strip must agree, or a jump opens the wrong slide.
        assert_eq!(sections.len(), f.doc().sections.len());
        assert!(sections[0].text.contains("café first"), "{:?}", sections[0].text);
        assert!(sections[1].text.contains("naïve second"), "{:?}", sections[1].text);
        // A slide's name is not indexed: its title is already in its own text.
        assert_eq!(sections[0].name, "");
    }

    #[test]
    fn speaker_notes_are_not_indexed() {
        // The renderer drops the notes page (see the test above), so a hit in it
        // could never be highlighted — and every slide of a deck carries one, which
        // would make every slide match a query for its own footer.
        let shapes = format!(
            "{}<presentation:notes><draw:frame svg:width=\"4in\" svg:height=\"2in\">\
             <draw:text-box><text:p>only for the speaker</text:p></draw:text-box>\
             </draw:frame></presentation:notes>",
            text_frame("", "on the slide")
        );
        let f = deck("index-notes", "", "", "", &page("One", &shapes));
        let text = &crate::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("on the slide"), "{text:?}");
        assert!(!text.contains("only for the speaker"), "{text:?}");
    }

    #[test]
    fn master_furniture_is_not_indexed() {
        // The slide draws it, but it is the same text on every slide of the deck.
        let f = deck(
            "index-master",
            "",
            &text_frame("", "every slide footer"),
            "",
            &page("One", &text_frame("", "café")),
        );
        assert!(f.doc().html.contains("every slide footer"), "the master is drawn");
        let text = &crate::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("café"), "{text:?}");
        assert!(!text.contains("every slide footer"), "{text:?}");
    }
}
