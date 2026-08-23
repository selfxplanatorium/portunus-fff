//! `w:drawing`, `w:pict` and `w:object`: the images a document carries, and the
//! boxes the graphics this preview cannot draw degrade to.
//!
//! Everything here becomes a [`Graphic`] run, so an image lands in the line it
//! belongs to and the paragraph emitter places it. The slide renderer's approach
//! — an absolutely positioned box per picture — has nothing to be absolute
//! against here: the column reflows to the reader's width and is not paginated,
//! so a page-relative position would put the picture on top of text that has
//! moved somewhere else.
//!
//! Known approximations, all deliberate: an anchored drawing becomes a float or
//! falls back into the text flow ([`Wrap::of`]), `a:srcRect` crops are not
//! applied (see [`image`]), and a chart, diagram or OLE object is a labelled box
//! at its real size.

use super::super::emit;
use super::super::html::{emu_to_px, pt_to_px};
use super::super::media::{self, Media};
use super::super::model::{Break, Graphic, Run, Side};
use super::super::opc;
use super::super::xml::{attr_i64, attr_local, child, descendant, elems, text_of};
use super::{body, Ctx};
use roxmltree::Node;

/// Graphics — images *and* placeholders — per document, on top of the byte caps
/// `MediaBudget` enforces. Each one is a `data:` URI WebKit has to parse and hold,
/// and the writer's byte cap alone would let a thousand tiny images through.
const MAX_GRAPHICS: usize = 200;

/// Ceiling on either axis of a drawing's box. Extents are stated in EMU by an
/// untrusted document, and the text column is the real limit on the width (see
/// [`fit`]); this bounds the height, and the width when there is no column.
const MAX_BOX_PX: f32 = 4096.0;

/// Box for a drawing that states no extent anywhere. Word always writes one, so
/// this is the malformed case: something the reader can see and scroll past.
const DEFAULT_BOX_PX: f32 = 96.0;

/// Characters of `wp:docPr@descr` kept. It is document text in an attribute, so
/// it is untrusted and unbounded; a paragraph of alt text is not a label.
const MAX_LABEL_CHARS: usize = 200;

/// `mc:AlternateContent` nesting. Two is the real depth (a choice holding a
/// drawing that itself has a fallback); past this a generated file is recursing.
const MAX_ALT_DEPTH: usize = 4;

/// A drawing hugs an edge when its near side is within this fraction of the
/// column of it. Anything further in is centred or free-floating, and floats
/// nothing — see [`side`].
const EDGE_FRACTION: f32 = 0.25;

const NOTE_COUNT: &str = "Some images not shown";

// Word for the two frame kinds a report is actually built around. Deliberately
// the same strings the slide renderer uses (`pptx::shapes`), so a reader who
// previews both formats is told the same thing in the same words.
const NOTE_CHART: &str = "Charts not drawn";
const NOTE_DIAGRAM: &str = "SmartArt not drawn";

/// The OLE relationship kind. `emit::graphic_label` keys on the `/ole` in it, so
/// an object whose own relationship cannot be read still labels through the
/// shared vocabulary instead of a literal of its own.
const OLE_KIND: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject";

/// One drawing element from a run: `w:drawing`, `w:pict`, `w:object`, or the
/// `mc:AlternateContent` that wraps one. Pushes zero or more runs — zero when
/// there is no graphic to show, more than one when the wrap needs a line of its
/// own.
pub fn emit_drawing(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node) {
    emit_node(ctx, out, n, 0);
}

fn emit_node(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node, depth: usize) {
    if depth > MAX_ALT_DEPTH {
        return;
    }
    match n.tag_name().name() {
        // A newer element with an older fallback — usually a `wps` shape whose
        // fallback is VML, or a metafile whose fallback is a raster. Markup
        // compatibility says a consumer supporting none of the `Requires`
        // namespaces takes the fallback, and that is exactly this renderer.
        "AlternateContent" => {
            if let Some(branch) = media::prefer_raster_branch(n) {
                for c in elems(branch) {
                    emit_node(ctx, out, c, depth + 1);
                }
            }
        }
        "drawing" => drawing(ctx, out, n),
        "pict" => pict(ctx, out, n),
        "object" => object(ctx, out, n),
        _ => {}
    }
}

