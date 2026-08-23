//! The `w:body` walk: block dispatch, the property cascade per paragraph, and
//! the run tree inside one.
//!
//! Paragraphs are emitted one at a time rather than collected: `w:p`, `w:tbl` and
//! `w:sdt` interleave freely in a body, and buffering paragraphs to emit them in
//! batches would need a second structure just to remember where the tables went.
//! One paragraph through [`model::emit_paras`] with a one-element slice keeps
//! document order for free.
//!
//! Two things in here are stateful and order-dependent, and both are the reason
//! the walk is a single forward pass: [`Numbering::label`] advances list counters,
//! and `w:contextualSpacing` compares a paragraph with the one before it.

use super::super::drawingml::color::Color;
use super::super::drawingml::theme::{parse_hex_rgb, SchemeSlot};
use super::super::fonts;
use super::super::html::{attr, Writer};
use super::super::link;
use super::super::media;
use super::super::model::{self, Break, HtmlStyle, ListMarker, Run};
use super::super::xml::{self, attr_local, child, elems};
use super::numbering::{Indent, Marker as NumMarker, Suffix};
use super::style::{self, ParaProps, RunProps};
use super::{draw, notes, table, Ctx};
use roxmltree::Node;

/// The docx spelling of the paragraph model. A page column has real page breaks,
/// so unlike a slide it has a class for them.
const HTML: HtmlStyle = HtmlStyle {
    para_class: "of-p",
    list_class: "of-p of-li",
    marker_class: "of-bu",
    text_class: "of-tx",
    break_class: "of-pb",
    img_class: "of-img",
    graphic_class: "of-gph",
    scalable: false,
};

/// Paragraphs drawn per document. The writer's byte cap is the real backstop;
/// this bounds the walk itself, so a generated file cannot make the *cascade* the
/// expensive part of a preview that will be cut off anyway.
const MAX_PARAS: usize = 20_000;

/// Runs per paragraph.
const MAX_RUNS: usize = 4_000;

/// Nesting of the wrappers that hold real content — `w:sdt` at block level,
/// `w:hyperlink`/`w:ins`/`w:sdt` inside a paragraph. Real documents nest two or
/// three deep; a generated one can nest without bound.
const MAX_DEPTH: usize = 16;

/// Text boxes hoisted out of the text per document. Each one is a bordered block
/// the reader has to scroll past, and a generated file can anchor thousands to a
/// single paragraph.
const MAX_BOXES: usize = 64;

/// Nesting of hoisted boxes. A text box inside a text box happens (a callout with
/// a caption); deeper than this is a document generating itself.
const MAX_BOX_DEPTH: usize = 3;

/// Anchored bookmark ids per document.
const MAX_BOOKMARKS: usize = 2_000;

/// Byte cap for one hoisted box. Its content is capped by the *document's* writer
/// when the block is spliced in, but the box renders into a buffer of its own
/// first, so that buffer needs a bound that does not depend on the outer one.
const BOX_CAP: usize = 256 * 1024;

pub const NOTE_PARAS: &str = "Long document — first part only";
pub const NOTE_BOXES: &str = "Some text boxes not shown";
pub const NOTE_TXBX: &str = "Text boxes are shown in the text flow, after the paragraph they \
are attached to, rather than at the position the document places them.";

// ── block level ──────────────────────────────────────────────────────────────

