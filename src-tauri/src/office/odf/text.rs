//! `office:text` → styled HTML, one continuous page column.
//!
//! The docx renderer is the model, deliberately: both dialects land on
//! `docshape`'s `.of-page` contract, both flow one unpaginated column, and both
//! emit through [`model::emit_paras`]. What differs is the source vocabulary, and
//! three of those differences shape this file:
//!
//! - **Text is text nodes.** There is no `w:t` wrapper: a `text:p` mixes character
//!   data and elements, so the run walk iterates *children* rather than elements
//!   and pushes a run for each text node it meets.
//! - **Nesting is the list level.** `text:list` announces a level by enclosing
//!   one, so the walk drives [`Lists`] with `enter`/`item`/`leave` instead of
//!   naming a level per paragraph. The one-`item`-per-drawn-item invariant is the
//!   whole of the numbering contract — see [`emit_list`].
//! - **A footnote's text is inline.** `text:note` carries its own body where the
//!   reference stands, so the bodies are collected during the walk and the tail
//!   block is written from what the walk found (`docx::notes` reads a separate
//!   part instead).
//!
//! Approximations, all of them notes in the footer rather than silence: headers
//! and footers are not drawn (a header belongs to a page), a multi-column section
//! is one column, a text frame lands after the paragraph that anchors it, and an
//! anchored frame keeps only the side it hugs.

use std::collections::HashSet;

use super::super::docshape::{self, MIN_COLUMN_PX};
use super::super::emit::{self, Notes};
use super::super::highlight::{Marker as Highlight, Terms};
use super::super::html::{attr, Writer};
use super::super::listnum;
use super::super::media::{MediaBudget, MediaCache};
use super::super::model::{self, Break, HtmlStyle, ListMarker, Run, Script};
use super::super::pkg::{Budget, Zip};
use super::super::xml::{self, attr_local, attr_u32, child, elems};
use super::super::{link, OfficeDoc, Shape};
use super::list::{FollowedBy, LevelIndent, ListStart, Lists, Marker as ListMark};
use super::pkg::{Entries, Package};
use super::style::{self, BreakKind, Family, Styles, TextProps};
use super::{draw, table};
use roxmltree::Node;

/// The class names one shape's stylesheet publishes, so the block walk can serve
/// both of them.
///
/// The walk is the same for a page and for a slide — ODF spells a paragraph, a
/// list and a table one way wherever they sit — but the stylesheets are not the
/// same file, and each publishes its own names to `srcdoc.ts` and to the frame's
/// selection engine. Passing the names in keeps one walk; hardcoding them would
/// have meant two.
pub struct Classes {
    pub html: HtmlStyle,
    pub table: &'static str,
    /// A table with no column widths to lay out against.
    pub table_auto: &'static str,
    pub cell: &'static str,
}

/// The doc-shape names, i.e. `docshape::BASE_CSS`'s.
pub const DOC: Classes = Classes {
    html: HTML,
    table: "of-tbl",
    table_auto: "of-tbl of-tbl-auto",
    cell: "of-tc",
};

/// The paragraph model as this renderer spells it — the same class names the docx
/// path uses, because they are `docshape::BASE_CSS`'s and that stylesheet is one
/// stylesheet however many dialects render into it.
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

/// Byte cap for the emitted body HTML — a text document is one long column, so it
/// matches docx's rather than a slide's.
pub const HTML_CAP: usize = 8 * 1024 * 1024;

/// The paragraph style a document's text falls back to. Not a guess: every ODF
/// producer writes it, `Styles::resolve` degrades to the family default when a
/// package somehow does not, and using it as the page's base is what lets a run
/// that resolves to the common face leave `font-family` unsaid.
const BASE_STYLE: &str = "Standard";

/// Tab width when no style states `style:tab-stop-distance`: half an inch, the
/// same fallback the docx path takes from `w:defaultTabStop`.
const DEFAULT_TAB_PX: f32 = 48.0;
const MAX_TAB_PX: f32 = 384.0;

/// Paragraphs drawn per document. The writer's byte cap is the real backstop; this
/// bounds the *walk*, so a generated file cannot make the style cascade the
/// expensive part of a preview that is going to be cut off anyway.
const MAX_PARAS: usize = 20_000;

/// Runs per paragraph.
const MAX_RUNS: usize = 4_000;

/// Nesting of the elements that wrap content without being it — `text:section`
/// and an index body at block level, `text:span` / `text:a` / a field inside a
/// paragraph. Real documents nest three or four deep.
const MAX_DEPTH: usize = 16;

/// Text frames hoisted out of the text per document, and how deep one may sit
/// inside another.
const MAX_BOXES: usize = 64;
const MAX_BOX_DEPTH: usize = 3;

/// Byte cap for one hoisted frame. Its content is capped by the document's writer
/// when the block is spliced in, but it renders into a buffer of its own first.
const BOX_CAP: usize = 256 * 1024;

/// Anchored bookmark ids per document.
const MAX_BOOKMARKS: usize = 2_000;

/// Spaces one `text:s` may stand for. The attribute is document-controlled and a
/// line of a million spaces is not indentation.
const MAX_SPACES: usize = 200;

/// Notes shown in the tail block, and the bytes it may take of the writer's
/// budget: the notes arrive after all the text the reader asked for, so they may
/// not be what exhausts the cap.
const MAX_NOTES_SHOWN: usize = 64;
const MAX_NOTE_TAIL_BYTES: usize = 128 * 1024;

/// Characters of a `text:note-citation` kept. A citation is `1`, `iv` or `*`;
/// anything longer is not a citation.
const MAX_CITATION_CHARS: usize = 16;

/// Paragraphs examined for a `style:master-page-name` before the document's first
/// master page is taken instead. The page a column shows is the one it *starts*
/// on, so the answer is always near the top of the body.
const MAX_MASTER_SCAN: usize = 512;

/// Section styles remembered for the multi-column note.
const MAX_SECTION_STYLES: usize = 1024;

const NOTE_PARAS: &str = "Long document — first part only";
const NOTE_BOXES: &str = "Some text boxes not shown";
pub const NOTE_TXBX: &str = "Frames placed in the text flow";
pub const NOTE_HEADER: &str = "Headers and footers not shown";
pub const NOTE_COLUMNS: &str = "Multi-column section shown as one column";
const NOTE_NOTES_CAPPED: &str = "Some footnotes not shown";
pub const NOTE_BULLET_IMAGE: &str = "Image bullets shown as plain bullets";
const NOTE_BODY: &str = "Document body unreadable";

/// Everything the body walk needs, grouped so the emitters can take disjoint
/// mutable borrows of the pieces they touch.
///
/// `'a` is the lifetime of *both* the package and the parsed `content.xml`: a
/// frame reads media through `zip` while the tree is alive, and a `text:note`
/// keeps its body node until the tail block is written. That is only sound
/// because `pkg::Package` was destructured on arrival — see its own note.
pub struct Ctx<'a> {
    /// The package and the media path through it: a frame resolves an
    /// `xlink:href` against `entries`, reads the part, and encodes it into a
    /// `data:` URI.
    pub zip: &'a mut Zip,
    pub budget: &'a mut Budget,
    pub media: &'a mut MediaCache,
    pub mb: &'a mut MediaBudget,
    /// The archive listing every `xlink:href` is validated against. The one gate:
    /// see `pkg::Entries::resolve_href`.
    pub entries: &'a Entries,
    /// Width of the text column in px — the widest box a frame can occupy before
    /// it would push the page open.
    pub column_px: f32,
    /// Graphics emitted so far, images and placeholders alike.
    pub images: usize,
    /// The stylesheet's own class names: `DOC` for a page column, the slide
    /// renderer's own for a canvas.
    pub classes: &'a Classes,
    pub styles: &'a Styles,
    /// Mutable because list counters advance as the walk goes: one [`Lists`]
    /// serves exactly one forward pass over the body.
    pub lists: &'a mut Lists,
    /// The CSS font stack `.of-page` states, so a run that resolves to the same
    /// face can leave it unsaid.
    pub default_font: Option<&'a str>,
    /// Section styles that state more than one column, for the honest note.
    pub multi_col: HashSet<String>,
    pub terms: &'a Terms,
    pub marker: &'a mut Highlight,
    pub notes: &'a mut Notes,
    /// Finished HTML for blocks that cannot live inside the paragraph currently
    /// being built (a text frame's own paragraphs), flushed straight after it.
    /// Each string comes from a [`Writer`] of its own, i.e. is already escaped.
    pub pending: Vec<String>,
    pub boxes: usize,
    pub box_depth: usize,
    pub bookmarks: usize,
    pub paras: usize,
    /// Table cells planned so far, across every table in the document.
    pub cells: usize,
    /// The notes referenced so far, in first-reference order: what the tail block
    /// holds, and the number each marker shows.
    pub note_refs: Vec<NoteRef<'a>>,
    /// True while the tail block is being written, so a note inside a note is
    /// dropped rather than followed.
    pub in_note: bool,
    /// `text:style-name` of the paragraph immediately before this one, for
    /// `style:contextual-spacing`. `None` is "nothing precedes it" — the start of
    /// the body, or of a table cell — which is distinct from a paragraph that names
    /// no style and therefore takes the document's default (the empty string).
    pub prev_style: Option<String>,
}