// ── w:drawing ────────────────────────────────────────────────────────────────

fn drawing(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node) {
    // A `wps:txbx` shape is a text box: real paragraphs, checked before the
    // picture path because a text box may *also* carry a picture fill and the text
    // is the content.
    if let Some(c) = txbx(n) {
        body::hoist(ctx, c);
        return;
    }
    let Some(frame) = frame_of(n) else {
        return;
    };
    let (w_px, h_px) = box_of(frame, ctx.column_px);
    let label = doc_pr_label(frame);
    let data = child(frame, "graphic").and_then(|g| child(g, "graphicData"));
    let uri = data.and_then(|d| d.attribute("uri")).unwrap_or("");

    // A blip anywhere under the `graphicData` names a picture part: `pic:pic` is
    // the common shape, but a shape with a picture fill carries one too, and both
    // resolve through the same relationship.
    let embed = data
        .and_then(|d| descendant(d, "blip"))
        .and_then(|b| attr_local(b, "embed"));

    let g = match embed {
        Some(rid) => image(ctx, rid, w_px, h_px, label),
        // A `graphicData` that names a picture but embeds no blip is a *linked*
        // image: the bytes live outside the package, and a preview does not reach
        // out to the filesystem or the network for them.
        None if uri.contains("/picture") => {
            placeholder(w_px, h_px, label, "image unavailable")
        }
        // A chart or a SmartArt diagram is data plus a layout, never a picture,
        // and an OLE object needs the application that made it. A labelled box at
        // the document's own size is what is left — and the label, not the
        // author's description, because it is the label that explains the box.
        None => {
            // The box says *what* it stands for; the footer says the preview will
            // never draw it, which a reader looking at a report's one chart cannot
            // tell from the box alone.
            if uri.contains("/chart") {
                ctx.notes.add(NOTE_CHART);
            } else if uri.contains("/diagram") {
                ctx.notes.add(NOTE_DIAGRAM);
            }
            placeholder(w_px, h_px, String::new(), emit::graphic_label(uri))
        }
    };
    let wrap = if frame.tag_name().name() == "anchor" {
        Wrap::of(frame, w_px, ctx.column_px)
    } else {
        Wrap::Inline
    };
    push(ctx, out, g, wrap);
}

/// The `w:txbxContent` of a text box — DrawingML (`wps:txbx`) or VML
/// (`v:textbox`) — if it holds any block content at all. An empty one is a shape
/// with a text frame nobody typed in, and hoisting it would put an empty bordered
/// box in the middle of the page.
fn txbx<'a>(n: Node<'a, 'a>) -> Option<Node<'a, 'a>> {
    let c = descendant(n, "txbxContent")?;
    elems(c).next().map(|_| c)
}

/// The `wp:inline` (in the text flow) or `wp:anchor` (floated or positioned)
/// inside a `w:drawing`. Either can sit behind an `mc:AlternateContent` written
/// inside the drawing rather than around it.
fn frame_of<'a>(n: Node<'a, 'a>) -> Option<Node<'a, 'a>> {
    let is_frame = |c: &Node| matches!(c.tag_name().name(), "inline" | "anchor");
    elems(n).find(is_frame).or_else(|| {
        elems(n)
            .find_map(media::prefer_raster_branch)
            .and_then(|b| elems(b).find(is_frame))
    })
}