/// Walks the children of `w:body` (or of a `w:sdtContent` or a `w:tc` standing in
/// for part of it) and emits each block in document order.
///
/// Two independent depths, because the two things they bound nest independently:
/// `depth` counts the *wrappers* around this content (`w:sdt`), and `tables`
/// counts the tables enclosing it. A body walk starts at `(0, 0)`; a cell's
/// content starts a fresh wrapper count one table deeper.
pub fn walk(ctx: &mut Ctx, w: &mut Writer, parent: Node, depth: usize, tables: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for n in elems(parent) {
        if w.is_full() {
            break;
        }
        match n.tag_name().name() {
            "p" => emit_para(ctx, w, n),
            "tbl" => table::emit_table(ctx, w, n, tables),
            // A structured document tag is a wrapper around ordinary content —
            // a date picker, a content control, a citation — so dropping it
            // loses real text.
            "sdt" => {
                if let Some(c) = child(n, "sdtContent") {
                    walk(ctx, w, c, depth + 1, tables);
                }
            }
            // A choice of markup around whole blocks — the paragraph- and
            // run-level cases are handled where they occur. `prefer_raster_branch`
            // ranks by picture and otherwise takes the `mc:Fallback`, which is the
            // branch markup compatibility says a consumer of none of the
            // `Requires` namespaces must take; at block level that is the branch
            // holding the paragraphs.
            "AlternateContent" => {
                if let Some(b) = media::prefer_raster_branch(n) {
                    walk(ctx, w, b, depth + 1, tables);
                }
            }
            // Page geometry comes from the document-level `w:sectPr` before the
            // walk starts; here it is not content.
            "sectPr" => {}
            // A bookmark between blocks: an id for a link to land on, and nothing
            // else. A `span` rather than nothing at all because a table of
            // contents entry points at exactly these.
            "bookmarkStart" => {
                if let Some(id) = bookmark(ctx, n) {
                    w.open("span", &attr("id", &id));
                    w.close();
                }
            }
            // Zero-width markers around content rather than content: the end of a
            // bookmark (its start carries the name), comment anchors, spell-check
            // spans, editing permissions.
            "bookmarkEnd" | "commentRangeStart" | "commentRangeEnd" | "proofErr"
            | "permStart" | "permEnd" => {}
            _ => {}
        }
    }
}

fn emit_para(ctx: &mut Ctx, w: &mut Writer, p: Node) {
    if ctx.paras >= MAX_PARAS {
        ctx.notes.add(NOTE_PARAS);
        return;
    }
    ctx.paras += 1;

    let ppr = child(p, "pPr");
    let direct = ppr
        .map(|n| style::parse_para_props(n, ctx.theme))
        .unwrap_or_default();
    let resolved = ctx.styles.resolve(direct.p_style.as_deref());

    // The order `style.rs` documents: docDefaults plus the `w:basedOn` chain,
    // then the numbering level's own `w:pPr`, then direct formatting. The level
    // is identified from the two together, because either link may name it.
    let mut pp = resolved.para.clone();
    let num_id = direct.num_id.or(pp.num_id).filter(|v| *v > 0);
    let ilvl = direct.ilvl.or(pp.ilvl).unwrap_or(0).clamp(0, 8) as usize;
    if let Some(lvl) = num_id.and_then(|id| ctx.numbering.level(id as u32, ilvl)) {
        style::merge_para(&mut pp, &level_props(&lvl.indent));
    }
    style::merge_para(&mut pp, &direct);

    // The paragraph *mark's* run properties over the style chain's: this is what
    // sizes an empty line and what every run's own size is compared against.
    let mut mark = resolved.run.clone();
    style::merge_run(&mut mark, &pp.mark);
    let base_pt = style::size_pt(&mark);

    let mut para = style::to_para(&pp, base_pt, style::heading_of(&pp, resolved.heading));

    // `w:contextualSpacing` drops the space between neighbours of one style —
    // both sides in Word. Only the leading side can be honoured here: the
    // predecessor is already written to the buffer, so its `space_after_px`
    // cannot be taken back. The visible difference is one gap's worth of space
    // at the seam, never a lost paragraph.
    if pp.contextual_spacing == Some(true) && ctx.prev_style.as_ref() == Some(&pp.p_style) {
        para.space_before_px = 0.0;
    }

    // Exactly one `label` call per drawn list paragraph, in document order: the
    // counters live in `Numbering`, so a second call double-advances the list and
    // a skipped one renumbers everything after it.
    para.marker = num_id
        .and_then(|id| ctx.numbering.label(id as u32, ilvl))
        .map(list_marker);

    let mut runs: Vec<Run> = Vec::new();
    collect_runs(ctx, &mut runs, p, &resolved.run, base_pt, 0, None);
    // A `w:sectPr` inside this paragraph's `w:pPr` is a section break: the
    // section ends *with* this paragraph, which is a page boundary.
    if ppr.and_then(|n| child(n, "sectPr")).is_some() && runs.len() < MAX_RUNS {
        runs.push(Run::Break(Break::Page));
    }
    para.runs = runs;

    // One paragraph, one call: document order with the other block kinds needs no
    // buffering because nothing downstream looks at more than one paragraph.
    model::emit_paras(
        w,
        std::slice::from_ref(&para),
        &HTML,
        ctx.marker,
        ctx.terms,
    );
    // Blocks the runs could not hold — a text box's own paragraphs — land here,
    // immediately after the paragraph they were anchored to and before the next
    // one, which is the closest a single forward pass gets to Word's placement.
    // Already-escaped markup from a writer of its own; see [`hoist`].
    for html in std::mem::take(&mut ctx.pending) {
        w.raw(&html);
    }
    ctx.prev_style = Some(pp.p_style);
}