/// One referenced `text:note`: its body, and the number the marker shows. The two
/// classes are numbered separately, as Writer numbers them.
pub struct NoteRef<'a> {
    endnote: bool,
    label: String,
    body: Node<'a, 'a>,
    num: usize,
}

impl NoteRef<'_> {
    fn prefix(&self) -> &'static str {
        if self.endnote {
            "of-en-"
        } else {
            "of-fn-"
        }
    }

    /// Id of the note's block in the tail.
    fn note_anchor(&self) -> String {
        format!("{}{}", self.prefix(), self.num)
    }

    /// Id of the marker in the text, which the note links back to.
    fn ref_anchor(&self) -> String {
        format!("{}ref-{}", self.prefix(), self.num)
    }
}

/// What a list contributes to one paragraph: the label plus its indentation for
/// the item's first paragraph, the indentation alone for the ones after it and for
/// a `text:list-header`.
enum ListPart {
    Marker(ListMark),
    Indent(LevelIndent),
}

pub fn render(
    package: Package,
    notes: Notes,
    _section: Option<u32>,
    terms: &[String],
) -> Result<OfficeDoc, String> {
    render_with(package, notes, terms, HTML_CAP, MediaBudget::new())
}

/// The byte cap and the image budget are parameters only so tests can reach the
/// refusal paths without generating megabytes; [`render`] passes the real ones.
///
/// `_section` is accepted and ignored: a text document renders whole, so there is
/// one section and `sections` is empty — the frontend's section strip no-ops below
/// two entries, and inventing "Page 1…n" would be a pagination this renderer does
/// not do.
fn render_with(
    package: Package,
    mut notes: Notes,
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
    let body = child(root, "body").and_then(|b| child(b, "text"));

    // The page the column shows is the one the document starts on, which is what
    // the first paragraph style naming a master page says.
    let setup = styles.page_setup(body.and_then(|b| first_master(&styles, b)).as_deref());
    if setup.has_header || setup.has_footer {
        notes.add(NOTE_HEADER);
    }
    let page = setup.page;

    let base = styles.resolve(Family::Paragraph, BASE_STYLE);
    let base_pt = style::size_pt(&base.text, style::DEFAULT_SIZE_PT);
    // Tabs are literal U+0009 under `white-space:pre-wrap`, so the default stop is
    // the only tab measurement CSS can express — the same limit the docx path has.
    let tab_px = base
        .para
        .tab_stop_distance_px
        .filter(|v| *v > 0.0 && v.is_finite())
        .map(|v| v.min(MAX_TAB_PX))
        .unwrap_or(DEFAULT_TAB_PX);

    let query = Terms::new(terms);
    let mut hl = Highlight::new();
    let mut media = MediaCache::new();
    let mut w = Writer::new(html_cap);
    let column_px = (page.width - page.left - page.right).max(MIN_COLUMN_PX);

    // The frontend's doc variant keys its selection and its page geometry off
    // `.of-page`; see the contract in `srcdoc.ts`.
    w.open("div", &attr("class", "of-page"));
    match body {
        Some(b) => {
            let mut ctx = Ctx {
                zip: &mut zip,
                budget: &mut budget,
                media: &mut media,
                mb: &mut mb,
                entries: &entries,
                classes: &DOC,
                column_px,
                images: 0,
                styles: &styles,
                lists: &mut lists,
                default_font: base.text.font.as_deref(),
                multi_col: multi_column_sections(root),
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
            walk(&mut ctx, &mut w, b, 0, 0);
            // Inside `.of-page`: the notes belong to the column, at the end of it,
            // and the walk is over so every reference is in.
            emit_note_tail(&mut ctx, &mut w);
        }
        None => notes.add(NOTE_BODY),
    }
    w.close();
    // The image path explains its own degradations, one line per class of failure
    // however many pictures hit it.
    for n in mb.notes() {
        notes.add(n);
    }

    let truncated = w.truncated();
    let page_css = docshape::page_css(&page, base.text.font.as_deref(), base_pt, tab_px);
    let html = emit::wrap_style(docshape::BASE_CSS, &page_css, w.finish());

    Ok(OfficeDoc {
        html,
        shape: Shape::Doc,
        sections: Vec::new(),
        section: 0,
        // A page column has no fixed canvas; its geometry is the page box.
        natural: None,
        page: Some((page.width, page.left, page.top)),
        best_mark_id: hl.best_mark_id(),
        truncated,
        notes: notes.into_vec(),
    })
}

// ── page geometry ────────────────────────────────────────────────────────────

/// The master page the document starts on: the first `text:p` or `text:h` whose
/// paragraph style names one.
///
/// `None` — the state of two of the three corpus documents — leaves the choice to
/// `Styles::page_setup`, which falls back to the document's first master page.
/// Bounded, because a document that names a master page on paragraph 900 is
/// starting a *later* page with it, not this one.
fn first_master(styles: &Styles, body: Node) -> Option<String> {
    body.descendants()
        .filter(|n| n.is_element() && matches!(n.tag_name().name(), "p" | "h"))
        .take(MAX_MASTER_SCAN)
        .find_map(|n| {
            styles
                .master_page_of(attr_local(n, "style-name")?)
                .map(str::to_string)
        })
}

/// Section style names that lay their content out in more than one column.
///
/// `style:family="section"` is not part of the style cascade — a section states
/// only its column layout, and this renderer has one column — so the single fact
/// worth telling the reader about is collected here instead of in [`Styles`].
fn multi_column_sections(root: Node) -> HashSet<String> {
    let mut out = HashSet::new();
    for container in elems(root)
        .filter(|e| matches!(e.tag_name().name(), "styles" | "automatic-styles"))
    {
        for s in elems(container).filter(|e| e.tag_name().name() == "style") {
            if attr_local(s, "family") != Some("section") || out.len() >= MAX_SECTION_STYLES {
                continue;
            }
            let Some(name) = attr_local(s, "name") else {
                continue;
            };
            let cols = child(s, "section-properties")
                .and_then(|p| child(p, "columns"))
                .and_then(|c| attr_u32(c, "column-count"))
                .unwrap_or(1);
            if cols > 1 {
                out.insert(name.to_string());
            }
        }
    }
    out
}

// ── block level ──────────────────────────────────────────────────────────────

/// Walks the children of `office:text` — or of a `text:section`, an index body, a
/// `table:table-cell` or a `draw:text-box` standing in for part of it — and emits
/// each block in document order.
///
/// Two independent depths, because the two things they bound nest independently:
/// `depth` counts the wrappers around this content, `tables` the tables enclosing
/// it.
pub fn walk<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    parent: Node<'a, 'a>,
    depth: usize,
    tables: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for n in elems(parent) {
        if w.is_full() {
            break;
        }
        match n.tag_name().name() {
            "p" | "h" => emit_para(ctx, w, n, None),
            "list" => emit_list(ctx, w, n, depth, tables),
            "table" => table::emit_table(ctx, w, n, tables),
            // A section is a named region with its own column layout and, when it
            // is linked, its own source. Its content is ordinary blocks; the
            // layout is the part this column cannot honour.
            "section" => {
                if attr_local(n, "style-name").is_some_and(|s| ctx.multi_col.contains(s)) {
                    ctx.notes.add(NOTE_COLUMNS);
                }
                walk(ctx, w, n, depth + 1, tables);
            }
            // An index — a table of contents, a list of figures — is generated
            // content whose *cached* body is the only thing that exists here: the
            // producer already resolved every entry into paragraphs.
            "table-of-content" | "illustration-index" | "table-index" | "object-index"
            | "user-index" | "alphabetical-index" | "bibliography" => {
                if let Some(b) = child(n, "index-body") {
                    walk(ctx, w, b, depth + 1, tables);
                }
            }
            // A frame anchored to the page or to a paragraph can sit between
            // blocks. It has no paragraph to join, so it gets one of its own.
            "frame" => emit_frame_block(ctx, w, n),
            // `text:soft-page-break` is the producer's own pagination — where
            // *its* layout engine broke the page — and this column has no pages.
            // An author-stated `fo:break-before` is a different thing entirely and
            // is drawn; see [`emit_para`].
            "soft-page-break" => {}
            // The deleted halves of tracked changes live in here, and a preview
            // that shows one is showing a document that does not exist. The
            // `text:change*` markers left in the body are handled where they occur.
            "tracked-changes" => {}
            // A bookmark between blocks: an id for a link to land on and nothing
            // else. A `span` rather than nothing at all because a table of
            // contents entry points at exactly these.
            "bookmark-start" | "bookmark" => {
                if let Some(id) = bookmark(ctx, n) {
                    w.open("span", &attr("id", &id));
                    w.close();
                }
            }
            // Declarations, view state and zero-width markers rather than content.
            // `office:annotation` is here for the reason `docx::body` drops
            // `w:commentReference`: a comment is an annotation *about* the document
            // rather than part of it, and Writer does not print one either.
            "sequence-decls" | "user-field-decls" | "variable-decls" | "dde-connection-decls"
            | "alphabetical-index-auto-mark-file" | "forms" | "annotation" | "annotation-end"
            | "bookmark-end" | "change" | "change-start" | "change-end" => {}
            _ => {}
        }
    }
}