/// The drawing's box in px: `wp:extent`, else the first `a:ext` under it (a frame
/// written without an extent still carries the shape's own), else a default
/// square.
fn box_of(frame: Node, column_px: f32) -> (f32, f32) {
    let px = |n: Node, name: &str| {
        attr_i64(n, name)
            .map(emu_to_px)
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    let ext = child(frame, "extent")
        .or_else(|| descendant(frame, "ext"))
        .and_then(|e| Some((px(e, "cx")?, px(e, "cy")?)));
    match ext {
        Some((w, h)) => fit(w, h, column_px),
        None => (DEFAULT_BOX_PX, DEFAULT_BOX_PX),
    }
}

/// Scales a box down until the column can hold it, keeping its aspect. A stated
/// width wider than the page would otherwise push the column open and leave the
/// reader panning, and clamping one axis alone would stretch the image.
fn fit(w: f32, h: f32, column_px: f32) -> (f32, f32) {
    let max_w = if column_px.is_finite() && column_px >= 1.0 {
        column_px.min(MAX_BOX_PX)
    } else {
        MAX_BOX_PX
    };
    let scale = (max_w / w).min(MAX_BOX_PX / h).min(1.0);
    ((w * scale).max(1.0), (h * scale).max(1.0))
}

/// `wp:docPr@descr` (the author's alt text) then `@name` (Word's own
/// "Picture 3"): the image's `alt`, and the placeholder's label when the picture
/// cannot be shown.
fn doc_pr_label(frame: Node) -> String {
    child(frame, "docPr")
        .and_then(|p| attr_local(p, "descr").or_else(|| attr_local(p, "name")))
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_LABEL_CHARS)
        .collect()
}

// ── wrapping ─────────────────────────────────────────────────────────────────

/// How an anchored drawing joins the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrap {
    Inline,
    Float(Side),
    /// Text above and below it, none beside it.
    OwnLine,
}

impl Wrap {
    /// Word positions an anchor absolutely — relative to a margin, a column, the
    /// page or the paragraph — and lets the text wrap around it. A reflowing,
    /// unpaginated column cannot honour that position: the text it was measured
    /// against is somewhere else, so an absolutely placed drawing would overlap
    /// unpredictable words. A float keeps the two things that survive
    /// reflow — the side the author chose and text staying out from under the
    /// box — and everything else degrades into the text flow, which is the
    /// honest approximation rather than a guess at a position.
    fn of(anchor: Node, w_px: f32, column_px: f32) -> Wrap {
        let Some(wrap) = elems(anchor).find(|c| c.tag_name().name().starts_with("wrap")) else {
            return Wrap::Inline;
        };
        match wrap.tag_name().name() {
            "wrapSquare" | "wrapTight" | "wrapThrough" => {
                match side(anchor, wrap, w_px, column_px) {
                    Some(s) => Wrap::Float(s),
                    None => Wrap::Inline,
                }
            }
            "wrapTopAndBottom" => Wrap::OwnLine,
            // `wp:wrapNone` is behind or in front of the text. Inline: the
            // overlap it asks for lands on different words at every width, and a
            // watermark drawn over the middle of a paragraph is worse than one
            // sitting between two.
            _ => Wrap::Inline,
        }
    }
}

/// Which edge a wrapped anchor hugs, or `None` when it hugs neither — a centred
/// figure, or one positioned somewhere in the middle of the column.
fn side(anchor: Node, wrap: Node, w_px: f32, column_px: f32) -> Option<Side> {
    // `@wrapText` states which side of the drawing the *text* runs down, so it
    // names the drawing's own side by elimination — and it is stated far more
    // often than a horizontal position this renderer can resolve.
    match attr_local(wrap, "wrapText") {
        Some("left") => return Some(Side::Right),
        Some("right") => return Some(Side::Left),
        _ => {}
    }
    let pos = child(anchor, "positionH")?;
    if let Some(a) = child(pos, "align").and_then(text_of) {
        return match a.trim() {
            // "inside"/"outside" alternate with the page parity, which an
            // unpaginated column does not have; the odd-page reading is the one
            // the author saw first.
            "left" | "inside" => Some(Side::Left),
            "right" | "outside" => Some(Side::Right),
            _ => None,
        };
    }
    // An offset is honoured only against the text column itself. A page-relative
    // one would need the page's margins to say which side of the column it lands
    // on, and guessing wrong floats a drawing over the text it belongs beside.
    match attr_local(pos, "relativeFrom") {
        None | Some("margin") | Some("column") => {}
        _ => return None,
    }
    let off = child(pos, "posOffset")
        .and_then(text_of)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(emu_to_px)
        .filter(|v| v.is_finite())?;
    let edge = column_px * EDGE_FRACTION;
    if off <= edge {
        Some(Side::Left)
    } else if off + w_px >= column_px - edge {
        Some(Side::Right)
    } else {
        None
    }
}