/// The id of one `w:bookmarkStart`. Bounded per document: a bookmark is a target
/// for a link, so a file with more of them than a reader has links to follow is
/// spending the byte cap on ids.
fn bookmark(ctx: &mut Ctx, n: Node) -> Option<String> {
    if ctx.bookmarks >= MAX_BOOKMARKS {
        return None;
    }
    let id = link::bookmark_id(attr_local(n, "name")?)?;
    ctx.bookmarks += 1;
    Some(id)
}

/// Renders a text box's block content into a buffer of its own, for
/// [`emit_para`] to flush after the paragraph that anchored it.
///
/// Hoisted rather than emitted in place because `w:txbxContent` holds paragraphs
/// and tables, and a block element inside a `<p>` is closed *before* the
/// paragraph by every HTML parser — the box would tear the anchor in two. The
/// position Word gives the box is lost either way (it is absolute against a page
/// this column does not paginate), which is what [`NOTE_TXBX`] says out loud.
pub fn hoist(ctx: &mut Ctx, content: Node) {
    // Both bounds drop real text, so both say so: the count one because the
    // document has more boxes than a reader scrolls past, the depth one because a
    // box inside a box inside a box is a document generating itself.
    if ctx.boxes >= MAX_BOXES || ctx.box_depth >= MAX_BOX_DEPTH {
        ctx.notes.add(NOTE_BOXES);
        return;
    }
    ctx.boxes += 1;
    ctx.box_depth += 1;
    ctx.notes.add(NOTE_TXBX);
    // The pending list belongs to the paragraph being built *outside* this box; a
    // box nested in here flushes into this buffer, so the outer list must not be
    // reachable while it renders.
    let outer = std::mem::take(&mut ctx.pending);
    let mut inner = Writer::new(BOX_CAP);
    inner.open("div", &attr("class", "of-txbx"));
    // A fresh wrapper *and* table count: the box is a new block context, and the
    // tables enclosing its anchor do not enclose it in the output. What bounds a
    // table-in-a-box-in-a-cell chain is [`MAX_BOX_DEPTH`], since every restart
    // costs one level of it.
    walk(ctx, &mut inner, content, 0, 0);
    inner.close();
    // Normally empty — every paragraph flushes what it anchored — but a box
    // anchored to a paragraph the para cap refused still has to go somewhere, and
    // after this box is the only place left.
    let stranded = std::mem::replace(&mut ctx.pending, outer);
    ctx.pending.push(inner.finish());
    ctx.pending.extend(stranded);
    ctx.box_depth -= 1;
}