/// One `text:p` or `text:h`.
fn emit_para<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, p: Node<'a, 'a>, list: Option<ListPart>) {
    if ctx.paras >= MAX_PARAS {
        ctx.notes.add(NOTE_PARAS);
        return;
    }
    ctx.paras += 1;

    let style_name = attr_local(p, "style-name").unwrap_or("");
    let resolved = ctx.styles.resolve(Family::Paragraph, style_name);
    // The paragraph's own text properties size an empty line and are what every
    // run's size is measured against.
    let base_pt = style::size_pt(&resolved.text, style::DEFAULT_SIZE_PT);
    let mut pp = resolved.para.clone();

    let (marker, indent) = match &list {
        Some(ListPart::Marker(m)) => (marker_of(ctx, m), Some(m.indent)),
        Some(ListPart::Indent(i)) => (None, Some(*i)),
        None => (None, None),
    };
    // A level's own indentation replaces the paragraph's rather than adding to it:
    // every level in the corpus states it through
    // `text:list-level-label-alignment`, which is the mode ODF gives precedence
    // over the paragraph style's margins. A level that states none — the older
    // `text:space-before` spelling resolves to zero here — leaves the paragraph's
    // own indent alone.
    if let Some(i) = indent.filter(|i| i.indent_px != 0.0 || i.first_line_px != 0.0) {
        pp.indent_px = Some(i.indent_px);
        pp.first_line_px = Some(i.first_line_px);
    }

    // A break the *author* stated. Drawn as the boundary it is, because a
    // scrolling column cannot paginate one away: `br.of-pb` in
    // `docshape::BASE_CSS` is the rule.
    if drawn_break(pp.break_before) {
        w.void("br", &attr("class", ctx.classes.html.break_class));
    }

    let mut para = style::to_para(&pp, base_pt, heading_of(p));
    // `style:contextual-spacing` drops the space between neighbours of one style.
    // Only the leading side can be honoured: the predecessor is already in the
    // buffer, so its `space_after_px` cannot be taken back. Worst case is one
    // gap's worth of space at the seam, never lost text.
    if pp.contextual_spacing == Some(true) && ctx.prev_style.as_deref() == Some(style_name) {
        para.space_before_px = 0.0;
    }
    para.marker = marker;

    let mut runs: Vec<Run> = Vec::new();
    collect_runs(ctx, &mut runs, p, &resolved.text, base_pt, 0, None);
    para.runs = runs;

    // One paragraph, one call: document order with the other block kinds needs no
    // buffering, because nothing downstream looks at more than one paragraph.
    model::emit_paras(
        w,
        std::slice::from_ref(&para),
        &ctx.classes.html,
        ctx.marker,
        ctx.terms,
    );
    // Blocks the runs could not hold — a text frame's own paragraphs — land here,
    // immediately after the paragraph they were anchored to. Already-escaped
    // markup from a writer of its own; see [`hoist`].
    for html in std::mem::take(&mut ctx.pending) {
        w.raw(&html);
    }
    if drawn_break(pp.break_after) {
        w.void("br", &attr("class", ctx.classes.html.break_class));
    }
    ctx.prev_style = Some(style_name.to_string());
}

/// Whether a `fo:break-*` value is one this renderer draws. `Auto` is the
/// document cancelling an inherited break, and there is nothing to draw for it.
fn drawn_break(b: Option<BreakKind>) -> bool {
    matches!(b, Some(BreakKind::Page) | Some(BreakKind::Column))
}

/// The heading level of one block: `text:outline-level` on a `text:h`, clamped to
/// the six levels HTML has elements for.
///
/// Levels past 6 collapse onto `h6` rather than losing their place in the outline,
/// which is what the docx path does with `w:outlineLvl`. A `text:h` that states no
/// level at all is still a heading, and level 1 is what Writer means by one.
fn heading_of(p: Node) -> Option<u8> {
    if p.tag_name().name() != "h" {
        return None;
    }
    Some(attr_u32(p, "outline-level").unwrap_or(1).clamp(1, 6) as u8)
}

/// The id of one `text:bookmark-start` or `text:bookmark`. Bounded per document: a
/// bookmark is a target for a link, so a file with more of them than a reader has
/// links to follow is spending the byte cap on ids.
fn bookmark(ctx: &mut Ctx, n: Node) -> Option<String> {
    if ctx.bookmarks >= MAX_BOOKMARKS {
        return None;
    }
    let id = link::bookmark_id(attr_local(n, "name")?)?;
    ctx.bookmarks += 1;
    Some(id)
}

/// A `draw:frame` between blocks: a paragraph of its own holding just the frame,
/// so a page-anchored picture is still somewhere in the flow.
fn emit_frame_block<'a>(ctx: &mut Ctx<'a>, w: &mut Writer, frame: Node<'a, 'a>) {
    let mut runs: Vec<Run> = Vec::new();
    draw::emit_frame(ctx, &mut runs, frame);
    if !runs.is_empty() {
        let base = ctx.styles.resolve(Family::Paragraph, BASE_STYLE);
        let base_pt = style::size_pt(&base.text, style::DEFAULT_SIZE_PT);
        let mut para = style::to_para(&base.para, base_pt, None);
        para.runs = runs;
        model::emit_paras(
            w,
            std::slice::from_ref(&para),
            &ctx.classes.html,
            ctx.marker,
            ctx.terms,
        );
    }
    for html in std::mem::take(&mut ctx.pending) {
        w.raw(&html);
    }
}

/// Renders a text frame's block content into a buffer of its own, for
/// [`emit_para`] to flush after the paragraph that anchored it.
///
/// Hoisted rather than emitted in place because `draw:text-box` holds paragraphs
/// and tables, and a block element inside a `<p>` is closed *before* the paragraph
/// by every HTML parser — the frame would tear the anchor in two. The position the
/// document gives the frame is lost either way (it is absolute against a page this
/// column does not paginate), which is what [`NOTE_TXBX`] says out loud.
pub fn hoist<'a>(ctx: &mut Ctx<'a>, content: Node<'a, 'a>) {
    if ctx.boxes >= MAX_BOXES || ctx.box_depth >= MAX_BOX_DEPTH {
        ctx.notes.add(NOTE_BOXES);
        return;
    }
    ctx.boxes += 1;
    ctx.box_depth += 1;
    ctx.notes.add(NOTE_TXBX);
    // The pending list belongs to the paragraph being built *outside* this frame; a
    // frame nested in here flushes into this buffer, so the outer list must not be
    // reachable while it renders.
    let outer = std::mem::take(&mut ctx.pending);
    let mut inner = Writer::new(BOX_CAP);
    inner.open("div", &attr("class", "of-txbx"));
    walk(ctx, &mut inner, content, 0, 0);
    inner.close();
    // Normally empty — every paragraph flushes what it anchored — but a frame
    // anchored to a paragraph the paragraph cap refused still has to go somewhere.
    let stranded = std::mem::replace(&mut ctx.pending, outer);
    ctx.pending.push(inner.finish());
    ctx.pending.extend(stranded);
    ctx.box_depth -= 1;
}