// ── w:pict / w:object (legacy VML) ───────────────────────────────────────────

/// Legacy VML. Word still writes a `w:pict` for anything pasted from an old
/// document, and the image inside one is an ordinary package part reached through
/// an ordinary relationship.
fn pict(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node) {
    // `v:textbox` around a `w:txbxContent` — the legacy spelling of a text box,
    // and the `mc:Fallback` a modern `wps` one is usually paired with.
    if let Some(c) = txbx(n) {
        body::hoist(ctx, c);
        return;
    }
    let Some((rid, data, shape)) = image_data(n) else {
        // A VML shape with no image data and no text is a horizontal rule or an
        // empty box. A dashed placeholder in the middle of a sentence would be
        // worse than the nothing that stands there now.
        return;
    };
    let (w_px, h_px) = vml_box(shape, ctx.column_px);
    // `o:title` on the image data, then the shape's own VML alt text: the two
    // places a legacy picture states what it shows.
    let label = attr_local(data, "title")
        .or_else(|| attr_local(shape, "alt"))
        .or_else(|| attr_local(shape, "title"))
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_LABEL_CHARS)
        .collect();
    let g = image(ctx, rid, w_px, h_px, label);
    push(ctx, out, g, Wrap::Inline);
}

/// An embedded OLE object. Word writes a picture *of* it — the `v:imagedata`
/// preview — beside the object itself, and that preview is the only thing a
/// viewer without the originating application can show.
fn object(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node) {
    let img = image_data(n);
    let (w_px, h_px) = match img.map(|(_, _, s)| s).or_else(|| descendant(n, "shape")) {
        Some(s) => vml_box(s, ctx.column_px),
        None => (DEFAULT_BOX_PX, DEFAULT_BOX_PX),
    };
    let g = match img {
        Some((rid, _, _)) => image(ctx, rid, w_px, h_px, String::new()),
        None => placeholder(w_px, h_px, String::new(), emit::graphic_label(&ole_kind(ctx, n))),
    };
    push(ctx, out, g, Wrap::Inline);
}

/// The relationship kind of the embedded object, so its label comes out of
/// [`emit::graphic_label`] like every other undrawable graphic's.
fn ole_kind(ctx: &Ctx, n: Node) -> String {
    descendant(n, "OLEObject")
        .and_then(|o| attr_local(o, "id"))
        .and_then(|id| ctx.rels.get(id))
        .map(|r| r.kind.clone())
        .unwrap_or_else(|| OLE_KIND.to_string())
}

/// The first `v:imagedata` carrying a relationship id, as
/// `(rId, the imagedata, the shape around it)`. The two nodes are both needed:
/// the box is on the shape, the title on the image data.
fn image_data<'a>(n: Node<'a, 'a>) -> Option<(&'a str, Node<'a, 'a>, Node<'a, 'a>)> {
    let data = n
        .descendants()
        .filter(|d| d.is_element() && d.tag_name().name() == "imagedata")
        .find(|d| attr_local(*d, "id").is_some())?;
    let shape = data.parent().filter(|p| p.is_element()).unwrap_or(n);
    Some((attr_local(data, "id")?, data, shape))
}

/// The box from a VML shape's `style="width:…;height:…"`. Both are needed: half a
/// box is not a size, and a stated width with a guessed height distorts the
/// picture.
fn vml_box(shape: Node, column_px: f32) -> (f32, f32) {
    let style = attr_local(shape, "style").unwrap_or("");
    match (css_len(style, "width"), css_len(style, "height")) {
        (Some(w), Some(h)) => fit(w, h, column_px),
        _ => (DEFAULT_BOX_PX, DEFAULT_BOX_PX),
    }
}