/// The numbering level's indent as paragraph properties, so it merges through the
/// same path as every other link of the cascade. `w:hanging` is the negative
/// direction of `first_line_dxa` and wins over `w:firstLine` when a level states
/// both, exactly as in `style::parse_para_props`.
fn level_props(ind: &Indent) -> ParaProps {
    ParaProps {
        ind_start_dxa: ind.left,
        first_line_dxa: ind.hanging.map(|v| -v).or(ind.first_line),
        ..Default::default()
    }
}

/// A numbering marker as the model's own. The label is already substituted and
/// remapped by [`super::numbering`]; what is left is the suffix and the raw
/// formatting values.
fn list_marker(m: NumMarker) -> ListMarker {
    let mut label = m.text;
    match m.suffix {
        // A non-breaking space, because the page is `white-space:pre-wrap` and an
        // ordinary trailing space would still be a wrap opportunity between the
        // marker and its first word.
        Suffix::Space => label.push('\u{00a0}'),
        // A tab needs nothing: the marker span is sized to the hanging indent
        // (see `model::emit_marker`), so the gap it would advance across is
        // already the width of the box the marker sits in.
        Suffix::Tab => {}
        Suffix::Nothing => {}
    }
    ListMarker {
        label,
        // `auto` is a statement — the marker takes the reader's text colour — and
        // not a colour, so it stays `None` rather than resolving to black.
        color: m
            .fmt
            .color
            .as_deref()
            .filter(|c| !c.eq_ignore_ascii_case("auto"))
            .and_then(parse_hex_rgb)
            .map(Color::from_rgb),
        // A symbol bullet font is only useful for a glyph the remap could not
        // resolve; the remapped glyph reads better in the text font.
        font: m
            .fmt
            .font
            .as_deref()
            .filter(|f| !fonts::is_symbol_font(f))
            .map(fonts::css_font_stack),
        size_pt: m.fmt.half_points.map(|v| v as f32 / 2.0),
    }
}

// ── runs ─────────────────────────────────────────────────────────────────────

/// Collects the runs of one paragraph, descending through the elements that wrap
/// runs without being one.
///
/// `link` is the destination of the `w:hyperlink` this content sits inside, or
/// `None` outside one. It is threaded rather than merged into [`RunProps`]
/// because it is a property of the *wrapper*, not of the run's own `w:rPr`, and a
/// nested hyperlink replaces it outright instead of inheriting.
fn collect_runs(
    ctx: &mut Ctx,
    out: &mut Vec<Run>,
    parent: Node,
    base: &RunProps,
    base_pt: f32,
    depth: usize,
    link: Option<&str>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for c in elems(parent) {
        if out.len() >= MAX_RUNS {
            return;
        }
        match c.tag_name().name() {
            "r" => emit_run(ctx, out, c, base, base_pt, link),
            // Tracked deletions and the source half of a tracked move are text
            // the author took out. A preview that shows them is showing a
            // document that does not exist.
            "del" | "moveFrom" => {}
            // A link around runs. An unresolvable or non-whitelisted destination
            // gives `None`, which renders the text without an `<a>` — never the
            // enclosing link's href, or a rejected target would inherit its way
            // back into the output.
            "hyperlink" => {
                let href = link::href_of(ctx.rels, c);
                collect_runs(ctx, out, c, base, base_pt, depth + 1, href.as_deref());
            }
            // All of these wrap real runs: an insertion, the destination of a
            // move, a legacy smart tag, a field with its result inline.
            "ins" | "moveTo" | "smartTag" | "fldSimple" => {
                collect_runs(ctx, out, c, base, base_pt, depth + 1, link)
            }
            "sdt" => {
                if let Some(cc) = child(c, "sdtContent") {
                    collect_runs(ctx, out, cc, base, base_pt, depth + 1, link);
                }
            }
            // A bookmark inside the paragraph: a position in the text, so it is a
            // run rather than a block of its own.
            "bookmarkStart" => {
                if let Some(id) = bookmark(ctx, c) {
                    out.push(Run::Anchor(id));
                }
            }
            // A choice of markup written around whole runs rather than inside
            // one — a drawing-bearing run with a legacy fallback is the common
            // case. `prefer_raster_branch` picks the branch with a picture in it
            // and otherwise the `mc:Fallback`, which is the branch markup
            // compatibility says a consumer of none of the `Requires` namespaces
            // must take.
            "AlternateContent" => {
                if let Some(b) = media::prefer_raster_branch(c) {
                    collect_runs(ctx, out, b, base, base_pt, depth + 1, link);
                }
            }
            _ => {}
        }
    }
}

