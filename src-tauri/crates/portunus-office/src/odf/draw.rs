//! `draw:frame`: the pictures a text document carries, the frames it anchors, and
//! the boxes the graphics this preview cannot draw degrade to.
//!
//! Everything here becomes a [`Graphic`] run, so a picture lands in the line it
//! belongs to and the paragraph emitter places it — the same reasoning
//! `docx::draw` documents: the column reflows to the reader's width and is not
//! paginated, so a page-relative position would put the picture on top of text
//! that has moved somewhere else.
//!
//! One frame element covers what WordprocessingML spreads over `w:drawing`,
//! `w:pict` and `w:object`: a `draw:frame` states the box and holds whatever it
//! frames — a `draw:image`, a `draw:text-box`, or a `draw:object` with its
//! pre-rendered replacement image beside it. So the children are ranked rather
//! than dispatched (see [`content`]), and a frame that frames nothing this
//! renderer draws still leaves a box at the document's own geometry.

use super::super::emit;
use super::super::media::Media;
use super::super::model::{Break, Graphic, Run, Side};
use super::super::xml::{attr_local, child, elems, inner_text};
use super::length;
use super::style::{Family, GraphicProps, HPos, WrapMode};
use super::text::{self, Ctx};
use roxmltree::Node;

/// Graphics — images *and* placeholders — per document, on top of the byte caps
/// `MediaBudget` enforces. Each one is a `data:` URI WebKit has to parse and hold,
/// and the writer's byte cap alone would let a thousand tiny images through.
const MAX_GRAPHICS: usize = 200;

/// Ceiling on either axis of a frame's box. `svg:width` is stated by an untrusted
/// document; the text column is the real limit on the width (see [`fit`]), and
/// this bounds the height, and the width when there is no column.
const MAX_BOX_PX: f32 = 4096.0;

/// Box for a frame that states no usable size anywhere. Something the reader can
/// see and scroll past, rather than a zero-height nothing.
const DEFAULT_BOX_PX: f32 = 96.0;

/// Characters of a frame's description kept. It is document text in an attribute
/// or a child element, so it is untrusted and unbounded; a paragraph of alt text
/// is not a label.
const MAX_LABEL_CHARS: usize = 200;

const NOTE_COUNT: &str = "Some images not shown";

/// A frame this column cannot place where the document does. Deliberately close to
/// the docx path's text-box wording, because it is the same limitation: there are
/// no pages to be absolute against.
const NOTE_ANCHOR: &str = "Frames placed in the text flow";

/// An embedded object — a chart, a formula, a spreadsheet — with no replacement
/// image beside it. Deliberately the same sentence shape as `docx::draw`'s chart
/// note: the reason is identical, the object is data plus an application.
const NOTE_OBJECT: &str = "Embedded objects not drawn";

/// One `draw:frame` from a paragraph or from between blocks. Pushes zero or more
/// runs — zero for a text frame (whose blocks are hoisted instead) or when the
/// graphic cap is reached, more than one when the wrap needs a line of its own.
pub fn emit_frame<'a>(ctx: &mut Ctx<'a>, out: &mut Vec<Run>, frame: Node<'a, 'a>) {
    // A text frame is block content and cannot live inside a `<p>`, so it is
    // hoisted. Checked before the picture path because a text frame may *also*
    // carry a fill or a background image, and the text is the content.
    if let Some(tb) = text_box(frame) {
        text::hoist(ctx, tb);
        return;
    }
    let gp = ctx
        .styles
        .resolve(Family::Graphic, attr_local(frame, "style-name").unwrap_or(""))
        .graphic;
    let (w_px, h_px) = box_of(frame, &gp, ctx.column_px);
    let label = frame_label(frame);
    let g = content(ctx, frame, w_px, h_px, label);
    let wrap = wrap_of(ctx, frame, &gp);
    push(ctx, out, g, wrap);
}

/// The `draw:text-box` of a text frame, if it holds any block content at all. An
/// empty one is a frame nobody typed in, and hoisting it would put an empty
/// bordered box in the middle of the page.
fn text_box<'a>(frame: Node<'a, 'a>) -> Option<Node<'a, 'a>> {
    let tb = child(frame, "text-box")?;
    elems(tb).next().map(|_| tb)
}