/// One declaration of an inline style, in px. Only the absolute units VML
/// actually uses are read — a percentage or a keyword has no px value here, and
/// pretending otherwise sizes the box off nothing.
fn css_len(style: &str, prop: &str) -> Option<f32> {
    let raw = style
        .split(';')
        .find_map(|decl| {
            let (k, v) = decl.split_once(':')?;
            k.trim().eq_ignore_ascii_case(prop).then_some(v.trim())
        })?
        .trim();
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    let v: f32 = digits.trim().parse().ok()?;
    let px = match raw[digits.len()..].trim().to_ascii_lowercase().as_str() {
        // A bare number is a pixel count in CSS's own quirks reading, which is
        // what a VML producer that omits the unit means by it.
        "" | "px" => v,
        // Points are the unit Word writes.
        "pt" => pt_to_px(v),
        "in" => v * 96.0,
        "cm" => v * 96.0 / 2.54,
        "mm" => v * 96.0 / 25.4,
        "pc" => pt_to_px(v * 12.0),
        _ => return None,
    };
    (px.is_finite() && px > 0.0).then_some(px)
}

// ── media ────────────────────────────────────────────────────────────────────

/// The picture behind one relationship id, sized for the box it will occupy.
fn image(ctx: &mut Ctx, rid: &str, w_px: f32, h_px: f32, label: String) -> Graphic {
    // The display box decides the encoded size, so a full-resolution original is
    // downscaled before it is base64'd into the page.
    let want = w_px.max(1.0).min(MAX_BOX_PX).round() as u32;
    let m = match resolve(ctx, rid) {
        Some(part) => ctx.media.get(ctx.zip, ctx.budget, ctx.mb, &part, want),
        // Missing, external, or a target that escapes the package: the box stays,
        // because the document's layout was built around it.
        None => Media::Placeholder("image unavailable"),
    };
    match m {
        // `a:srcRect` is *not* applied — a known approximation, not an oversight.
        // A crop is an oversized image inside a box that clips it, and a clipping
        // box is a block element, which cannot sit inside a paragraph. So a
        // cropped picture shows its whole self at the cropped box's size.
        Media::DataUri(uri) => Graphic {
            src: Some(uri.as_str().to_string()),
            w_px,
            h_px,
            label,
            float: None,
        },
        // The *class* of failure is explained once in the footer, by the note
        // `MediaBudget` already owns — nothing new is invented here.
        Media::Placeholder(reason) => placeholder(w_px, h_px, label, reason),
    }
}

/// The box a graphic that cannot be drawn leaves behind, labelled with the
/// author's own description of it where there is one and the reason otherwise.
fn placeholder(w_px: f32, h_px: f32, label: String, reason: &str) -> Graphic {
    Graphic {
        src: None,
        w_px,
        h_px,
        label: if label.is_empty() {
            reason.to_string()
        } else {
            label
        },
        float: None,
    }
}

fn resolve(ctx: &Ctx, rid: &str) -> Option<String> {
    let r = ctx.rels.get(rid)?;
    if r.external {
        return None;
    }
    opc::resolve_target(ctx.part, &r.target)
}