// ── lists ────────────────────────────────────────────────────────────────────

/// One `text:list`, with the counter contract [`Lists`] documents: `enter` once
/// before descending, exactly one `item` per *drawn* item in document order, and
/// `leave` once on the way out. A doubled or skipped `item` renumbers every label
/// after it, which is why the shape of this function is the API.
fn emit_list<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    list: Node<'a, 'a>,
    depth: usize,
    tables: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    ctx.lists.enter(ListStart::read(list));
    for item in elems(list) {
        if w.is_full() {
            break;
        }
        match item.tag_name().name() {
            // A header consumes no number. It is still indented like an item,
            // which is what `Lists::header` is for — and it takes `&self`, so it
            // cannot be the call that advances a counter.
            "list-header" => {
                let part = ctx.lists.header().map(ListPart::Indent);
                emit_item(ctx, w, item, part, depth, tables);
            }
            "list-item" => {
                if let Some(v) = attr_u32(item, "start-value") {
                    ctx.lists.restart_at(v);
                }
                // An item holding nothing but a nested list draws no label of its
                // own, so it must not consume a number either.
                let part = if draws_label(item) {
                    let base_pt = item_size_pt(ctx, item);
                    ctx.lists.item(ctx.styles, base_pt).map(ListPart::Marker)
                } else {
                    None
                };
                emit_item(ctx, w, item, part, depth, tables);
            }
            _ => {}
        }
    }
    ctx.lists.leave();
}

/// The content of one item: the label lands on its first paragraph, the level's
/// indentation on the ones after it, and a nested list recurses one level deeper.
fn emit_item<'a>(
    ctx: &mut Ctx<'a>,
    w: &mut Writer,
    item: Node<'a, 'a>,
    part: Option<ListPart>,
    depth: usize,
    tables: usize,
) {
    let indent = match &part {
        Some(ListPart::Marker(m)) => Some(m.indent),
        Some(ListPart::Indent(i)) => Some(*i),
        None => None,
    };
    let mut first = part;
    for n in elems(item) {
        if w.is_full() {
            break;
        }
        match n.tag_name().name() {
            "p" | "h" => {
                let part = first.take().or_else(|| indent.map(ListPart::Indent));
                emit_para(ctx, w, n, part);
            }
            "list" => emit_list(ctx, w, n, depth + 1, tables),
            "table" => table::emit_table(ctx, w, n, tables),
            // `text:number` is the producer's own pre-rendered label, stale the
            // moment anything above it changes; `Lists` regenerates it.
            "number" => {}
            _ => walk(ctx, w, item, depth + 1, tables),
        }
    }
}

/// Whether an item draws a label, i.e. holds content of its own rather than only
/// a nested list.
fn draws_label(item: Node) -> bool {
    elems(item).any(|n| !matches!(n.tag_name().name(), "list" | "number"))
}

/// The size the item's own first paragraph resolves to: a level states its label's
/// size as a percentage often enough that the marker cannot be sized without it.
fn item_size_pt(ctx: &Ctx, item: Node) -> f32 {
    let name = elems(item)
        .find(|n| matches!(n.tag_name().name(), "p" | "h"))
        .and_then(|n| attr_local(n, "style-name"))
        .unwrap_or(BASE_STYLE);
    style::size_pt(
        &ctx.styles.resolve(Family::Paragraph, name).text,
        style::DEFAULT_SIZE_PT,
    )
}

/// A level's marker as the model's own. `text:label-followed-by` decides what
/// fills the gap after the label.
fn marker_of(ctx: &mut Ctx, m: &ListMark) -> Option<ListMarker> {
    if m.image.is_some() {
        // The model's marker carries text, not an image, so the level's fallback
        // bullet is what is drawn — and the reader is told why.
        ctx.notes.add(NOTE_BULLET_IMAGE);
    }
    let mut label = m.label.clone()?;
    match m.indent.followed_by {
        // A non-breaking space, because the page is `white-space:pre-wrap` and an
        // ordinary trailing space would still be a wrap opportunity between the
        // marker and its first word.
        FollowedBy::Space => label.label.push('\u{00a0}'),
        // A tab needs nothing: the marker span is sized to the hanging indent (see
        // `model::emit_marker`), so the gap it would advance across is already the
        // width of the box the marker sits in. `nothing` needs nothing by name.
        FollowedBy::ListTab | FollowedBy::Nothing => {}
    }
    Some(label)
}

// ── runs ─────────────────────────────────────────────────────────────────────

/// Collects the runs of one paragraph.
///
/// Iterates *children*, not elements: in ODF the text is character data inside
/// `text:p` and `text:span`, so a text node is a run and an element is either
/// markup around runs or one of the handful of things that are a run by
/// themselves.
///
/// `link` is the destination of the `text:a` this content sits inside. It is
/// threaded rather than merged into [`TextProps`] because it belongs to the
/// wrapper, and a nested link replaces it outright instead of inheriting — a
/// rejected destination must not fall back to the enclosing one.
fn collect_runs<'a>(
    ctx: &mut Ctx<'a>,
    out: &mut Vec<Run>,
    parent: Node<'a, 'a>,
    base: &TextProps,
    base_pt: f32,
    depth: usize,
    link: Option<&str>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for c in parent.children() {
        if out.len() >= MAX_RUNS {
            return;
        }
        if c.is_text() {
            // Verbatim, including whitespace: the page is `white-space:pre-wrap`,
            // so collapsing here would lose indentation the author typed.
            match c.text() {
                Some(t) if !t.is_empty() => {
                    out.push(text_run(ctx, t.to_string(), base, base_pt, link))
                }
                _ => {}
            }
            continue;
        }
        if !c.is_element() {
            continue;
        }
        match c.tag_name().name() {
            "span" => {
                let props = merged_text(ctx, base, c);
                collect_runs(ctx, out, c, &props, base_pt, depth + 1, link);
            }
            // A link around runs. An unresolvable or non-whitelisted destination
            // gives `None`, which renders the text without an `<a>`.
            "a" => {
                let href = attr_local(c, "href").and_then(link::sanitize_href);
                let props = merged_text(ctx, base, c);
                collect_runs(ctx, out, c, &props, base_pt, depth + 1, href.as_deref());
            }
            // `text:c` spaces in one element, because ODF collapses runs of them
            // in the markup.
            "s" => {
                let n = attr_u32(c, "c").unwrap_or(1).min(MAX_SPACES as u32) as usize;
                out.push(text_run(ctx, " ".repeat(n), base, base_pt, link));
            }
            "tab" => out.push(Run::Tab),
            "line-break" => out.push(Run::Break(Break::Line)),
            // A note's own marker carries its own href, so the enclosing link is
            // not threaded into it: a footnote inside a hyperlink links to the
            // note, which is the only destination a reader can use here.
            "note" => note(ctx, out, c, base, base_pt),
            "frame" => draw::emit_frame(ctx, out, c),
            // A bookmark inside the paragraph is a position in the text, so it is
            // a run rather than a block of its own.
            "bookmark-start" | "bookmark" => {
                if let Some(id) = bookmark(ctx, c) {
                    out.push(Run::Anchor(id));
                }
            }
            // Zero-width markers around content rather than content: bookmark and
            // reference ends, index marks, the tracked-change anchors whose
            // deleted text lives in `text:tracked-changes`, and the producer's own
            // pagination.
            "bookmark-end" | "reference-mark" | "reference-mark-start" | "reference-mark-end"
            | "alphabetical-index-mark" | "alphabetical-index-mark-start"
            | "alphabetical-index-mark-end" | "toc-mark" | "toc-mark-start" | "toc-mark-end"
            | "user-index-mark" | "user-index-mark-start" | "user-index-mark-end"
            | "change" | "change-start" | "change-end" | "soft-page-break" | "number" => {}
            // A comment, and text the document itself hides: neither is what the
            // reader sees on the page, and hiding it here also keeps it away from
            // the search-term highlighter — a preview that scrolls to a match
            // inside hidden text would jump to nothing.
            "annotation" | "annotation-end" | "hidden-text" | "hidden-paragraph" => {}
            // Everything else that can sit inside a paragraph wraps text: a field
            // with its cached result (`text:page-number`, `text:sequence`,
            // `text:bookmark-ref`, a date, a title), a ruby, a meta span. Their
            // content *is* document text, so descending is what keeps it.
            _ => collect_runs(ctx, out, c, base, base_pt, depth + 1, link),
        }
    }
}