fn emit_run(
    ctx: &mut Ctx,
    out: &mut Vec<Run>,
    r: Node,
    base: &RunProps,
    base_pt: f32,
    link: Option<&str>,
) {
    let direct = child(r, "rPr").map(|n| style::parse_run_props(n, ctx.theme));
    let mut rp = base.clone();
    // The `w:rStyle` chain sits between the paragraph's accumulated run
    // properties and the run's own `w:rPr` — including a character style the
    // paragraph style itself named, which is why the inherited id is a fallback
    // rather than being ignored.
    let style_id = direct
        .as_ref()
        .and_then(|d| d.r_style.clone())
        .or_else(|| rp.r_style.clone());
    if let Some(id) = style_id {
        let chain = ctx.styles.resolve_char(&id);
        style::merge_run(&mut rp, &chain);
    }
    if let Some(d) = direct.as_ref() {
        style::merge_run(&mut rp, d);
    }
    // Hidden text is dropped here rather than in `style::to_text_run`, so that it
    // never reaches the search-term highlighter either: a preview that scrolls to
    // a match inside `w:vanish` text would jump to nothing.
    if rp.vanish == Some(true) {
        return;
    }

    for c in elems(r) {
        if out.len() >= MAX_RUNS {
            return;
        }
        match c.tag_name().name() {
            // Verbatim, including whitespace: `xml:space="preserve"` needs no
            // handling because roxmltree hands back the raw text either way, and
            // the page is `white-space:pre-wrap`, so collapsing here would lose
            // indentation the author typed.
            "t" => {
                let mut s = String::new();
                xml::inner_text(c, &mut s);
                if !s.is_empty() {
                    out.push(text_run(ctx, s, &rp, base_pt, link));
                }
            }
            "tab" => out.push(Run::Tab),
            "br" => out.push(Run::Break(match attr_local(c, "type") {
                Some("page") => Break::Page,
                Some("column") => Break::Column,
                _ => Break::Line,
            })),
            "cr" => out.push(Run::Break(Break::Line)),
            "noBreakHyphen" => out.push(text_run(ctx, "\u{2011}".to_string(), &rp, base_pt, link)),
            "softHyphen" => out.push(text_run(ctx, "\u{00ad}".to_string(), &rp, base_pt, link)),
            // A drawing is a run: `draw` turns it into a `Run::Graphic` (or, for
            // a wrap that needs one, a line of its own) and the paragraph
            // emitter places it in the flow. `mc:AlternateContent` is here
            // because at run level it is essentially always wrapping one of the
            // three — a `wps` shape with a VML fallback, or a metafile with a
            // raster one.
            "drawing" | "pict" | "object" | "AlternateContent" => {
                draw::emit_drawing(ctx, out, c)
            }
            // A field *code* is not document text. Its cached result sits in
            // ordinary `w:t` runs between the field separator and end, and comes
            // through on its own; `w:delText` is a deletion's text, which the
            // run walk above already refuses to reach.
            "instrText" | "delText" | "delInstrText" => {}
            // A code point in a symbol font, which is not the code point it looks
            // like: see [`sym_run`].
            "sym" => {
                if let Some(run) = sym_run(ctx, c, &rp, base_pt, link) {
                    out.push(run);
                }
            }
            // The marker in the text; `notes::emit_tail` writes the note itself at
            // the end of the column.
            "footnoteReference" | "endnoteReference" => {
                notes::reference(ctx, out, c, &rp, base_pt)
            }
            // The number a note shows in front of its own text. Not emitted here:
            // `notes::emit_tail` draws it as the link back to the reference,
            // because a note written without this element still needs that link.
            "footnoteRef" | "endnoteRef" => {}
            // polish pass, deliberately left: a comment is an annotation *about*
            // the document rather than part of it — Word does not print one either,
            // and `comments.xml` would need its own margin column to be readable.
            "commentReference" => {}
            _ => {}
        }
    }
}