/// Adds the graphic to the paragraph's runs, in the shape its wrap asks for.
fn push(ctx: &mut Ctx, out: &mut Vec<Run>, mut g: Graphic, wrap: Wrap) {
    if ctx.images >= MAX_GRAPHICS {
        ctx.notes.add(NOTE_COUNT);
        return;
    }
    ctx.images += 1;
    match wrap {
        Wrap::Inline => out.push(Run::Graphic(g)),
        Wrap::Float(side) => {
            g.float = Some(side);
            out.push(Run::Graphic(g));
        }
        // `wp:wrapTopAndBottom` keeps text off both sides. A break either side is
        // as close as a column without pagination gets to that.
        Wrap::OwnLine => {
            out.push(Run::Break(Break::Line));
            out.push(Run::Graphic(g));
            out.push(Run::Break(Break::Line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
         xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
         xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
         xmlns:v=\"urn:schemas-microsoft-com:vml\" \
         xmlns:o=\"urn:schemas-microsoft-com:office:office\"";

    /// The single element of a fragment, for the helpers that take a node. The
    /// document has to outlive the node, so the body is a closure.
    fn with<T>(xml: &str, f: impl FnOnce(Node) -> T) -> T {
        let src = format!("<root {NS}>{xml}</root>");
        let doc = super::super::super::xml::parse(&src).expect("fixture parses");
        let node = elems(doc.root_element()).next().expect("one element");
        f(node)
    }

    /// A US Letter column: 816px of page less two 96px margins.
    const COLUMN: f32 = 624.0;

    #[test]
    fn an_extent_becomes_px_and_keeps_its_aspect_when_clamped() {
        // 914400 EMU = 1in = 96px, so 2x1in is 192x96.
        let (w, h) = with(
            "<wp:inline><wp:extent cx=\"1828800\" cy=\"914400\"/></wp:inline>",
            |n| box_of(n, COLUMN),
        );
        assert_eq!((w, h), (192.0, 96.0));

        // Absurd: 1e11 EMU is 10.5 million px. The column caps the width and the
        // height follows it down, so a 2:1 picture stays 2:1.
        let (w, h) = with(
            "<wp:inline><wp:extent cx=\"100000000000\" cy=\"50000000000\"/></wp:inline>",
            |n| box_of(n, COLUMN),
        );
        assert_eq!(w, COLUMN);
        assert!((w / h - 2.0).abs() < 0.01, "aspect lost: {w}x{h}");

        // No extent at all, and a non-numeric one: a box the reader can see.
        let (w, h) = with("<wp:inline/>", |n| box_of(n, COLUMN));
        assert_eq!((w, h), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
        let (w, h) = with(
            "<wp:inline><wp:extent cx=\"café\" cy=\"0\"/></wp:inline>",
            |n| box_of(n, COLUMN),
        );
        assert_eq!((w, h), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
    }

    #[test]
    fn an_extent_falls_back_to_the_shapes_own() {
        let (w, h) = with(
            "<wp:anchor><a:graphic><a:graphicData><a:xfrm>\
             <a:ext cx=\"914400\" cy=\"457200\"/></a:xfrm></a:graphicData></a:graphic></wp:anchor>",
            |n| box_of(n, COLUMN),
        );
        assert_eq!((w, h), (96.0, 48.0));
    }

    #[test]
    fn a_label_prefers_the_authors_alt_text() {
        let label = |attrs: &str| {
            with(&format!("<wp:inline><wp:docPr {attrs}/></wp:inline>"), |n| {
                doc_pr_label(n)
            })
        };
        assert_eq!(label("descr=\"café Widget\" name=\"Picture 3\""), "café Widget");
        assert_eq!(label("name=\"Picture 3\""), "Picture 3");
        assert_eq!(label(""), "");
        // Untrusted and unbounded: a paragraph of alt text is not a label.
        let long = "é".repeat(MAX_LABEL_CHARS + 50);
        assert_eq!(label(&format!("descr=\"{long}\"")).chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn a_wrapped_anchor_floats_to_the_side_the_text_runs_past() {
        let wrap = |body: &str| with(&format!("<wp:anchor>{body}</wp:anchor>"), |n| Wrap::of(n, 96.0, COLUMN));
        // Text down the left of the drawing puts the drawing on the right.
        assert_eq!(
            wrap("<wp:wrapSquare wrapText=\"left\"/>"),
            Wrap::Float(Side::Right)
        );
        assert_eq!(
            wrap("<wp:wrapTight wrapText=\"right\"/>"),
            Wrap::Float(Side::Left)
        );
        // `bothSides` says nothing about the side, so the position decides.
        assert_eq!(
            wrap("<wp:wrapThrough wrapText=\"bothSides\"/>\
                  <wp:positionH relativeFrom=\"column\"><wp:align>right</wp:align></wp:positionH>"),
            Wrap::Float(Side::Right)
        );
        assert_eq!(
            wrap("<wp:wrapSquare wrapText=\"bothSides\"/>\
                  <wp:positionH relativeFrom=\"margin\"><wp:align>left</wp:align></wp:positionH>"),
            Wrap::Float(Side::Left)
        );
        // Centred: nothing to float to, so it stays in the line.
        assert_eq!(
            wrap("<wp:wrapSquare wrapText=\"bothSides\"/>\
                  <wp:positionH relativeFrom=\"column\"><wp:align>center</wp:align></wp:positionH>"),
            Wrap::Inline
        );
    }

    #[test]
    fn an_offset_anchor_floats_only_when_it_hugs_an_edge() {
        let at = |offset: &str, from: &str| {
            with(
                &format!(
                    "<wp:anchor><wp:wrapSquare wrapText=\"bothSides\"/>\
                     <wp:positionH relativeFrom=\"{from}\"><wp:posOffset>{offset}</wp:posOffset>\
                     </wp:positionH></wp:anchor>"
                ),
                |n| Wrap::of(n, 96.0, COLUMN),
            )
        };
        // 0 EMU: hard against the column's leading edge.
        assert_eq!(at("0", "column"), Wrap::Float(Side::Left));
        // 4572000 EMU = 480px, so the box's far edge is at 576 of 624.
        assert_eq!(at("4572000", "column"), Wrap::Float(Side::Right));
        // 2286000 EMU = 240px: neither edge.
        assert_eq!(at("2286000", "column"), Wrap::Inline);
        // Page-relative: the page's own margins are not in scope here, so the
        // offset says nothing about which side of the column it lands on.
        assert_eq!(at("0", "page"), Wrap::Inline);
    }

    #[test]
    fn the_other_wrap_kinds_degrade_predictably() {
        let wrap = |body: &str| with(&format!("<wp:anchor>{body}</wp:anchor>"), |n| Wrap::of(n, 96.0, COLUMN));
        assert_eq!(wrap("<wp:wrapTopAndBottom/>"), Wrap::OwnLine);
        // Behind or in front of the text: neither is a position a reflowing
        // column can honour.
        assert_eq!(wrap("<wp:wrapNone/>"), Wrap::Inline);
        assert_eq!(wrap(""), Wrap::Inline);
    }

    #[test]
    fn a_vml_shape_states_its_box_in_css_units() {
        let vml = |style: &str| {
            with(&format!("<v:shape style=\"{style}\"/>"), |n| {
                vml_box(n, COLUMN)
            })
        };
        // 120pt x 90pt = 160px x 120px: points are what Word writes.
        assert_eq!(vml("width:120pt;height:90pt"), (160.0, 120.0));
        assert_eq!(vml("width:64px;height:32px"), (64.0, 32.0));
        assert_eq!(vml("position:absolute;WIDTH:1in;height:0.5in;z-index:1"), (96.0, 48.0));
        // Unitless is a pixel count.
        assert_eq!(vml("width:40;height:20"), (40.0, 20.0));
        // Half a box, no box, or a unit with no px value: the default square.
        assert_eq!(vml("width:120pt"), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
        assert_eq!(vml(""), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
        assert_eq!(vml("width:50%;height:50%"), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
        assert_eq!(vml("width:auto;height:auto"), (DEFAULT_BOX_PX, DEFAULT_BOX_PX));
        // A stated box wider than the column is scaled down, aspect kept.
        let (w, h) = vml("width:1000pt;height:500pt");
        assert_eq!(w, COLUMN);
        assert!((w / h - 2.0).abs() < 0.01, "{w}x{h}");
    }

    #[test]
    fn image_data_finds_the_relationship_and_the_shape_that_sizes_it() {
        let (rid, shape, title) = with(
            "<w:pict><v:shape style=\"width:24pt;height:12pt\" id=\"Widget\">\
             <v:imagedata r:id=\"rId7\" o:title=\"café\"/></v:shape></w:pict>",
            |n| {
                let (rid, data, shape) = image_data(n).expect("one imagedata");
                (
                    rid.to_string(),
                    attr_local(shape, "style").unwrap_or("").to_string(),
                    attr_local(data, "title").unwrap_or("").to_string(),
                )
            },
        );
        assert_eq!(rid, "rId7");
        assert_eq!(shape, "width:24pt;height:12pt");
        assert_eq!(title, "café");

        // A shape with no image data at all is not a graphic.
        assert!(with("<w:pict><v:rect style=\"width:0;height:1.5pt\"/></w:pict>", |n| {
            image_data(n).is_none()
        }));
    }
}