/// The text properties of a `text:span` or `text:a`: its own `text:style-name`
/// resolved on its own and merged over what it sits inside.
///
/// Resolved separately rather than as one chain because `fo:font-size` may be a
/// percentage of the enclosing size, and `style::merge_text` is the only place
/// that knows what it is a percentage *of*.
fn merged_text(ctx: &Ctx, base: &TextProps, n: Node) -> TextProps {
    let mut props = base.clone();
    if let Some(name) = attr_local(n, "style-name") {
        style::merge_text(&mut props, &ctx.styles.resolve(Family::Text, name).text);
    }
    props
}

/// One span of text with its resolved properties.
///
/// A run whose face is the page's own states no `font-family`, the same way the
/// model elides a size that matches the paragraph's: `.of-page` carries that face,
/// so the computed style is identical and a long document saves one declaration
/// per run.
fn text_run(ctx: &Ctx, text: String, t: &TextProps, base_pt: f32, link: Option<&str>) -> Run {
    let mut run = style::to_text_run(text, t, base_pt);
    if run.font.is_some() && run.font.as_deref() == ctx.default_font {
        run.font = None;
    }
    if let Some(href) = link {
        // An underline and nothing else. ODF has no theme slot a link colour could
        // come from — Writer paints one through the `Internet Link` character
        // style, which arrives through the `text:a`'s own `text:style-name` when
        // the document states it — so inventing a blue here would repaint links
        // the author left black.
        run.underline = true;
        run.link = Some(href.to_string());
    }
    Run::Text(run)
}

// ── footnotes and endnotes ───────────────────────────────────────────────────

/// A `text:note`: the marker in the text, plus the body remembered for the tail
/// block. ODF keeps the note's text where the reference stands, so this is the
/// only place it can be collected from.
fn note<'a>(ctx: &mut Ctx<'a>, out: &mut Vec<Run>, n: Node<'a, 'a>, t: &TextProps, base_pt: f32) {
    // The tail block is mid-write, and a note that references itself would not
    // terminate.
    if ctx.in_note {
        return;
    }
    let Some(body) = child(n, "note-body") else {
        return;
    };
    if ctx.note_refs.len() >= MAX_NOTES_SHOWN {
        ctx.notes.add(NOTE_NOTES_CAPPED);
        return;
    }
    let endnote = attr_local(n, "note-class") == Some("endnote");
    let num = ctx.note_refs.iter().filter(|r| r.endnote == endnote).count() + 1;
    // The citation is what the document shows in front of the note — usually the
    // number, but an author may have typed `*`. Unlike docx's cached field result
    // it is regenerated by the producer on every save, so it is not stale; a note
    // written without one still needs a label, hence the fallback.
    let label = citation(n).unwrap_or_else(|| {
        if endnote {
            listnum::roman(num as u32, false)
        } else {
            listnum::decimal(num as u32)
        }
    });
    let r = NoteRef {
        endnote,
        label: label.clone(),
        body,
        num,
    };
    let (target, anchor) = (r.note_anchor(), r.ref_anchor());
    ctx.note_refs.push(r);

    out.push(Run::Anchor(anchor));
    let mut run = text_run(ctx, label, t, base_pt, Some(&format!("#{target}")));
    if let Run::Text(tr) = &mut run {
        // A note reference is a superscript in every producer's output; a document
        // that states one anyway agrees with this rather than fighting it.
        tr.script = Some(Script::Super);
    }
    out.push(run);
}

/// The `text:note-citation`'s own text, bounded.
fn citation(n: Node) -> Option<String> {
    let body = child(n, "note-citation")?;
    let mut s = String::new();
    xml::inner_text(body, &mut s);
    let s: String = s.trim().chars().take(MAX_CITATION_CHARS).collect();
    (!s.is_empty()).then_some(s)
}

/// The notes the column referenced, as one hairline-separated block at the end of
/// it. Consumes the collected references, so it renders once.
fn emit_note_tail<'a>(ctx: &mut Ctx<'a>, w: &mut Writer) {
    let used = std::mem::take(&mut ctx.note_refs);
    if used.is_empty() {
        return;
    }
    let start = w.len();
    w.open("div", &attr("class", "of-fnotes"));
    for r in &used {
        if w.is_full() {
            break;
        }
        // The tail's own byte budget, checked between notes: the writer's cap is
        // the whole document's, and a document whose notes are longer than its body
        // would spend it here.
        if w.len().saturating_sub(start) > MAX_NOTE_TAIL_BYTES {
            ctx.notes.add(NOTE_NOTES_CAPPED);
            break;
        }
        w.open(
            "div",
            &super::super::html::attrs(&[
                &attr("class", "of-fn"),
                &attr("id", &r.note_anchor()),
            ]),
        );
        // The number is drawn here rather than from the note's own citation
        // element, because it is also the link back to the marker.
        w.open(
            "a",
            &super::super::html::attrs(&[
                &attr("class", "of-fnb"),
                &attr("href", &format!("#{}", r.ref_anchor())),
            ]),
        );
        w.text(&r.label);
        w.close();
        ctx.in_note = true;
        // A note's body is ordinary block content, so it renders through the same
        // walk the page does.
        walk(ctx, w, r.body, 0, 0);
        ctx.in_note = false;
        w.close();
    }
    w.close();
}