/// One span of text with its resolved properties.
///
/// A run whose face is the document's default states no `font-family`, the same
/// way the model already elides a size that matches the paragraph's. `.of-page`
/// carries that face, so the computed style is identical — and every run in a
/// document *does* resolve to it (docDefaults' `w:rFonts` reaches all of them),
/// so without this the page is one `font-family` span per run and the byte cap
/// arrives several times sooner.
pub(super) fn text_run(
    ctx: &Ctx,
    text: String,
    rp: &RunProps,
    base_pt: f32,
    link: Option<&str>,
) -> Run {
    let mut t = style::to_text_run(text, rp, base_pt);
    if t.font.is_some() && t.font.as_deref() == ctx.default_font {
        t.font = None;
    }
    if let Some(href) = link {
        // What Word's `Hyperlink` character style paints — an underline and the
        // theme's `hlink` slot — applied here because plenty of documents link
        // without naming that style. A run that states a colour of its own keeps
        // it: the author overrode the link's appearance on purpose. The underline
        // is not conditional, because it is the only cue left in a document whose
        // link colour *is* the text colour.
        t.underline = true;
        if t.color.is_none() {
            t.color = Some(Color::from_rgb(ctx.theme.color(SchemeSlot::Hlink)));
        }
        t.link = Some(href.to_string());
    }
    Run::Text(t)
}

/// A `w:sym`: `@w:char` is a code point *in the symbol font*, not in Unicode —
/// almost always U+F0xx, the private-use block Wingdings and Symbol keep their
/// glyphs in. A substituted face has nothing there, so the glyph is remapped onto
/// the real character it draws where the tables know it, and where they do not the
/// raw code point is kept with the symbol font's own stack: a system Wingdings can
/// then still draw it, and a reader without one sees a missing glyph rather than
/// the missing *text* this used to be.
fn sym_run(ctx: &Ctx, n: Node, rp: &RunProps, base_pt: f32, link: Option<&str>) -> Option<Run> {
    let font = attr_local(n, "font").unwrap_or("").trim();
    let code = u32::from_str_radix(attr_local(n, "char")?.trim(), 16).ok()?;
    let raw = char::from_u32(code)?;
    let mapped = fonts::remap(font, raw);
    let mut run = text_run(ctx, mapped.unwrap_or(raw).to_string(), rp, base_pt, link);
    if let Run::Text(t) = &mut run {
        match mapped {
            // A remapped glyph reads better in the paragraph's own face, and the
            // run's face is the symbol one wherever the document was consistent
            // about it — same reasoning as a list marker's, see [`list_marker`].
            Some(_) if is_symbol(rp) => t.font = None,
            Some(_) => {}
            // Unmapped: the code point means this glyph only in the font that
            // stated it, so that font has to travel with it — and by name, which
            // `css_font_stack` would substitute away.
            None if !font.is_empty() => t.font = Some(fonts::literal_font_stack(font)),
            None => {}
        }
    }
    Some(run)
}

fn is_symbol(rp: &RunProps) -> bool {
    rp.font_raw
        .as_deref()
        .map(fonts::is_symbol_font)
        .unwrap_or(false)
}