/// What the frame frames, in preference order: the picture, then an object's
/// pre-rendered replacement image, then a labelled box.
///
/// Ranked rather than dispatched because one frame holds several children — a
/// `draw:object` is written *with* the `draw:image` that stands in for it, and
/// either may come first — and a resolvable image is the best of them whatever its
/// position.
fn content(ctx: &mut Ctx, frame: Node, w_px: f32, h_px: f32, label: String) -> Graphic {
    let mut object = false;
    let mut linked = false;
    for c in elems(frame) {
        match c.tag_name().name() {
            "image" => match part_of(ctx, c) {
                Some(part) => return image(ctx, &part, w_px, h_px, label),
                // An href the package does not hold: a *linked* picture, whose
                // bytes live outside the document. A preview does not reach out to
                // the filesystem or the network for them.
                None => linked = true,
            },
            // An object stored as a whole sub-document, or something needing a
            // plugin. Sub-documents are never rendered recursively — see the module
            // note in `odf`.
            "object" | "object-ole" | "plugin" | "applet" | "floating-frame" => object = true,
            _ => {}
        }
    }
    if object {
        // The box says *what* it stands for; the footer says the preview will never
        // draw it, which a reader looking at a report's one chart cannot tell from
        // the box alone.
        ctx.notes.add(NOTE_OBJECT);
        return placeholder(w_px, h_px, label, "embedded object");
    }
    if linked {
        return placeholder(w_px, h_px, label, "image unavailable");
    }
    // A frame with nothing in it this renderer knows: the document's layout was
    // still built around the box, so the box stays.
    placeholder(w_px, h_px, label, emit::graphic_label(""))
}

/// The archive entry one `xlink:href` names, or `None` when it names nothing this
/// package holds. `pkg::Entries::resolve_href` is the single gate every untrusted
/// href passes through — there is no separate check a call site could forget.
fn part_of(ctx: &Ctx, n: Node) -> Option<String> {
    ctx.entries
        .resolve_href(attr_local(n, "href")?)
        .map(str::to_string)
}