/// Fixtures live here rather than in each renderer's own test module: an ODF
/// package is the same three parts whatever its body holds, and `table` and `draw`
/// are tested through the same walk that drives them in production.
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::office::pkg::TestPkg;

    /// Every prefix the fixtures use. `roxmltree` refuses an undeclared one, and
    /// the renderers match on local names, so which URIs these are does not matter
    /// beyond being the real ones.
    pub const NS: &str = "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
         xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
         xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
         xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
         xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
         xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
         xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
         xmlns:xlink=\"http://www.w3.org/1999/xlink\"";

    /// An ODF package on disk. Text parts and media parts are separate arguments
    /// because a picture is not UTF-8.
    pub struct Fixture(TestPkg);

    impl Fixture {
        pub fn new(tag: &str, parts: &[(&str, Vec<u8>)]) -> Fixture {
            Fixture(TestPkg::new(tag, parts))
        }

        pub fn path(&self) -> &str {
            self.0.path()
        }

        pub fn doc(&self) -> OfficeDoc {
            super::super::render(self.path(), None, &[]).expect("render")
        }

        pub fn html(&self) -> String {
            self.doc().html
        }

        /// The rendered body without the stylesheet in front of it.
        ///
        /// Worth having: `docshape::BASE_CSS` mentions every class the renderers
        /// emit and its comments are prose, so a `contains("of-txbx")` or a
        /// `find("before")` against the whole document passes on the stylesheet and
        /// says nothing about the markup.
        pub fn body_html(&self) -> String {
            let html = self.html();
            match html.split_once("</style>") {
                Some((_, body)) => body.to_string(),
                None => html,
            }
        }
    }

    /// `content.xml`: `auto` becomes its automatic styles, `body` its text body.
    pub fn content(auto: &str, body: &str) -> String {
        format!(
            "<office:document-content {NS}>\
             <office:automatic-styles>{auto}</office:automatic-styles>\
             <office:body><office:text>{body}</office:text></office:body>\
             </office:document-content>"
        )
    }

    /// `styles.xml`: the `Standard` paragraph style every producer writes, a US
    /// Letter page with 1in margins (816 × 1056 px, 96px padding), plus whatever
    /// `extra` adds to `office:styles` and `master` to the master page.
    pub fn styles_with(extra: &str, master: &str) -> String {
        format!(
            "<office:document-styles {NS}>\
             <office:styles>\
             <style:style style:name=\"Standard\" style:family=\"paragraph\">\
             <style:text-properties fo:font-family=\"Widget Sans\" fo:font-size=\"12pt\"/>\
             </style:style>{extra}</office:styles>\
             <office:automatic-styles>\
             <style:page-layout style:name=\"pm1\"><style:page-layout-properties \
             fo:page-width=\"8.5in\" fo:page-height=\"11in\" fo:margin-top=\"1in\" \
             fo:margin-bottom=\"1in\" fo:margin-left=\"1in\" fo:margin-right=\"1in\"/>\
             </style:page-layout></office:automatic-styles>\
             <office:master-styles>\
             <style:master-page style:name=\"Standard\" style:page-layout-name=\"pm1\">{master}\
             </style:master-page></office:master-styles></office:document-styles>"
        )
    }

    pub fn styles(extra: &str) -> String {
        styles_with(extra, "")
    }

    /// A three-part odt: `mimetype`, `styles.xml`, `content.xml`.
    pub fn odt(tag: &str, styles_xml: &str, content_xml: &str) -> Fixture {
        odt_media(tag, styles_xml, content_xml, &[])
    }

    pub fn odt_media(
        tag: &str,
        styles_xml: &str,
        content_xml: &str,
        media: &[(&str, Vec<u8>)],
    ) -> Fixture {
        let mut parts: Vec<(&str, Vec<u8>)> = vec![
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.text".to_vec(),
            ),
            ("styles.xml", styles_xml.as_bytes().to_vec()),
            ("content.xml", content_xml.as_bytes().to_vec()),
        ];
        parts.extend(media.iter().map(|(n, b)| (*n, b.clone())));
        Fixture::new(tag, &parts)
    }

    /// The common case: default styles, one body, no automatic styles.
    pub fn body(tag: &str, body: &str) -> Fixture {
        odt(tag, &styles(""), &content("", body))
    }

    /// A real PNG, so the media path decodes and re-encodes it for its box. Opaque,
    /// so what comes back is a JPEG — which is why the assertions stop at
    /// `data:image/`.
    pub fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_fn(w, h, |x, y| image::Rgb([(x * 8) as u8, (y * 8) as u8, 40]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    // ── the document as a whole ─────────────────────────────────────────────

    #[test]
    fn renders_one_page_column_at_the_stated_geometry() {
        let doc = body("geom", "<text:p>café</text:p>").doc();
        assert_eq!(doc.shape, Shape::Doc);
        // 8.5in × 96dpi, and a 1in margin on each side.
        assert_eq!(doc.page, Some((816.0, 96.0, 96.0)));
        // A text document renders whole: one section, so the strip has nothing to
        // switch between.
        assert!(doc.sections.is_empty());
        assert_eq!(doc.section, 0);
        assert!(doc.natural.is_none(), "a column has no fixed canvas");
        assert!(!doc.truncated);
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
        assert!(doc.html.contains("class=\"of-page\""), "{}", doc.html);
        assert!(doc.html.contains("café"));
    }

    #[test]
    fn a_missing_styles_part_still_renders_and_says_so() {
        let f = Fixture::new(
            "nostyles",
            &[(
                "content.xml",
                content("", "<text:p>café</text:p>").into_bytes(),
            )],
        );
        let doc = f.doc();
        assert!(doc.html.contains("café"));
        // ODF keeps the real stylesheet in `styles.xml`, so losing it is a
        // degradation rather than the loss of a few overrides.
        assert!(!doc.notes.is_empty(), "a missing stylesheet is a note");
    }

    #[test]
    fn a_missing_or_malformed_body_is_an_error() {
        let f = Fixture::new("nocontent", &[("styles.xml", styles("").into_bytes())]);
        assert!(super::super::render(f.path(), None, &[]).is_err());

        let f = Fixture::new(
            "badxml",
            &[("content.xml", b"<office:document-content".to_vec())],
        );
        assert!(super::super::render(f.path(), None, &[]).is_err());
    }

    #[test]
    fn document_text_is_escaped() {
        let html = body("esc", "<text:p>a &lt;b&gt; &amp; \"c\"</text:p>").html();
        assert!(html.contains("a &lt;b&gt; &amp; \"c\""), "{html}");
    }

    #[test]
    fn headings_are_real_elements_and_clamp_at_six() {
        let html = body(
            "head",
            "<text:h text:outline-level=\"1\">one</text:h>\
             <text:h text:outline-level=\"8\">deep</text:h>\
             <text:h>bare</text:h>",
        )
        .html();
        assert!(html.contains("<h1 "), "{html}");
        // Level 8 keeps its place in the outline rather than losing it: HTML has
        // six heading elements, so the deeper levels collapse onto the last.
        assert!(html.contains("<h6 "), "{html}");
        assert!(!html.contains("<h8"), "{html}");
        // A `text:h` with no level is still a heading, and level 1 is what Writer
        // means by one.
        assert_eq!(html.matches("<h1 ").count(), 2, "{html}");
    }

    #[test]
    fn headers_and_footers_are_noted_rather_than_drawn() {
        let f = odt(
            "hdr",
            &styles_with(
                "",
                "<style:header><text:p>naïve running head</text:p></style:header>",
            ),
            &content("", "<text:p>café</text:p>"),
        );
        let doc = f.doc();
        assert!(doc.notes.iter().any(|n| n == NOTE_HEADER), "{:?}", doc.notes);
        assert!(
            !doc.html.contains("running head"),
            "a header belongs to a page, and this column has none"
        );
    }

    // ── the property cascade ────────────────────────────────────────────────

    #[test]
    fn character_styles_cascade_and_a_percentage_size_resolves() {
        let extra = "<style:style style:name=\"Strong\" style:family=\"text\">\
             <style:text-properties fo:font-weight=\"bold\"/></style:style>\
             <style:style style:name=\"Big\" style:family=\"text\">\
             <style:text-properties fo:font-size=\"200%\" fo:font-style=\"italic\"/></style:style>";
        let html = odt(
            "chars",
            &styles(extra),
            &content(
                "",
                "<text:p><text:span text:style-name=\"Strong\">bold\
                 <text:span text:style-name=\"Big\">both</text:span></text:span></text:p>",
            ),
        )
        .html();
        assert!(html.contains("font-weight:700"), "{html}");
        // 200% of the paragraph's 12pt is 24pt, i.e. 32px — and the nested span
        // keeps the weight it inherited.
        assert!(html.contains("font-size:32px"), "{html}");
        assert!(html.contains("font-style:italic"), "{html}");
    }

    #[test]
    fn an_automatic_style_overrides_the_named_one_it_is_based_on() {
        let auto = "<style:style style:name=\"P1\" style:family=\"paragraph\" \
             style:parent-style-name=\"Standard\">\
             <style:paragraph-properties fo:text-align=\"center\" fo:margin-left=\"0.5in\" \
             fo:margin-right=\"0.25in\" fo:text-indent=\"-0.25in\"/></style:style>";
        let html = odt(
            "auto",
            &styles(""),
            &content(auto, "<text:p text:style-name=\"P1\">café</text:p>"),
        )
        .html();
        assert!(html.contains("text-align:center"), "{html}");
        assert!(html.contains("margin-left:48px"), "{html}");
        assert!(html.contains("margin-right:24px"), "{html}");
        // ODF spells a hanging indent as a negative `fo:text-indent`, which is
        // already the model's convention — nothing is negated on the way through.
        assert!(html.contains("text-indent:-24px"), "{html}");
    }

    #[test]
    fn the_three_line_height_spellings_reach_the_box() {
        let auto = "<style:style style:name=\"Prop\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:line-height=\"150%\"/></style:style>\
             <style:style style:name=\"Least\" style:family=\"paragraph\">\
             <style:paragraph-properties style:line-height-at-least=\"24pt\"/></style:style>\
             <style:style style:name=\"Lead\" style:family=\"paragraph\">\
             <style:paragraph-properties style:line-spacing=\"6pt\"/></style:style>";
        let html = odt(
            "lines",
            &styles(""),
            &content(
                auto,
                "<text:p text:style-name=\"Prop\">a</text:p>\
                 <text:p text:style-name=\"Least\">b</text:p>\
                 <text:p text:style-name=\"Lead\">c</text:p>",
            ),
        )
        .html();
        // A proportional height is against the font's line, so 150% of single
        // spacing is 1.8 of the em box — unitless, so a run bigger than the
        // paragraph resolves it against its own size.
        assert!(html.contains("line-height:1.8;"), "{html}");
        // A floor keeps the em fallback in the declaration: which of the two wins
        // depends on the font, so CSS decides it at layout time.
        assert!(html.contains("line-height:max(32px, 1.2em)"), "{html}");
        // Leading is added to a single line rather than being one.
        assert!(html.contains("line-height:27.2px"), "{html}");
    }

    #[test]
    fn a_paragraph_carries_its_own_border_and_shading() {
        let auto = "<style:style style:name=\"Boxed\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:border=\"1pt solid #ff0000\" \
             fo:background-color=\"#00ff00\"/></style:style>";
        let html = odt(
            "boxed",
            &styles(""),
            &content(auto, "<text:p text:style-name=\"Boxed\">café</text:p>"),
        )
        .html();
        assert!(html.contains("background-color:#00ff00"), "{html}");
        assert!(html.contains("solid #ff0000"), "{html}");
    }

    #[test]
    fn contextual_spacing_closes_the_gap_between_neighbours_of_one_style() {
        let auto = "<style:style style:name=\"Tight\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:margin-top=\"12pt\" fo:margin-bottom=\"12pt\" \
             style:contextual-spacing=\"true\"/></style:style>\
             <style:style style:name=\"Loose\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:margin-top=\"12pt\"/></style:style>";
        let two = odt(
            "ctx",
            &styles(""),
            &content(
                auto,
                "<text:p text:style-name=\"Tight\">one</text:p>\
                 <text:p text:style-name=\"Tight\">two</text:p>",
            ),
        )
        .html();
        // The first still opens its gap — only the seam between two neighbours of
        // the same style closes, and only on the leading side (the predecessor is
        // already in the buffer).
        assert_eq!(two.matches("margin-top:16px").count(), 1, "{two}");

        let mixed = odt(
            "ctx2",
            &styles(""),
            &content(
                auto,
                "<text:p text:style-name=\"Loose\">one</text:p>\
                 <text:p text:style-name=\"Tight\">two</text:p>",
            ),
        )
        .html();
        assert_eq!(mixed.matches("margin-top:16px").count(), 2, "{mixed}");
    }

    #[test]
    fn an_author_page_break_is_drawn_and_the_producers_is_not() {
        let auto = "<style:style style:name=\"Br\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:break-before=\"page\"/></style:style>\
             <style:style style:name=\"Auto\" style:family=\"paragraph\">\
             <style:paragraph-properties fo:break-before=\"auto\"/></style:style>";
        let html = odt(
            "breaks",
            &styles(""),
            &content(
                auto,
                "<text:p>one</text:p><text:soft-page-break/>\
                 <text:p text:style-name=\"Br\">two</text:p>\
                 <text:p text:style-name=\"Auto\">three</text:p>",
            ),
        )
        .html();
        // One rule: the author's break. `text:soft-page-break` is where the
        // producer's own layout engine broke a page this column does not have, and
        // `auto` is the document cancelling an inherited break.
        assert_eq!(html.matches("class=\"of-pb\"").count(), 1, "{html}");
    }

    #[test]
    fn a_multi_column_section_is_shown_as_one_column_and_says_so() {
        let auto = "<style:style style:name=\"Sect1\" style:family=\"section\">\
             <style:section-properties><style:columns fo:column-count=\"2\"/>\
             </style:section-properties></style:style>";
        let doc = odt(
            "cols",
            &styles(""),
            &content(
                auto,
                "<text:section text:style-name=\"Sect1\"><text:p>café</text:p></text:section>",
            ),
        )
        .doc();
        assert!(doc.html.contains("café"), "a section's content is blocks");
        assert!(
            doc.notes.iter().any(|n| n == NOTE_COLUMNS),
            "{:?}",
            doc.notes
        );
    }

    // ── lists ───────────────────────────────────────────────────────────────

    /// Two numbered levels and a bullet third, the shape every producer writes.
    fn list_styles() -> String {
        "<text:list-style style:name=\"L1\">\
         <text:list-level-style-number text:level=\"1\" style:num-format=\"1\" \
         style:num-suffix=\".\"/>\
         <text:list-level-style-number text:level=\"2\" style:num-format=\"a\" \
         style:num-suffix=\")\"/>\
         <text:list-level-style-bullet text:level=\"3\" text:bullet-char=\"•\"/>\
         </text:list-style>"
            .to_string()
    }

    /// The labels a rendered list drew, in document order.
    fn markers(html: &str) -> Vec<String> {
        html.split("class=\"of-bu\"")
            .skip(1)
            .filter_map(|chunk| {
                let rest = chunk.split_once('>')?.1;
                Some(rest.split_once('<')?.0.trim().to_string())
            })
            .collect()
    }

    #[test]
    fn nested_list_labels_follow_their_levels_in_document_order() {
        let html = odt(
            "list",
            &styles(&list_styles()),
            &content(
                "",
                "<text:list text:style-name=\"L1\">\
                 <text:list-item><text:p>one</text:p>\
                 <text:list><text:list-item><text:p>inner</text:p></text:list-item>\
                 <text:list-item><text:p>inner2</text:p></text:list-item></text:list>\
                 </text:list-item>\
                 <text:list-item><text:p>two</text:p></text:list-item></text:list>",
            ),
        )
        .html();
        // A nested list with no style of its own inherits the enclosing one's, and
        // the level is the nesting depth.
        assert_eq!(markers(&html), vec!["1.", "a)", "b)", "2."], "{html}");
        assert!(html.contains("class=\"of-p of-li\""), "{html}");
    }

    #[test]
    fn a_list_header_draws_no_number_and_an_item_holding_only_a_list_consumes_none() {
        let html = odt(
            "hdr2",
            &styles(&list_styles()),
            &content(
                "",
                "<text:list text:style-name=\"L1\">\
                 <text:list-header><text:p>intro</text:p></text:list-header>\
                 <text:list-item><text:p>one</text:p></text:list-item>\
                 <text:list-item>\
                 <text:list><text:list-item><text:p>inner</text:p></text:list-item></text:list>\
                 </text:list-item>\
                 <text:list-item><text:p>two</text:p></text:list-item></text:list>",
            ),
        )
        .html();
        // The header is indented like an item but takes no number, and neither does
        // the item that draws nothing of its own.
        assert_eq!(markers(&html), vec!["1.", "a)", "2."], "{html}");
        assert!(html.contains("intro"), "the header's text still renders");
    }

    #[test]
    fn a_start_value_restarts_a_list_and_continue_numbering_resumes_one() {
        let html = odt(
            "start",
            &styles(&list_styles()),
            &content(
                "",
                "<text:list text:style-name=\"L1\">\
                 <text:list-item><text:p>a</text:p></text:list-item>\
                 <text:list-item text:start-value=\"5\"><text:p>b</text:p></text:list-item>\
                 </text:list>",
            ),
        )
        .html();
        assert_eq!(markers(&html), vec!["1.", "5."], "{html}");

        let body = |cont: &str| {
            format!(
                "<text:list text:style-name=\"L1\">\
                 <text:list-item><text:p>a</text:p></text:list-item></text:list>\
                 <text:list text:style-name=\"L1\"{cont}>\
                 <text:list-item><text:p>b</text:p></text:list-item></text:list>"
            )
        };
        let resumed = odt(
            "cont",
            &styles(&list_styles()),
            &content("", &body(" text:continue-numbering=\"true\"")),
        )
        .html();
        assert_eq!(markers(&resumed), vec!["1.", "2."], "{resumed}");
        // A new list restarts unless it says otherwise.
        let restarted = odt("cont2", &styles(&list_styles()), &content("", &body(""))).html();
        assert_eq!(markers(&restarted), vec!["1.", "1."], "{restarted}");
    }

    #[test]
    fn a_bullet_level_draws_its_character() {
        let html = odt(
            "bullet",
            &styles(&list_styles()),
            &content(
                "",
                "<text:list text:style-name=\"L1\"><text:list-item>\
                 <text:list><text:list-item>\
                 <text:list><text:list-item><text:p>deep</text:p></text:list-item></text:list>\
                 </text:list-item></text:list></text:list-item></text:list>",
            ),
        )
        .html();
        assert_eq!(markers(&html), vec!["•"], "{html}");
    }

    // ── runs ────────────────────────────────────────────────────────────────

    #[test]
    fn tabs_spaces_and_line_breaks_survive_as_themselves() {
        let html = body(
            "runs",
            "<text:p>a<text:tab/>b<text:s text:c=\"3\"/>c<text:s/>d<text:line-break/>e</text:p>",
        )
        .html();
        // A literal U+0009, because only a real tab is subject to `tab-size`.
        assert!(html.contains("a\tb   c d"), "{html}");
        // A line break never takes the page-break class.
        assert!(html.contains("<br>"), "{html}");
        assert!(!html.contains("<br class"), "{html}");
    }

    #[test]
    fn a_hyperlink_is_whitelisted_before_it_reaches_an_attribute() {
        let html = body(
            "links",
            "<text:p><text:a xlink:href=\"https://example.org/?a=1&amp;b=2\">good</text:a>\
             <text:a xlink:href=\"javascript:alert(1)\">bad</text:a></text:p>",
        )
        .html();
        assert!(
            html.contains("href=\"https://example.org/?a=1&amp;b=2\""),
            "{html}"
        );
        // A rejected destination renders its text without an `<a>` rather than
        // falling back to anything.
        assert!(!html.contains("javascript"), "{html}");
        assert!(html.contains("bad"), "{html}");
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
    }

    #[test]
    fn a_bookmark_becomes_an_anchor_the_document_can_be_linked_to() {
        let html = body(
            "marks",
            "<text:p><text:bookmark-start text:name=\"café Widget\"/>x\
             <text:bookmark-end text:name=\"café Widget\"/></text:p>\
             <text:bookmark text:name=\"plain\"/>",
        )
        .html();
        // Folded to what an id may hold — `é` and the space are both replaced —
        // and prefixed so a document's own name cannot collide with an id this
        // renderer generates. The end marker is zero-width and adds nothing.
        assert!(
            html.contains("<span id=\"of-bm-caf--Widget\"></span>"),
            "{html}"
        );
        assert!(html.contains("<span id=\"of-bm-plain\"></span>"), "{html}");
    }

    #[test]
    fn tracked_deletions_and_annotations_are_not_document_text() {
        let html = body(
            "tracked",
            "<text:tracked-changes><text:changed-region>\
             <text:deletion><text:p>gone</text:p></text:deletion>\
             </text:changed-region></text:tracked-changes>\
             <text:p>kept<office:annotation><text:p>comment</text:p></office:annotation>\
             <text:change-start/><text:change-end/></text:p>",
        )
        .html();
        assert!(html.contains("kept"), "{html}");
        // A preview that shows a deletion is showing a document that does not
        // exist, and a comment is an annotation *about* the document.
        assert!(!html.contains("gone"), "{html}");
        assert!(!html.contains("comment"), "{html}");
    }

    #[test]
    fn a_field_and_an_index_keep_their_cached_text() {
        let html = body(
            "fields",
            "<text:p><text:page-number>7</text:page-number> of \
             <text:page-count>9</text:page-count></text:p>\
             <text:table-of-content><text:index-body>\
             <text:p>Chapter one</text:p></text:index-body></text:table-of-content>",
        )
        .html();
        // The producer already resolved both; the cached result is the only text
        // that exists here.
        assert!(html.contains("7 of 9"), "{html}");
        assert!(html.contains("Chapter one"), "{html}");
    }

    // ── footnotes ───────────────────────────────────────────────────────────

    #[test]
    fn notes_get_a_marker_in_the_text_and_a_block_at_the_end() {
        let doc = body(
            "notes",
            "<text:p>body<text:note text:note-class=\"footnote\">\
             <text:note-citation>1</text:note-citation>\
             <text:note-body><text:p>the note</text:p></text:note-body></text:note></text:p>",
        )
        .doc();
        let html = &doc.html;
        // The marker links to the note and the note links back.
        assert!(html.contains("id=\"of-fn-ref-1\""), "{html}");
        assert!(html.contains("href=\"#of-fn-1\""), "{html}");
        assert!(html.contains("class=\"of-fnotes\""), "{html}");
        assert!(html.contains("id=\"of-fn-1\""), "{html}");
        assert!(html.contains("href=\"#of-fn-ref-1\""), "{html}");
        assert!(html.contains("the note"), "{html}");
        // A reference is a superscript in every producer's output.
        assert!(html.contains("vertical-align:super"), "{html}");
        // Where the notes sit is visible in the column itself, so it is not a
        // footer line.
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
    }

    #[test]
    fn endnotes_are_numbered_apart_from_footnotes() {
        let html = body(
            "notes2",
            "<text:p>a<text:note text:note-class=\"footnote\">\
             <text:note-body><text:p>f</text:p></text:note-body></text:note>\
             b<text:note text:note-class=\"endnote\">\
             <text:note-body><text:p>e</text:p></text:note-body></text:note></text:p>",
        )
        .html();
        assert!(html.contains("id=\"of-fn-1\""), "{html}");
        assert!(html.contains("id=\"of-en-1\""), "{html}");
        // No citation of its own: a footnote counts in decimals, an endnote in
        // lower-case roman.
        assert!(html.contains(">i</a>"), "{html}");
    }

    // ── search terms ────────────────────────────────────────────────────────

    #[test]
    fn terms_are_marked_in_the_first_paint_and_the_best_one_is_named() {
        let f = body("terms", "<text:p>the runner was running fast</text:p>");
        let doc = super::super::render(f.path(), None, &["run".to_string()]).expect("render");
        // Keyed through the same porter stemming FTS5 uses, so `run` marks
        // `running` and not `fast`.
        assert!(doc.html.contains("<mark class=\"preview-hl\""), "{}", doc.html);
        assert!(doc.html.contains("id=\"pm-"), "{}", doc.html);
        let id = doc.best_mark_id.expect("a match names a jump target");
        assert!(doc.html.contains(&format!("id=\"{id}\"")), "{id}");
    }

    // ── caps ────────────────────────────────────────────────────────────────

    /// Renders with a byte cap and an image budget the real entry point would not
    /// use, so the refusal paths are reachable without megabytes of fixture.
    fn render_capped(f: &Fixture, cap: usize) -> OfficeDoc {
        let mut notes = Notes::new();
        let package = super::super::pkg::open(f.path(), &mut notes).expect("open");
        super::render_with(package, notes, &[], cap, MediaBudget::new()).expect("render")
    }

    #[test]
    fn the_byte_cap_truncates_and_still_closes_every_tag() {
        let long: String = (0..400)
            .map(|i| format!("<text:p>paragraph {i} of café</text:p>"))
            .collect();
        let f = body("cap", &long);
        let doc = render_capped(&f, 2_048);
        assert!(doc.truncated, "the cap must be reached");
        // Never unbalanced: the writer closes what it opened, whatever the cap did.
        assert!(doc.html.contains("class=\"of-page\""), "{}", doc.html);
        assert!(doc.html.trim_end().ends_with("</div>"), "{}", doc.html);
        assert_eq!(
            doc.html.matches("<div").count(),
            doc.html.matches("</div>").count(),
            "{}",
            doc.html
        );
        assert_eq!(
            doc.html.matches("<p ").count(),
            doc.html.matches("</p>").count(),
            "{}",
            doc.html
        );
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn a_text_document_indexes_as_one_unnamed_section() {
        let f = body("index-one", "<text:p>café</text:p><text:h>naïve</text:h>");
        let sections = crate::office::odf::extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "");
        assert!(sections[0].text.contains("café"), "{:?}", sections[0].text);
        assert!(sections[0].text.contains("naïve"), "{:?}", sections[0].text);
    }

    #[test]
    fn tabs_and_line_breaks_separate_indexed_words() {
        // `text:tab` and `text:line-break` are elements, never characters. Emitting
        // nothing for them fused the words on either side into one token.
        let f = body(
            "index-tab",
            "<text:p>café<text:tab/>naïve</text:p>\
             <text:p>one<text:line-break/>two</text:p>",
        );
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("café naïve"), "{text:?}");
        assert!(text.contains("one two"), "{text:?}");
    }

    #[test]
    fn table_cells_do_not_fuse_into_one_token() {
        let f = body(
            "index-cells",
            "<table:table table:name=\"T\"><table:table-row>\
             <table:table-cell><text:p>café</text:p></table:table-cell>\
             <table:table-cell><text:p>naïve</text:p></table:table-cell>\
             </table:table-row></table:table>",
        );
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(!text.contains("cafénaïve"), "{text:?}");
    }

    #[test]
    fn deleted_text_and_comments_are_not_indexed() {
        // Neither is drawn: `text:tracked-changes` holds the deleted side of a
        // change and `office:annotation` a comment Word and Writer both keep out of
        // the page. A hit in either could never be highlighted.
        let f = body(
            "index-skip",
            "<text:tracked-changes><text:changed-region><text:deletion>\
             <text:p>deleted-café</text:p></text:deletion></text:changed-region>\
             </text:tracked-changes>\
             <text:p>kept<office:annotation><text:p>comment-naïve</text:p>\
             </office:annotation></text:p>",
        );
        let text = &crate::office::odf::extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("kept"), "{text:?}");
        assert!(!text.contains("deleted-café"), "{text:?}");
        assert!(!text.contains("comment-naïve"), "{text:?}");
    }
}