/// The picture behind one package part, sized for the box it will occupy.
fn image(ctx: &mut Ctx, part: &str, w_px: f32, h_px: f32, label: String) -> Graphic {
    // The display box decides the encoded size, so a full-resolution original is
    // downscaled before it is base64'd into the page.
    let want = w_px.max(1.0).min(MAX_BOX_PX).round() as u32;
    match ctx.media.get(ctx.zip, ctx.budget, ctx.mb, part, want) {
        // `draw:image` has no crop of its own — `fo:clip` on the graphic style is
        // not applied, for the reason `docx::draw` gives about `a:srcRect`: a crop
        // is an oversized image inside a clipping box, and a clipping box is a
        // block element, which cannot sit inside a paragraph.
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

/// The frame's box in px: `svg:width` / `svg:height`, else the graphic style's
/// `fo:min-width` / `fo:min-height` for a frame sized by its own content, else a
/// default square.
///
/// Both axes or neither: half a box is not a size, and a stated width with a
/// guessed height distorts the picture.
fn box_of(frame: Node, gp: &GraphicProps, column_px: f32) -> (f32, f32) {
    let len = |name: &str| {
        attr_local(frame, name)
            .and_then(length::parse_len)
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    match (
        len("width").or(gp.min_width_px),
        len("height").or(gp.min_height_px),
    ) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => fit(w, h, column_px),
        _ => (DEFAULT_BOX_PX, DEFAULT_BOX_PX),
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

/// What the frame says it shows: `svg:desc` (the author's description), then
/// `svg:title`, then `draw:name` (the producer's own "Picture 2"). The image's
/// `alt`, and the placeholder's label when the picture cannot be shown.
fn frame_label(frame: Node) -> String {
    let mut s = String::new();
    for name in ["desc", "title"] {
        if let Some(n) = child(frame, name) {
            inner_text(n, &mut s);
            if !s.trim().is_empty() {
                break;
            }
            s.clear();
        }
    }
    if s.trim().is_empty() {
        s = attr_local(frame, "name").unwrap_or("").to_string();
    }
    s.trim().chars().take(MAX_LABEL_CHARS).collect()
}

// ── anchoring ────────────────────────────────────────────────────────────────

/// How an anchored frame joins the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrap {
    Inline,
    Float(Side),
    /// Text above and below it, none beside it.
    OwnLine,
}

/// `text:anchor-type` plus the graphic style's `style:wrap`.
///
/// A frame anchored to a paragraph or to the page is positioned absolutely, and a
/// reflowing unpaginated column cannot honour that: the text it was measured
/// against is somewhere else. A float keeps the two things that survive reflow —
/// the side the author chose, and text staying out from under the box — and
/// everything else degrades into the flow, which is the honest approximation
/// rather than a guess at a position.
fn wrap_of(ctx: &mut Ctx, frame: Node, gp: &GraphicProps) -> Wrap {
    match attr_local(frame, "anchor-type") {
        // In the line, sharing it with the text on either side. `char` is anchored
        // *at* a character rather than *as* one, which is a position this column
        // cannot place any better than the line that character is on — and it is
        // where the document put it, so it needs no note.
        None | Some("as-char") | Some("char") => Wrap::Inline,
        _ => match gp.wrap_mode {
            // No text beside it at all: a line of its own is exactly that.
            Some(WrapMode::None) => Wrap::OwnLine,
            // The wrap names the side the *text* runs down, so it names the
            // frame's side by elimination.
            Some(WrapMode::Left) => Wrap::Float(Side::Right),
            Some(WrapMode::Right) => Wrap::Float(Side::Left),
            // Text on both sides: only `style:horizontal-pos` says which side the
            // frame itself is on, and a centred or offset frame has no side a float
            // could take.
            Some(WrapMode::Parallel) | Some(WrapMode::Dynamic) => match gp.h_pos {
                Some(HPos::Left) => Wrap::Float(Side::Left),
                Some(HPos::Right) => Wrap::Float(Side::Right),
                _ => {
                    ctx.notes.add(NOTE_ANCHOR);
                    Wrap::Inline
                }
            },
            // `run-through` is behind or in front of the text, and an unstated wrap
            // leaves it to the producer's default. Both land in the flow: the
            // overlap they ask for falls on different words at every width, and a
            // watermark drawn over the middle of a paragraph is worse than one
            // sitting between two.
            _ => {
                ctx.notes.add(NOTE_ANCHOR);
                Wrap::Inline
            }
        },
    }
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
        // A break either side is as close as a column without pagination gets to
        // "no text beside it".
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
    use crate::odf::text::tests::{content, odt, odt_media, png_bytes, styles, Fixture};
    use crate::OfficeDoc;

    /// Graphic styles covering the three wraps a frame can degrade through.
    fn graphic_styles() -> String {
        "<style:style style:name=\"gr-left\" style:family=\"graphic\">\
         <style:graphic-properties style:wrap=\"right\"/></style:style>\
         <style:style style:name=\"gr-none\" style:family=\"graphic\">\
         <style:graphic-properties style:wrap=\"none\"/></style:style>\
         <style:style style:name=\"gr-run\" style:family=\"graphic\">\
         <style:graphic-properties style:wrap=\"run-through\"/></style:style>"
            .to_string()
    }

    /// One picture in the package, framed by `frame`.
    fn with_picture(tag: &str, frame: &str) -> OfficeDoc {
        odt_media(
            tag,
            &styles(&graphic_styles()),
            &content("", frame),
            &[("Pictures/1000.png", png_bytes(24, 12))],
        )
        .doc()
    }

    fn frame(attrs: &str, inner: &str) -> String {
        format!("<text:p><draw:frame {attrs}>{inner}</draw:frame></text:p>")
    }

    const IMAGE: &str = "<draw:image xlink:href=\"Pictures/1000.png\"/>";

    #[test]
    fn a_picture_is_decoded_into_the_page_at_its_own_box() {
        let doc = with_picture(
            "pic",
            &frame(
                "svg:width=\"1in\" svg:height=\"0.5in\" text:anchor-type=\"as-char\"",
                IMAGE,
            ),
        );
        let html = &doc.html;
        // Re-encoded for the box it will occupy rather than carried at full
        // resolution; opaque, so what comes back is a JPEG.
        assert!(html.contains("<img class=\"of-img\" src=\"data:image/"), "{html}");
        assert!(html.contains("width:96px;height:48px;"), "{html}");
        // Inline: it shares the line with the text around it, which is where the
        // document put it, so there is nothing to note.
        assert!(!html.contains("float"), "{html}");
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
    }

    #[test]
    fn an_href_the_package_does_not_hold_leaves_a_labelled_box() {
        for href in [
            "../../etc/passwd",
            "/Pictures/1000.png",
            "Pictures/%2e%2e/1000.png",
            "https://example.org/1000.png",
            "Pictures/absent.png",
        ] {
            let doc = with_picture(
                "reject",
                &frame(
                    "svg:width=\"1in\" svg:height=\"1in\"",
                    &format!("<draw:image xlink:href=\"{href}\"/>"),
                ),
            );
            // No `src`, and no attempt to reach the filesystem or the network for
            // the bytes: the box the document laid out is what stays.
            assert!(!doc.html.contains("data:image/"), "{href}: {}", doc.html);
            assert!(!doc.html.contains(href), "{href} must not reach an attribute");
            assert!(doc.html.contains("class=\"of-gph\""), "{href}: {}", doc.html);
            assert!(
                doc.html.contains("image unavailable"),
                "{href}: {}",
                doc.html
            );
        }
    }

    #[test]
    fn the_authors_own_description_labels_a_box_it_can() {
        let doc = with_picture(
            "label",
            &frame(
                "svg:width=\"1in\" svg:height=\"1in\" draw:name=\"Picture 2\"",
                "<svg:desc>a naïve café</svg:desc>\
                 <draw:image xlink:href=\"Pictures/absent.png\"/>",
            ),
        );
        // `svg:desc` outranks the producer's own `draw:name`.
        assert!(doc.html.contains("a naïve café"), "{}", doc.html);
        assert!(!doc.html.contains("Picture 2"), "{}", doc.html);
    }

    #[test]
    fn an_embedded_object_is_a_placeholder_and_the_footer_says_why() {
        let doc = with_picture(
            "object",
            &frame(
                "svg:width=\"2in\" svg:height=\"1in\"",
                "<draw:object xlink:href=\"./Object 1\"/>",
            ),
        );
        assert!(doc.html.contains("embedded object"), "{}", doc.html);
        assert!(doc.notes.iter().any(|n| n == NOTE_OBJECT), "{:?}", doc.notes);
        // A sub-document is never rendered recursively.
        assert!(!doc.html.contains("data:image/"), "{}", doc.html);
    }

    #[test]
    fn an_objects_replacement_image_is_drawn_when_the_package_holds_one() {
        let doc = odt_media(
            "objrepl",
            &styles(&graphic_styles()),
            &content(
                "",
                &frame(
                    "svg:width=\"1in\" svg:height=\"1in\"",
                    "<draw:object xlink:href=\"./Object 1\"/>\
                     <draw:image xlink:href=\"./ObjectReplacements/Object 1\"/>",
                ),
            ),
            &[("ObjectReplacements/Object 1", png_bytes(16, 16))],
        )
        .doc();
        // A resolvable image is the best of a frame's children whatever its
        // position, so the chart's own rendering wins over the placeholder.
        assert!(doc.html.contains("data:image/"), "{}", doc.html);
    }

    #[test]
    fn a_frame_wider_than_the_column_is_scaled_rather_than_clipped() {
        let doc = with_picture(
            "wide",
            &frame("svg:width=\"20in\" svg:height=\"10in\"", IMAGE),
        );
        // The column is 8.5in less two 1in margins = 624px, and the aspect is kept:
        // clamping one axis alone would stretch the picture.
        assert!(doc.html.contains("width:624px;height:312px;"), "{}", doc.html);
    }

    #[test]
    fn a_frame_that_states_no_size_still_leaves_a_box() {
        let doc = with_picture("nosize", &frame("", IMAGE));
        // Something the reader can see and scroll past, rather than a zero-height
        // nothing.
        let box_px = format!("width:{DEFAULT_BOX_PX}px;height:{DEFAULT_BOX_PX}px;");
        assert!(doc.html.contains(&box_px), "{}", doc.html);

        // Half a stated box is not a size: both axes or neither.
        let doc = with_picture("halfsize", &frame("svg:width=\"3in\"", IMAGE));
        assert!(doc.html.contains(&box_px), "{}", doc.html);
    }

    #[test]
    fn an_anchored_frame_keeps_the_side_it_hugs_and_notes_what_it_cannot_keep() {
        // `style:wrap="right"` says the text runs down the frame's right, so the
        // frame is on the left.
        let doc = with_picture(
            "float",
            &frame(
                "svg:width=\"1in\" svg:height=\"1in\" text:anchor-type=\"paragraph\" \
                 draw:style-name=\"gr-left\"",
                IMAGE,
            ),
        );
        assert!(doc.html.contains("float:left"), "{}", doc.html);

        // No text beside it at all: a line of its own is exactly that.
        let doc = with_picture(
            "ownline",
            &frame(
                "svg:width=\"1in\" svg:height=\"1in\" text:anchor-type=\"page\" \
                 draw:style-name=\"gr-none\"",
                IMAGE,
            ),
        );
        assert_eq!(doc.html.matches("<br>").count(), 2, "{}", doc.html);

        // Behind or in front of the text: the overlap falls on different words at
        // every width, so it lands in the flow and the footer says so.
        let doc = with_picture(
            "through",
            &frame(
                "svg:width=\"1in\" svg:height=\"1in\" text:anchor-type=\"paragraph\" \
                 draw:style-name=\"gr-run\"",
                IMAGE,
            ),
        );
        assert!(doc.notes.iter().any(|n| n == NOTE_ANCHOR), "{:?}", doc.notes);
    }

    #[test]
    fn a_text_frame_is_hoisted_after_the_paragraph_that_anchored_it() {
        let f = odt(
            "txbx",
            &styles(&graphic_styles()),
            &content(
                "",
                "<text:p>café<draw:frame svg:width=\"2in\" svg:height=\"1in\">\
                 <draw:text-box><text:p>naïve</text:p></draw:text-box>\
                 </draw:frame></text:p><text:p>Widget</text:p>",
            ),
        );
        let html = f.body_html();
        let notes = f.doc().notes;
        // Block content cannot live inside a `<p>`: an HTML parser would close the
        // paragraph before it and tear the anchor in two.
        assert!(html.contains("class=\"of-txbx\""), "{html}");
        let anchor = html.find("café").expect("the anchoring paragraph");
        let box_at = html.find("of-txbx").expect("the frame");
        let next = html.find("Widget").expect("the next paragraph");
        assert!(anchor < box_at && box_at < next, "{html}");
        assert!(notes.iter().any(|n| n == text::NOTE_TXBX), "{notes:?}");
    }

    #[test]
    fn an_empty_text_frame_is_not_hoisted_at_all() {
        let f = odt(
            "emptybox",
            &styles(&graphic_styles()),
            &content(
                "",
                "<text:p>x<draw:frame svg:width=\"2in\" svg:height=\"1in\">\
                 <draw:text-box/></draw:frame></text:p>",
            ),
        );
        // A frame nobody typed in: hoisting it would put an empty bordered box in
        // the middle of the page.
        let html = f.body_html();
        assert!(!html.contains("of-txbx"), "{html}");
        assert!(f.doc().notes.is_empty(), "{:?}", f.doc().notes);
    }

    #[test]
    fn a_frame_between_blocks_gets_a_paragraph_of_its_own() {
        let doc = odt_media(
            "blockframe",
            &styles(&graphic_styles()),
            &content(
                "",
                &format!(
                    "<text:p>text</text:p><draw:frame svg:width=\"1in\" svg:height=\"1in\" \
                     text:anchor-type=\"page\">{IMAGE}</draw:frame>"
                ),
            ),
            &[("Pictures/1000.png", png_bytes(8, 8))],
        )
        .doc();
        // A page-anchored picture has no paragraph to join, so it is still
        // somewhere in the flow rather than nowhere.
        assert!(doc.html.contains("data:image/"), "{}", doc.html);
        assert_eq!(doc.html.matches("<p ").count(), 2, "{}", doc.html);
    }

    #[test]
    fn the_graphic_count_is_capped_and_says_so() {
        let one = frame("svg:width=\"0.2in\" svg:height=\"0.2in\"", IMAGE);
        let body: String = std::iter::repeat_n(one, MAX_GRAPHICS + 5).collect();
        let f = odt_media(
            "gcap",
            &styles(&graphic_styles()),
            &content("", &body),
            &[("Pictures/1000.png", png_bytes(8, 8))],
        );
        // Each graphic is a `data:` URI WebKit has to parse and hold, so the byte
        // cap alone would let a thousand tiny ones through.
        let html = f.body_html();
        let drawn = html.matches("<img ").count() + html.matches("class=\"of-gph\"").count();
        assert_eq!(drawn, MAX_GRAPHICS, "{drawn}");
        let notes = f.doc().notes;
        assert!(notes.iter().any(|n| n == NOTE_COUNT), "{notes:?}");
    }

    #[test]
    fn a_picture_encodes_once_however_often_it_is_framed() {
        let one = frame("svg:width=\"1in\" svg:height=\"1in\"", IMAGE);
        let f: Fixture = odt_media(
            "reuse",
            &styles(&graphic_styles()),
            &content("", &format!("{one}{one}{one}")),
            &[("Pictures/1000.png", png_bytes(32, 32))],
        );
        let html = f.html();
        // Three frames, one cache entry: `MediaCache` is keyed by part and wanted
        // size, so the same picture at the same box is encoded once.
        assert_eq!(html.matches("data:image/").count(), 3, "{html}");
        let first = html
            .split("src=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("a data uri");
        assert_eq!(html.matches(first).count(), 3, "identical bytes");
    }
}
