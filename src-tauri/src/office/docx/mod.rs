//! docx → styled HTML, one continuous page column.
//!
//! Word's own Web Layout is the model: the page keeps its real `w:sectPr` width
//! and margins, and the text flows down one column without pagination. Nothing
//! here measures text, so page and line counts do not match Word's — see the
//! fidelity notes in the plan.
//!
//! A document renders whole, so there is one section and `sections` is empty: the
//! frontend's section strip no-ops below two, and inventing "Page 1…n" entries
//! would be a pagination this renderer does not do.
//!
//! Only a missing main part is fatal. A stylesheet, numbering table or theme that
//! cannot be read costs the document its formatting and nothing else, so each of
//! those degradations is a line in the notes footer and the text still renders.

mod body;
mod draw;
mod notes;
mod numbering;
mod style;
mod table;

use super::docshape::{self, Page, MAX_MARGIN_PX, MAX_PAGE_PX, MIN_COLUMN_PX, MIN_PAGE_PX};
use super::drawingml::theme::Theme;
use super::emit::{self, Notes};
use super::highlight::{Marker, Terms};
use super::html::{attr, dxa_to_px, Writer};
use super::media::{MediaBudget, MediaCache};
use super::pkg::{self, Budget, Zip};
use super::text::Section;
use super::xml::{child, elems};
use super::{opc, xml, OfficeDoc, Shape};
use numbering::Numbering;
use roxmltree::Node;
use style::Styles;

/// Byte cap for the emitted body HTML. A long report is one column, so the cap is
/// larger than a slide's.
pub const HTML_CAP: usize = 8 * 1024 * 1024;

/// `w:defaultTabStop` when `settings.xml` states none: Word's own default, half
/// an inch.
const DEFAULT_TAB_DXA: i64 = 720;
const MAX_TAB_DXA: i64 = 5760;

const NOTE_STYLES: &str = "Stylesheet unreadable — text styles missing";
const NOTE_NUMBERING: &str = "Numbering definitions unreadable";
const NOTE_THEME: &str = "Theme unreadable — colours and fonts missing";
const NOTE_BODY: &str = "Document body unreadable";

/// Everything the body walk needs. Grouped so the emitters can take disjoint
/// mutable borrows of the pieces they touch.
pub struct Ctx<'a> {
    /// The package and the media path through it: a drawing resolves a
    /// relationship to a part, reads it, and encodes it into a `data:` URI. Four
    /// separate borrows because `MediaCache::get` needs all of them at once.
    pub zip: &'a mut Zip,
    pub budget: &'a mut Budget,
    pub media: &'a mut MediaCache,
    pub mb: &'a mut MediaBudget,
    /// Relationships of the document part, and the part itself: an `r:embed` or
    /// `r:id` on a drawing names one of these, and its target resolves against
    /// the document's own directory.
    pub rels: &'a opc::Rels,
    pub part: &'a str,
    /// Width of the text column in px — the widest box a drawing can occupy
    /// before it would push the page open. See `draw::fit`.
    pub column_px: f32,
    /// Graphics emitted so far, images and placeholders alike.
    pub images: usize,
    pub styles: &'a Styles,
    /// Mutable because list counters advance as the walk goes: one `Numbering`
    /// serves exactly one forward pass over the body.
    pub numbering: &'a mut Numbering,
    pub theme: &'a Theme,
    /// The CSS font stack `.of-page` states, so a run that resolves to the same
    /// face can leave it unsaid. See `body::text_run`.
    pub default_font: Option<&'a str>,
    pub terms: &'a Terms,
    pub marker: &'a mut Marker,
    pub notes: &'a mut Notes,
    /// Footnote and endnote bodies, by the id a reference names. Empty when the
    /// document carries neither part.
    pub note_store: &'a notes::Store<'a>,
    /// The notes referenced so far, in first-reference order: what the tail block
    /// holds, and the number each marker shows.
    pub used_notes: Vec<notes::Ref>,
    /// True while the tail block is being written. A reference inside a note is
    /// dropped rather than followed — see the module note in `notes.rs`.
    pub in_note: bool,
    /// Finished HTML for blocks that cannot live inside the paragraph currently
    /// being built (a text box's own paragraphs), flushed straight after it. Each
    /// string comes from a [`Writer`] of its own, i.e. is already escaped.
    pub pending: Vec<String>,
    /// Text boxes hoisted so far, and the nesting of the one being rendered.
    pub boxes: usize,
    pub box_depth: usize,
    /// Bookmark ids anchored so far.
    pub bookmarks: usize,
    /// Paragraphs emitted so far — the walk's own bound.
    pub paras: usize,
    /// Table cells planned so far, across every table in the document. A per-table
    /// cap alone would let a thousand tables of a thousand cells each through.
    pub cells: usize,
    /// `w:pStyle` of the paragraph immediately before this one, for
    /// `w:contextualSpacing`. The outer `None` is "nothing precedes it" — the
    /// start of the body — which is distinct from a paragraph that names no style
    /// and therefore takes the document's default.
    pub prev_style: Option<Option<String>>,
}

pub fn render(path: &str, section: Option<u32>, terms: &[String]) -> Result<OfficeDoc, String> {
    render_capped(path, section, terms, HTML_CAP)
}

fn render_capped(
    path: &str,
    section: Option<u32>,
    terms: &[String],
    html_cap: usize,
) -> Result<OfficeDoc, String> {
    render_with(path, section, terms, html_cap, MediaBudget::new())
}

/// The image budget is a parameter only so tests can reach its refusal paths
/// without generating 24 MB of pictures; `render` uses `MediaBudget::new`.
fn render_with(
    path: &str,
    _section: Option<u32>,
    terms: &[String],
    html_cap: usize,
    mut mb: MediaBudget,
) -> Result<OfficeDoc, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();
    let mut notes = Notes::new();

    let doc_part = opc::root_part(&mut zip, &mut budget, "word/document.xml");
    let doc_xml = pkg::read_entry(&mut zip, &doc_part, &mut budget)?
        .ok_or_else(|| format!("docx: missing document part ({doc_part})"))?;
    let rels = opc::read_rels(&mut zip, &doc_part, &mut budget).unwrap_or_default();

    // A companion part that cannot be *read* — corrupt, or past the package
    // budget — is indistinguishable here from one the package does not contain.
    // Only the main part is worth failing over, and it is already read: a budget
    // stop this late means a package tens of megabytes larger than the document
    // it holds.
    let read = |zip: &mut pkg::Zip, budget: &mut Budget, part: &str| {
        pkg::read_entry(zip, part, budget).unwrap_or(None)
    };

    let theme_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/theme", "theme/theme1.xml"),
    );
    let theme = match theme_xml.as_deref().map(Theme::parse) {
        Some(Ok(t)) => t,
        // A document with no theme part at all references no theme colours
        // either, so its absence is not worth a note; a theme that is present and
        // unreadable is.
        Some(Err(_)) => {
            notes.add(NOTE_THEME);
            Theme::default()
        }
        None => Theme::default(),
    };

    let styles_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/styles", "styles.xml"),
    );
    // `Styles::parse` degrades to `empty()` instead of failing, so the XML is
    // probed here purely to be able to say so in the notes. Word always writes a
    // stylesheet, which is why a missing one is reported too.
    let styles = match styles_xml.as_deref() {
        Some(x) if xml::parse(x).is_ok() => Styles::parse(x, &theme),
        _ => {
            notes.add(NOTE_STYLES);
            Styles::empty()
        }
    };

    let numbering_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/numbering", "numbering.xml"),
    );
    // Unlike the stylesheet, an absent numbering part is the normal state of a
    // document with no lists, so only a present-but-broken one is reported.
    let mut numbering = match numbering_xml.as_deref() {
        Some(x) if xml::parse(x).is_ok() => Numbering::parse(x),
        Some(_) => {
            notes.add(NOTE_NUMBERING);
            Numbering::empty()
        }
        None => Numbering::empty(),
    };

    let settings_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/settings", "settings.xml"),
    );
    let tab_px = default_tab_px(settings_xml.as_deref());

    // The note parts are read up front because their trees have to outlive the
    // walk: a reference is a marker plus an index into them, and the bodies are
    // rendered after the page. A part that is referenced but unreadable is a
    // degradation the reader is told about, at the point a reference misses.
    let footnotes_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/footnotes", "footnotes.xml"),
    );
    let endnotes_xml = read(
        &mut zip,
        &mut budget,
        &side_part(&rels, &doc_part, "/endnotes", "endnotes.xml"),
    );
    let footnotes = footnotes_xml.as_deref().and_then(|x| xml::parse(x).ok());
    let endnotes = endnotes_xml.as_deref().and_then(|x| xml::parse(x).ok());
    let note_store = notes::Store::parse(
        footnotes.as_ref().map(|d| d.root_element()),
        endnotes.as_ref().map(|d| d.root_element()),
    );

    let parsed = xml::parse(&doc_xml)?;
    let root = parsed.root_element();
    let body_node = child(root, "body");
    let page = body_node.map(page_geometry).unwrap_or_default();

    let query = Terms::new(terms);
    let mut hl = Marker::new();
    let mut media = MediaCache::new();
    let mut w = Writer::new(html_cap);
    // The text column: what a drawing's extent is scaled to fit.
    let column_px = (page.width - page.left - page.right).max(MIN_COLUMN_PX);

    // The frontend's doc variant keys its selection and its page geometry off
    // `.of-page`; see the contract in `srcdoc.ts`.
    w.open("div", &attr("class", "of-page"));
    match body_node {
        Some(b) => {
            let mut ctx = Ctx {
                zip: &mut zip,
                budget: &mut budget,
                media: &mut media,
                mb: &mut mb,
                rels: &rels,
                part: &doc_part,
                column_px,
                images: 0,
                styles: &styles,
                numbering: &mut numbering,
                theme: &theme,
                default_font: styles.defaults().run.font.as_deref(),
                terms: &query,
                marker: &mut hl,
                notes: &mut notes,
                note_store: &note_store,
                used_notes: Vec::new(),
                in_note: false,
                pending: Vec::new(),
                boxes: 0,
                box_depth: 0,
                bookmarks: 0,
                paras: 0,
                cells: 0,
                prev_style: None,
            };
            body::walk(&mut ctx, &mut w, b, 0, 0);
            // Inside `.of-page`: the notes belong to the page column, at the end
            // of it, and the walk is over so every reference is in.
            notes::emit_tail(&mut ctx, &mut w);
        }
        None => notes.add(NOTE_BODY),
    }
    w.close();
    // The image path explains its own degradations, in one line per class of
    // failure however many pictures hit it.
    for n in mb.notes() {
        notes.add(n);
    }

    let truncated = w.truncated();
    let d = styles.defaults();
    let page_css = docshape::page_css(
        &page,
        d.run.font.as_deref(),
        style::size_pt(&d.run),
        tab_px,
    );
    let html = emit::wrap_style(docshape::BASE_CSS, &page_css, w.finish());

    Ok(OfficeDoc {
        html,
        shape: Shape::Doc,
        // One section: a docx renders whole. The frontend's section strip no-ops
        // below two entries, so an empty list is the honest answer rather than a
        // pagination this renderer cannot do.
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

// ── content index ────────────────────────────────────────────────────────────

/// Flat text for the content index: one section, the main document part.
///
/// The renderer draws exactly this part — headers, footers and comments are not
/// drawn (see the notes footer), so indexing them would put hits in a document
/// where no highlight can ever land. Footnote bodies *are* drawn, in the tail
/// block, but they live in `footnotes.xml` and are left for a later pass rather
/// than half-indexed.
pub(super) fn extract_sections(path: &str) -> Result<Vec<Section>, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();
    let part = opc::root_part(&mut zip, &mut budget, "word/document.xml");
    let doc_xml = pkg::read_entry(&mut zip, &part, &mut budget)?
        .ok_or_else(|| format!("docx: missing document part ({part})"))?;
    let text = xml::xml_text(&doc_xml, &["p"], &["t"])?;
    Ok(vec![Section::new("", text)])
}

/// A companion part of the main document: the relationship of `kind` if the
/// document declares one, else `conventional` resolved next to the document part.
/// The conventional name is a convention and not a rule — `opc::root_part` already
/// has to be prepared for a package that names its main part something else, and
/// the parts beside it move with it.
fn side_part(rels: &opc::Rels, owner: &str, kind: &str, conventional: &str) -> String {
    opc::part_by_kind(rels, owner, kind)
        .or_else(|| opc::resolve_target(owner, conventional))
        .unwrap_or_else(|| conventional.to_string())
}

// ── page geometry ────────────────────────────────────────────────────────────

/// The page box from the document-level `w:sectPr` — the last child of `w:body`,
/// which is the section the tail of the document belongs to and the only one a
/// single-column preview can honour.
fn page_geometry(body: Node) -> Page {
    let Some(sect) = elems(body)
        .filter(|n| n.tag_name().name() == "sectPr")
        .last()
    else {
        return Page::default();
    };
    let mut p = Page::default();
    if let Some(sz) = child(sect, "pgSz") {
        let px = |name: &str| {
            xml::attr_i64(sz, name)
                .map(dxa_to_px)
                .filter(|v| v.is_finite() && *v >= MIN_PAGE_PX && *v <= MAX_PAGE_PX)
        };
        // Both or neither: half a stated page size is not a page, and mixing one
        // real dimension with a Letter default gives an aspect nothing wrote.
        if let (Some(w), Some(h)) = (px("w"), px("h")) {
            p.width = w;
            p.height = h;
        }
    }
    if let Some(mar) = child(sect, "pgMar") {
        let limit = |extent: f32| ((extent - MIN_COLUMN_PX) / 2.0).clamp(0.0, MAX_MARGIN_PX);
        let px = |name: &str, extent: f32, fallback: f32| {
            xml::attr_i64(mar, name)
                .map(dxa_to_px)
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, limit(extent)))
                .unwrap_or(fallback)
        };
        p.left = px("left", p.width, p.left);
        p.right = px("right", p.width, p.right);
        p.top = px("top", p.height, p.top);
        p.bottom = px("bottom", p.height, p.bottom);
    }
    p
}

/// `w:defaultTabStop` from `settings.xml`, in px.
fn default_tab_px(settings_xml: Option<&str>) -> f32 {
    let dxa = settings_xml
        .and_then(|x| xml::parse(x).ok())
        .and_then(|doc| {
            child(doc.root_element(), "defaultTabStop").and_then(|n| xml::attr_i64(n, "val"))
        })
        .filter(|v| *v > 0)
        .map(|v| v.min(MAX_TAB_DXA))
        .unwrap_or(DEFAULT_TAB_DXA);
    dxa_to_px(dxa)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::office::pkg::TestPkg;

    const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
         xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
         xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
         xmlns:v=\"urn:schemas-microsoft-com:vml\" \
         xmlns:o=\"urn:schemas-microsoft-com:office:office\" \
         xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\" \
         xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"";

    /// A docx package on disk. Text parts and media parts are separate arguments
    /// because a media part is not UTF-8.
    struct Fixture(TestPkg);

    impl Fixture {
        fn new(tag: &str, entries: &[(&str, String)]) -> Fixture {
            Fixture::with_media(tag, entries, &[])
        }

        fn with_media(
            tag: &str,
            entries: &[(&str, String)],
            media: &[(&str, Vec<u8>)],
        ) -> Fixture {
            let parts: Vec<(&str, Vec<u8>)> = entries
                .iter()
                .map(|(n, b)| (*n, b.as_bytes().to_vec()))
                .chain(media.iter().map(|(n, b)| (*n, b.clone())))
                .collect();
            Fixture(TestPkg::new(tag, &parts))
        }

        fn path(&self) -> &str {
            self.0.path()
        }

        fn render(&self) -> OfficeDoc {
            super::render(self.path(), None, &[]).expect("render")
        }
    }

    /// `(rId, relationship-kind, target)`.
    fn rels(items: &[(&str, &str, &str)]) -> String {
        let body: String = items
            .iter()
            .map(|(id, kind, target)| {
                format!(
                    "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}\" Target=\"{target}\"/>"
                )
            })
            .collect();
        format!(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>"
        )
    }

    /// A body plus a US Letter section: 12240 dxa = 816px, 15840 = 1056px, and a
    /// 1440 dxa margin = 96px on all four sides.
    fn document(body: &str) -> String {
        format!(
            "<w:document {NS}><w:body>{body}\
             <w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/>\
             <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/>\
             </w:sectPr></w:body></w:document>"
        )
    }

    fn styles_xml() -> String {
        format!(
            "<w:styles {NS}>\
             <w:docDefaults><w:rPrDefault><w:rPr>\
             <w:rFonts w:ascii=\"Widget Sans\"/><w:sz w:val=\"22\"/></w:rPr></w:rPrDefault>\
             </w:docDefaults>\
             <w:style w:type=\"paragraph\" w:styleId=\"Normal\" w:default=\"1\">\
             <w:name w:val=\"Normal\"/></w:style>\
             <w:style w:type=\"paragraph\" w:styleId=\"Heading1\">\
             <w:name w:val=\"Heading 1\"/><w:rPr><w:sz w:val=\"32\"/></w:rPr></w:style>\
             </w:styles>"
        )
    }

    /// One `w:t` run.
    fn run(text: &str) -> String {
        format!("<w:r><w:t>{text}</w:t></w:r>")
    }

    fn para(ppr: &str, runs: &str) -> String {
        let pr = if ppr.is_empty() {
            String::new()
        } else {
            format!("<w:pPr>{ppr}</w:pPr>")
        };
        format!("<w:p>{pr}{runs}</w:p>")
    }

    /// A package whose document body is `body`; `extra` adds or replaces parts.
    fn docx(tag: &str, body: &str, extra: &[(&str, String)]) -> Fixture {
        let mut parts = vec![
            (
                "_rels/.rels",
                rels(&[("rId1", "officeDocument", "word/document.xml")]),
            ),
            ("word/document.xml", document(body)),
            ("word/styles.xml", styles_xml()),
        ];
        parts.extend(extra.iter().cloned());
        Fixture::new(tag, &parts)
    }

    /// The markup without the stylesheet. Every structural class is named in the
    /// `<style>` block by definition, so an assertion about what the *page*
    /// contains has to look past it.
    fn body_html(doc: &OfficeDoc) -> &str {
        doc.html
            .split_once("</style>")
            .map(|(_, b)| b)
            .unwrap_or(&doc.html)
    }

    #[test]
    fn a_heading_style_becomes_a_heading_element() {
        let f = docx(
            "heading",
            &para("<w:pStyle w:val=\"Heading1\"/>", &run("Heading 1")),
            &[],
        );
        let doc = f.render();
        let html = body_html(&doc);
        assert!(html.contains("<h1 class=\"of-p\""), "{html}");
        assert!(html.contains("</h1>"), "{html}");
        // 32 half-points = 16pt = 21.33px, from the style's own `w:rPr`.
        assert!(html.contains("font-size:21.33px"), "{html}");
        assert!(matches!(doc.shape, Shape::Doc));
    }

    #[test]
    fn a_bold_run_lands_on_a_span() {
        let f = docx(
            "bold",
            &para(
                "",
                &format!(
                    "{}<w:r><w:rPr><w:b/></w:rPr><w:t>naïve</w:t></w:r>",
                    run("café ")
                ),
            ),
            &[],
        );
        let html = body_html(&f.render()).to_string();
        assert!(html.contains("<span style=\"font-weight:700;\">naïve</span>"), "{html}");
        // The unformatted run needs no span at all.
        assert!(html.contains(">café "), "{html}");
    }

    #[test]
    fn document_text_is_escaped() {
        let f = docx("escape", &para("", &run("a &lt;b&gt; &amp; c")), &[]);
        let html = body_html(&f.render()).to_string();
        assert!(html.contains("a &lt;b&gt; &amp; c"), "{html}");
        // The paragraph's own tags are the only markup in the output.
        assert_eq!(html.matches("<b>").count(), 0, "{html}");
    }

    #[test]
    fn page_geometry_comes_from_the_section() {
        let doc = docx("page", &para("", &run("café")), &[]).render();
        let (w, px, py) = doc.page.expect("a doc reports a page");
        assert_eq!((w, px, py), (816.0, 96.0, 96.0));
        assert!(w > 0.0 && px > 0.0 && py > 0.0);
        assert!(doc.natural.is_none());
        // Symmetric margins ride in the tuple, so no padding override is needed.
        assert!(!doc.html.contains("padding:96px"), "{}", doc.html);
    }

    #[test]
    fn asymmetric_margins_get_an_exact_padding_rule() {
        // 2880 dxa = 192px on the left only: the three-tuple cannot say that, so
        // the renderer's own stylesheet states all four sides.
        let body = format!(
            "{}<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/>\
             <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"2880\"/></w:sectPr>",
            para("", &run("café"))
        );
        let f = Fixture::new(
            "asym",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "word/document.xml")]),
                ),
                (
                    "word/document.xml",
                    format!("<w:document {NS}><w:body>{body}</w:body></w:document>"),
                ),
                ("word/styles.xml", styles_xml()),
            ],
        );
        let doc = f.render();
        assert_eq!(doc.page.map(|p| p.1), Some(192.0));
        assert!(
            doc.html.contains(".of-page{padding:96px 96px 96px 192px;}"),
            "{}",
            doc.html
        );
    }

    #[test]
    fn a_document_without_styles_still_renders() {
        let f = Fixture::new(
            "nostyles",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "word/document.xml")]),
                ),
                ("word/document.xml", document(&para("", &run("café")))),
            ],
        );
        let doc = super::render(f.path(), None, &[]).expect("a missing stylesheet is not fatal");
        assert!(body_html(&doc).contains("café"), "{}", doc.html);
        assert!(doc.notes.iter().any(|n| n == NOTE_STYLES), "{:?}", doc.notes);
    }

    #[test]
    fn a_missing_document_part_is_fatal() {
        let f = Fixture::new(
            "nodoc",
            &[(
                "_rels/.rels",
                rels(&[("rId1", "officeDocument", "word/document.xml")]),
            )],
        );
        let err = super::render(f.path(), None, &[]).expect_err("no document part");
        assert!(err.contains("word/document.xml"), "{err}");
    }

    #[test]
    fn a_two_level_list_numbers_in_document_order() {
        let numbering = format!(
            "<w:numbering {NS}><w:abstractNum w:abstractNumId=\"0\">\
             <w:lvl w:ilvl=\"0\"><w:start w:val=\"1\"/><w:numFmt w:val=\"decimal\"/>\
             <w:lvlText w:val=\"%1.\"/><w:pPr><w:ind w:left=\"720\" w:hanging=\"360\"/></w:pPr></w:lvl>\
             <w:lvl w:ilvl=\"1\"><w:start w:val=\"1\"/><w:numFmt w:val=\"decimal\"/>\
             <w:lvlText w:val=\"%1.%2.\"/><w:pPr><w:ind w:left=\"1440\" w:hanging=\"360\"/></w:pPr></w:lvl>\
             </w:abstractNum><w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num></w:numbering>"
        );
        let item = |ilvl: &str, text: &str| {
            para(
                &format!("<w:numPr><w:ilvl w:val=\"{ilvl}\"/><w:numId w:val=\"1\"/></w:numPr>"),
                &run(text),
            )
        };
        let f = docx(
            "list",
            &format!("{}{}", item("0", "Widget"), item("1", "café")),
            &[("word/numbering.xml", numbering)],
        );
        let html = body_html(&f.render()).to_string();
        assert!(
            html.contains("class=\"of-bu\" style=\"width:24px;\">1.</span>"),
            "{html}"
        );
        assert!(html.contains(">1.1.</span>"), "{html}");
        // The level's own indent reaches the paragraph: 720 dxa = 48px, less the
        // 360 dxa (24px) hang the marker occupies. The second level's 1440 dxa is
        // 96px, so 72px.
        assert!(html.contains("margin-left:24px"), "{html}");
        assert!(html.contains("margin-left:72px"), "{html}");
        assert!(html.contains("<span class=\"of-tx\">Widget</span>"), "{html}");
    }

    #[test]
    fn a_tracked_deletion_never_reaches_the_output() {
        let body = para(
            "",
            &format!(
                "{}<w:del><w:r><w:delText>removed</w:delText></w:r>\
                 <w:r><w:t>also removed</w:t></w:r></w:del>{}",
                run("café "),
                run(" naïve")
            ),
        );
        let doc = docx("del", &body, &[]).render();
        let html = body_html(&doc).to_string();
        assert!(html.contains("café "), "{html}");
        assert!(html.contains(" naïve"), "{html}");
        assert!(!html.contains("removed"), "{html}");
    }

    #[test]
    fn a_field_code_is_not_document_text() {
        // Word writes the code and its cached result as sibling runs; only the
        // result is text the reader ever saw.
        let body = para(
            "",
            "<w:r><w:instrText>PAGE \\* MERGEFORMAT</w:instrText></w:r><w:r><w:t>7</w:t></w:r>",
        );
        let html = body_html(&docx("field", &body, &[]).render()).to_string();
        assert!(html.contains(">7<"), "{html}");
        assert!(!html.contains("MERGEFORMAT"), "{html}");
    }

    #[test]
    fn a_page_break_is_drawn_as_a_boundary() {
        let body = para(
            "",
            &format!(
                "{}<w:r><w:br w:type=\"page\"/></w:r>{}<w:r><w:br/></w:r>",
                run("café"),
                run("naïve")
            ),
        );
        let html = body_html(&docx("break", &body, &[]).render()).to_string();
        assert!(html.contains("<br class=\"of-pb\">"), "{html}");
        // A plain `w:br` is a line break and takes no class.
        assert!(html.contains("<br>"), "{html}");
    }

    #[test]
    fn a_tab_run_emits_a_literal_tab() {
        // Not `&#9;` and not spaces: `tab-size` only acts on U+0009.
        let body = para(
            "",
            &format!("{}<w:r><w:tab/></w:r>{}", run("café"), run("naïve")),
        );
        let doc = docx("tab", &body, &[]).render();
        assert!(body_html(&doc).contains("café\tnaïve"), "{}", doc.html);
        // The default stop is 720 dxa = 48px.
        assert!(doc.html.contains("tab-size:48px"), "{}", doc.html);
    }

    #[test]
    fn a_document_renders_whole_with_no_sections() {
        let doc = docx("sections", &para("", &run("café")), &[]).render();
        assert!(doc.sections.is_empty(), "{:?}", doc.sections);
        assert_eq!(doc.section, 0);
        assert!(!doc.truncated);
    }

    #[test]
    fn a_tripped_cap_truncates_but_stays_balanced() {
        let f = docx(
            "cap",
            &para("", &run("café naïve Widget example.org")).repeat(4),
            &[],
        );
        let doc = render_capped(f.path(), None, &[], 64).expect("render");
        assert!(doc.truncated);
        let html = &doc.html;
        assert!(html.contains("office-trunc"), "{html}");
        assert_eq!(
            html.matches("<div").count(),
            html.matches("</div>").count(),
            "{html}"
        );
        assert_eq!(html.matches("<p ").count(), html.matches("</p>").count(), "{html}");
    }

    // ── drawings ────────────────────────────────────────────────────────────

    /// A real PNG, so the media path decodes and re-encodes it for its box. It is
    /// opaque, so what comes back is a JPEG — which is why every assertion below
    /// stops at `data:image/`.
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_fn(w, h, |x, y| {
                image::Rgb([(x * 8) as u8, (y * 8) as u8, 40])
            });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// The `docx` fixture plus a relationship table for the document part and the
    /// media parts those relationships point at.
    fn docx_media(
        tag: &str,
        body: &str,
        rel_items: &[(&str, &str, &str)],
        media: &[(&str, Vec<u8>)],
    ) -> Fixture {
        let parts = vec![
            (
                "_rels/.rels",
                rels(&[("rId1", "officeDocument", "word/document.xml")]),
            ),
            ("word/document.xml", document(body)),
            ("word/styles.xml", styles_xml()),
            ("word/_rels/document.xml.rels", rels(rel_items)),
        ];
        Fixture::with_media(tag, &parts, media)
    }

    /// One `w:drawing` run: `frame` is the `wp:inline` or `wp:anchor` body that
    /// precedes the graphic (its extent, position and wrap), `graphic` the
    /// `a:graphicData` inside it.
    fn drawing(frame: &str, anchor: bool, graphic: &str) -> String {
        let tag = if anchor { "anchor" } else { "inline" };
        format!(
            "<w:r><w:drawing><wp:{tag}>{frame}<a:graphic>{graphic}</a:graphic>\
             </wp:{tag}></w:drawing></w:r>"
        )
    }

    fn extent(cx: i64, cy: i64) -> String {
        format!("<wp:extent cx=\"{cx}\" cy=\"{cy}\"/>")
    }

    /// A `pic:pic` picture referencing `rid`. The prefix is `a:` throughout
    /// because only local names are matched, so the fixture needs no further
    /// namespace declarations.
    fn blip(rid: &str) -> String {
        format!(
            "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
             <a:pic><a:blipFill><a:blip r:embed=\"{rid}\"/></a:blipFill></a:pic>\
             </a:graphicData>"
        )
    }

    #[test]
    fn an_inline_drawing_becomes_an_image_at_its_stated_box() {
        // 1828800 x 914400 EMU = 2in x 1in = 192px x 96px.
        let body = para(
            "",
            &drawing(
                &format!("{}<wp:docPr name=\"Picture 1\" descr=\"café Widget\"/>", extent(1828800, 914400)),
                false,
                &blip("rId5"),
            ),
        );
        let f = docx_media(
            "img",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(16, 8))],
        );
        let doc = f.render();
        let html = body_html(&doc).to_string();
        assert!(html.contains("<img class=\"of-img\" src=\"data:image/"), "{html}");
        assert!(html.contains("style=\"width:192px;height:96px;\""), "{html}");
        // `wp:docPr@descr` is the alt text.
        assert!(html.contains("alt=\"café Widget\""), "{html}");
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
        assert!(!doc.truncated);
    }

    #[test]
    fn a_missing_relationship_leaves_a_placeholder_at_the_same_box() {
        let body = para("", &drawing(&extent(1828800, 914400), false, &blip("rId9")));
        // No relationship table entry for rId9, so nothing resolves.
        let doc = docx_media("norel", &body, &[], &[]).render();
        let html = body_html(&doc).to_string();
        assert!(!html.contains("<img"), "{html}");
        assert!(
            html.contains(
                "<span class=\"of-gph\" style=\"width:192px;height:96px;\">image unavailable</span>"
            ),
            "{html}"
        );

        // A picture that is *linked* rather than embedded: the bytes are outside
        // the package, so the same box with the same reason.
        let linked = blip("rId5").replace("r:embed", "r:link");
        let body = para("", &drawing(&extent(914400, 914400), false, &linked));
        let doc = docx_media(
            "linked",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(8, 8))],
        )
        .render();
        let html = body_html(&doc).to_string();
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains(">image unavailable</span>"), "{html}");
    }

    #[test]
    fn a_chart_is_a_labelled_box_at_the_right_size() {
        let chart = "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"/>";
        let diagram =
            "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/diagram\"/>";
        let body = format!(
            "{}{}",
            para("", &drawing(&extent(2743200, 1828800), false, chart)),
            para("", &drawing(&extent(914400, 914400), false, diagram)),
        );
        let doc = docx_media("chart", &body, &[], &[]).render();
        let html = body_html(&doc).to_string();
        // 3in x 2in.
        assert!(
            html.contains("<span class=\"of-gph\" style=\"width:288px;height:192px;\">chart</span>"),
            "{html}"
        );
        // The box names what it stands for; the footer says the preview will never
        // draw it, which one box in a long report cannot say on its own.
        assert!(
            doc.notes.iter().any(|n| n == "Charts not drawn"),
            "{:?}",
            doc.notes
        );
        assert!(
            doc.notes.iter().any(|n| n == "SmartArt not drawn"),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn an_anchored_square_wrap_floats_to_the_side_it_hugs() {
        let frame = format!(
            "{}<wp:positionH relativeFrom=\"column\"><wp:align>right</wp:align></wp:positionH>\
             <wp:wrapSquare wrapText=\"bothSides\"/><wp:docPr name=\"Picture 2\"/>",
            extent(914400, 914400)
        );
        let body = format!(
            "{}{}",
            para("", &drawing(&frame, true, &blip("rId5"))),
            para("", &run("café naïve Widget")),
        );
        let f = docx_media(
            "float",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(16, 16))],
        );
        let html = body_html(&f.render()).to_string();
        assert!(html.contains("float:right;margin:0 0 4px 8px;"), "{html}");
        // The text that follows is ordinary flow content, which is what makes the
        // float wrap it.
        assert!(html.contains("café naïve Widget"), "{html}");
    }

    #[test]
    fn a_top_and_bottom_wrap_gets_a_line_of_its_own() {
        let frame = format!("{}<wp:wrapTopAndBottom/>", extent(914400, 914400));
        let body = para(
            "",
            &format!("{}{}", run("café"), drawing(&frame, true, &blip("rId9"))),
        );
        let html = body_html(&docx_media("topbot", &body, &[], &[]).render()).to_string();
        assert!(html.contains("café<br><span class=\"of-gph\""), "{html}");
        assert!(html.contains("</span><br></p>"), "{html}");
        // A drawing on its own line is never also floated.
        assert!(!html.contains("float:"), "{html}");
    }

    #[test]
    fn a_legacy_vml_picture_renders_from_its_shape_box() {
        // 24pt x 12pt = 32px x 16px.
        let body = para(
            "",
            "<w:r><w:pict><v:shape style=\"width:24pt;height:12pt\" id=\"Widget\">\
             <v:imagedata r:id=\"rId5\" o:title=\"café\"/></v:shape></w:pict></w:r>",
        );
        let f = docx_media(
            "pict",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(8, 4))],
        );
        let html = body_html(&f.render()).to_string();
        assert!(html.contains("<img class=\"of-img\" src=\"data:image/"), "{html}");
        assert!(html.contains("style=\"width:32px;height:16px;\""), "{html}");
        assert!(html.contains("alt=\"café\""), "{html}");
    }

    #[test]
    fn an_ole_object_is_a_labelled_box() {
        let body = para(
            "",
            "<w:r><w:object w:dxaOrig=\"1440\" w:dyaOrig=\"720\">\
             <v:shape style=\"width:72pt;height:36pt\"/>\
             <o:OLEObject Type=\"Embed\" ProgID=\"Widget.Sheet\" r:id=\"rId6\"/>\
             </w:object></w:r>",
        );
        let f = docx_media("ole", &body, &[("rId6", "oleObject", "embeddings/o1.bin")], &[]);
        let html = body_html(&f.render()).to_string();
        assert!(
            html.contains(
                "<span class=\"of-gph\" style=\"width:96px;height:48px;\">embedded object</span>"
            ),
            "{html}"
        );
    }

    #[test]
    fn an_alternate_content_takes_the_branch_that_holds_a_picture() {
        // The shape branch needs a renderer this preview does not have; the VML
        // fallback is an ordinary image part.
        let body = para(
            "",
            &format!(
                "<w:r><mc:AlternateContent><mc:Choice Requires=\"wps\">{}</mc:Choice>\
                 <mc:Fallback><w:pict><v:shape style=\"width:48pt;height:48pt\">\
                 <v:imagedata r:id=\"rId5\"/></v:shape></w:pict></mc:Fallback>\
                 </mc:AlternateContent></w:r>",
                drawing(
                    &extent(609600, 609600),
                    false,
                    "<a:graphicData uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\"/>"
                )
            ),
        );
        let f = docx_media(
            "alt",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(8, 8))],
        );
        let html = body_html(&f.render()).to_string();
        assert!(html.contains("<img class=\"of-img\" src=\"data:image/"), "{html}");
        // 48pt = 64px, from the fallback's shape rather than the choice's extent.
        assert!(html.contains("style=\"width:64px;height:64px;\""), "{html}");
    }

    #[test]
    fn an_alternate_content_around_whole_runs_still_yields_its_content() {
        // Written around the runs instead of inside one, which is where
        // `collect_runs` has to know about it.
        let body = para(
            "",
            &format!(
                "<mc:AlternateContent><mc:Choice Requires=\"wps\">{}</mc:Choice>\
                 <mc:Fallback>{}</mc:Fallback></mc:AlternateContent>",
                run("choice"),
                run("café naïve")
            ),
        );
        let html = body_html(&docx("altpara", &body, &[]).render()).to_string();
        assert!(html.contains("café naïve"), "{html}");
        assert!(!html.contains("choice"), "{html}");
    }

    #[test]
    fn an_absurd_extent_is_scaled_into_the_column() {
        // 1e11 EMU is over ten million px. The column is 624px wide (816 less two
        // 96px margins) and the 2:1 aspect survives the clamp.
        let body = para(
            "",
            &drawing(&extent(100_000_000_000, 50_000_000_000), false, &blip("rId9")),
        );
        let doc = docx_media("huge", &body, &[], &[]).render();
        let html = body_html(&doc).to_string();
        assert!(html.contains("width:624px;height:312px;"), "{html}");
        assert!(!html.contains("10499999"), "{html}");
    }

    #[test]
    fn alt_text_is_escaped_as_an_attribute_value() {
        let body = para(
            "",
            &drawing(
                &format!(
                    "{}<wp:docPr name=\"Picture 1\" descr=\"café &amp;&quot;&lt;Widget&gt;\"/>",
                    extent(914400, 914400)
                ),
                false,
                &blip("rId5"),
            ),
        );
        let f = docx_media(
            "alt-esc",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(8, 8))],
        );
        let html = body_html(&f.render()).to_string();
        // Both quote kinds and the angle brackets: an alt text that could close
        // the attribute would be an injection, not a display bug.
        assert!(
            html.contains("alt=\"café &amp;&quot;&lt;Widget&gt;\""),
            "{html}"
        );
        assert_eq!(html.matches("<img").count(), 1, "{html}");
    }

    #[test]
    fn the_graphic_count_is_capped_per_document() {
        // Placeholders cost no image bytes, so `MediaBudget` never stops them:
        // the walk's own cap is what does.
        let chart = "<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"/>";
        let one = drawing(&extent(914400, 914400), false, chart);
        let doc = docx_media("count", &para("", &one.repeat(260)), &[], &[]).render();
        let html = body_html(&doc).to_string();
        assert_eq!(html.matches("of-gph").count(), 200, "{}", html.len());
        assert!(
            doc.notes.iter().any(|n| n == "Some images not shown"),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn the_image_budget_degrades_to_a_placeholder_and_says_so() {
        let body = para("", &drawing(&extent(914400, 914400), false, &blip("rId5")));
        let f = docx_media(
            "budget",
            &body,
            &[("rId5", "image", "media/image1.png")],
            &[("word/media/image1.png", png_bytes(32, 32))],
        );
        // Room for one image of any size, but not for the bytes of this one.
        let doc = render_with(
            f.path(),
            None,
            &[],
            HTML_CAP,
            crate::office::media::MediaBudget::with_caps(1 << 20, 64),
        )
        .expect("render");
        let html = body_html(&doc).to_string();
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("<span class=\"of-gph\""), "{html}");
        assert!(html.contains("image budget reached"), "{html}");
        // The footer note is the one `media.rs` owns, once for the document.
        assert!(
            doc.notes.iter().any(|n| n == crate::office::media::NOTE_BUDGET),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn a_table_renders_in_document_order_with_its_spans() {
        // A header row spanning both columns, then a body row, then a paragraph
        // after the table: document order is what the single forward pass buys.
        let body = format!(
            "{}<w:tbl><w:tblPr><w:tblBorders>\
             <w:top w:val=\"single\" w:sz=\"8\"/><w:bottom w:val=\"single\" w:sz=\"8\"/>\
             <w:left w:val=\"single\" w:sz=\"8\"/><w:right w:val=\"single\" w:sz=\"8\"/>\
             <w:insideH w:val=\"single\" w:sz=\"8\"/><w:insideV w:val=\"single\" w:sz=\"8\"/>\
             </w:tblBorders></w:tblPr>\
             <w:tblGrid><w:gridCol w:w=\"2880\"/><w:gridCol w:w=\"1440\"/></w:tblGrid>\
             <w:tr><w:tc><w:tcPr><w:gridSpan w:val=\"2\"/></w:tcPr>{}</w:tc></w:tr>\
             <w:tr><w:tc><w:tcPr><w:vMerge w:val=\"restart\"/></w:tcPr>{}</w:tc>\
             <w:tc>{}</w:tc></w:tr>\
             <w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr><w:p/></w:tc><w:tc>{}</w:tc></w:tr>\
             </w:tbl>{}",
            para("", &run("before")),
            para("", "<w:r><w:rPr><w:b/></w:rPr><w:t>Widget</w:t></w:r>"),
            para("", &run("café")),
            para("", &run("naïve")),
            para("", &run("example.org")),
            para("", &run("after")),
        );
        let doc = docx("table", &body, &[]).render();
        let html = body_html(&doc).to_string();
        // 2880 dxa = 192px, 1440 = 96px, so the table is 288px wide.
        assert!(
            html.contains("<colgroup><col style=\"width:192px;\"><col style=\"width:96px;\">"),
            "{html}"
        );
        assert!(html.contains("style=\"width:288px;\""), "{html}");
        assert!(html.contains("colspan=\"2\""), "{html}");
        assert!(html.contains("rowspan=\"2\""), "{html}");
        // Three rows, four cells: the continuation emits no element of its own.
        assert_eq!(html.matches("<tr").count(), 3, "{html}");
        assert_eq!(html.matches("<td").count(), 4, "{html}");
        // A run inside a cell is still a run: the paragraph emitter is the same one.
        assert!(html.contains("<span style=\"font-weight:700;\">Widget</span>"), "{html}");
        // The document keeps flowing after the table.
        assert!(html.find("after").unwrap() > html.find("</table>").unwrap(), "{html}");
        // The structural rules ride in the stylesheet, not on every cell.
        assert!(doc.html.contains("table.of-tbl{border-collapse:collapse;"), "{}", doc.html);
        assert!(doc.html.contains("td.of-tc{padding:0 7.2px;"), "{}", doc.html);
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
        assert!(!doc.truncated);
    }

    #[test]
    fn a_table_style_from_the_stylesheet_borders_its_cells() {
        // The common shape of a real document: the table states only its style, and
        // every border it has comes from `word/styles.xml`.
        let styles = format!(
            "<w:styles {NS}>\
             <w:style w:type=\"table\" w:styleId=\"TableGrid\"><w:name w:val=\"Table Grid\"/>\
             <w:tblPr><w:tblBorders>\
             <w:top w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             <w:left w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             <w:bottom w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             <w:right w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             <w:insideH w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             <w:insideV w:val=\"single\" w:sz=\"4\" w:color=\"auto\"/>\
             </w:tblBorders></w:tblPr></w:style></w:styles>"
        );
        let body = format!(
            "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/></w:tblPr>\
             <w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>\
             <w:tr><w:tc>{}</w:tc></w:tr></w:tbl>",
            para("", &run("café"))
        );
        let f = docx("tblstyle", &body, &[("word/styles.xml", styles)]);
        let html = body_html(&f.render()).to_string();
        // 4 eighths of a point = 0.5pt = 0.67px, and `auto` stays at the text
        // colour rather than resolving to a black the document never stated.
        assert!(html.contains("border-top:0.67px solid currentColor;"), "{html}");
        assert!(html.contains("border-left:0.67px solid currentColor;"), "{html}");
    }

    #[test]
    fn a_cells_first_paragraph_has_no_predecessor_for_contextual_spacing() {
        // `w:contextualSpacing` drops the space between neighbours of one style. The
        // paragraph before the *table* is not a neighbour of the one inside a cell,
        // so the cell's own space has to survive.
        let styles = format!(
            "<w:styles {NS}><w:style w:type=\"paragraph\" w:styleId=\"Body\">\
             <w:name w:val=\"Body\"/><w:pPr><w:spacing w:before=\"240\"/>\
             <w:contextualSpacing/></w:pPr></w:style></w:styles>"
        );
        let item = || para("<w:pStyle w:val=\"Body\"/>", &run("café"));
        let body = format!(
            "{}<w:tbl><w:tblGrid><w:gridCol w:w=\"1440\"/></w:tblGrid>\
             <w:tr><w:tc>{}{}</w:tc></w:tr></w:tbl>",
            item(),
            item(),
            item(),
        );
        let f = docx("ctxsp", &body, &[("word/styles.xml", styles)]);
        let html = body_html(&f.render()).to_string();
        // 240 dxa = 12pt = 16px, on the body's paragraph and on the cell's first —
        // but not on the second one in the cell, which *is* a neighbour.
        assert_eq!(html.matches("margin-top:16px").count(), 2, "{html}");
    }

    #[test]
    fn a_structured_document_tag_keeps_its_text() {
        let body = format!(
            "<w:sdt><w:sdtPr/><w:sdtContent>{}</w:sdtContent></w:sdt>",
            para("", "<w:sdt><w:sdtContent><w:r><w:t>café</w:t></w:r></w:sdtContent></w:sdt>")
        );
        let doc = docx("sdt", &body, &[]).render();
        assert!(body_html(&doc).contains("café"), "{}", doc.html);
    }

    // ── links, symbols, notes, text boxes ───────────────────────────────────

    /// A relationship table of hyperlink targets. `(rId, target, external)` —
    /// Word writes every link that leaves the document as an external target, and
    /// the internal case is one of the shapes the whitelist has to refuse.
    fn hyperlink_rels(items: &[(&str, &str, bool)]) -> String {
        let body: String = items
            .iter()
            .map(|(id, target, external)| {
                let mode = if *external {
                    " TargetMode=\"External\""
                } else {
                    ""
                };
                format!(
                    "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"{target}\"{mode}/>"
                )
            })
            .collect();
        format!(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>"
        )
    }

    #[test]
    fn an_external_link_becomes_an_anchor_that_states_where_it_points() {
        let body = para(
            "",
            &format!("<w:hyperlink r:id=\"rId7\">{}</w:hyperlink>", run("café")),
        );
        let f = docx(
            "link",
            &body,
            &[(
                "word/_rels/document.xml.rels",
                hyperlink_rels(&[("rId7", "https://example.org/Widget", true)]),
            )],
        );
        let html = body_html(&f.render()).to_string();
        assert!(
            html.contains(
                "<a href=\"https://example.org/Widget\" title=\"https://example.org/Widget\">"
            ),
            "{html}"
        );
        // Word's own link appearance, from the theme's `hlink` slot: the run states
        // no colour of its own, so it takes that one and an underline.
        assert!(html.contains("text-decoration:underline;color:#0563c1;"), "{html}");
        assert!(html.contains(">café</span></a>"), "{html}");
    }

    #[test]
    fn a_link_the_whitelist_refuses_keeps_its_text_and_loses_its_href() {
        // Three ways to reach an attribute with something that is not a link: a
        // scripting scheme, a package-internal target, and an rId with no
        // relationship behind it at all.
        let body = para(
            "",
            &format!(
                "<w:hyperlink r:id=\"rId1\">{}</w:hyperlink>\
                 <w:hyperlink r:id=\"rId2\">{}</w:hyperlink>\
                 <w:hyperlink r:id=\"rId9\">{}</w:hyperlink>",
                run("café"),
                run("naïve"),
                run("Widget"),
            ),
        );
        let f = docx(
            "badlink",
            &body,
            &[(
                "word/_rels/document.xml.rels",
                hyperlink_rels(&[
                    ("rId1", "javascript:alert(1)", true),
                    ("rId2", "other.docx", false),
                ]),
            )],
        );
        let doc = f.render();
        let html = body_html(&doc).to_string();
        // The text is document content and still renders; the destination does not
        // reach the markup in any form.
        for text in ["café", "naïve", "Widget"] {
            assert!(html.contains(text), "{html}");
        }
        assert!(!html.contains("<a "), "{html}");
        assert!(!html.contains("javascript"), "{html}");
        assert!(!html.contains("other.docx"), "{html}");
    }

    #[test]
    fn an_internal_link_lands_on_its_bookmarks_sanitized_id() {
        // The bookmark is between blocks and the link is inside one, which are the
        // two places each element occurs.
        let body = format!(
            "<w:bookmarkStart w:id=\"1\" w:name=\"café Widget\"/><w:bookmarkEnd w:id=\"1\"/>{}{}",
            para("", &run("naïve")),
            para(
                "",
                "<w:hyperlink w:anchor=\"café Widget\"><w:r><w:t>jump</w:t></w:r></w:hyperlink>"
            ),
        );
        let html = body_html(&docx("anchor", &body, &[]).render()).to_string();
        // Everything outside the id alphabet is one dash, so the name cannot carry
        // markup into the attribute — and both sides go through the same mapping.
        assert!(html.contains("<span id=\"of-bm-caf--Widget\"></span>"), "{html}");
        assert!(
            html.contains("<a href=\"#of-bm-caf--Widget\" title=\"#of-bm-caf--Widget\">"),
            "{html}"
        );

        // A bookmark whose name would sanitize to nothing anchors nothing.
        let none = format!(
            "<w:bookmarkStart w:id=\"2\" w:name=\"  \"/>{}",
            para("", &run("café"))
        );
        assert!(
            !body_html(&docx("anchor-empty", &none, &[]).render()).contains("of-bm-"),
            "an empty bookmark name is not an id"
        );
    }

    #[test]
    fn a_symbol_run_survives_as_text() {
        // Wingdings 0xFC is a check mark, which the remap knows; 0xAE it does not.
        let body = para(
            "",
            "<w:r><w:rPr><w:rFonts w:ascii=\"Wingdings\"/></w:rPr>\
             <w:sym w:font=\"Wingdings\" w:char=\"F0FC\"/>\
             <w:sym w:font=\"Wingdings\" w:char=\"F0AE\"/></w:r>",
        );
        let html = body_html(&docx("sym", &body, &[]).render()).to_string();
        // Remapped: a real character, in the paragraph's own face rather than a
        // symbol font a substitute cannot draw.
        assert!(html.contains('✔'), "{html}");
        // Unmapped: the raw code point, carrying the font that gives it meaning, so
        // a system Wingdings still draws it.
        assert!(html.contains('\u{F0AE}'), "{html}");
        // Quoted in the stack, and the quotes are entity-escaped by the attribute
        // writer rather than being able to end the `style` value.
        assert!(
            html.contains("font-family:&quot;Wingdings&quot;, sans-serif;"),
            "{html}"
        );
    }

    /// `word/footnotes.xml` holding Word's two separator pseudo-notes and one real
    /// note per `(id, text)`.
    fn footnotes_xml(kind: &str, items: &[(&str, &str)]) -> String {
        let notes: String = items
            .iter()
            .map(|(id, text)| {
                format!(
                    "<w:{kind} w:id=\"{id}\"><w:p><w:r><w:{kind}Ref/></w:r>{}</w:p></w:{kind}>",
                    run(text)
                )
            })
            .collect();
        format!(
            "<w:{kind}s {NS}>\
             <w:{kind} w:id=\"0\" w:type=\"separator\"><w:p>{}</w:p></w:{kind}>\
             <w:{kind} w:id=\"1\" w:type=\"continuationSeparator\"><w:p>{}</w:p></w:{kind}>\
             {notes}</w:{kind}s>",
            run("SEPARATOR"),
            run("CONTINUATION"),
        )
    }

    #[test]
    fn a_footnote_reference_marks_the_text_and_the_note_lands_at_the_end() {
        let body = format!(
            "{}{}",
            para(
                "",
                &format!(
                    "{}<w:r><w:footnoteReference w:id=\"2\"/></w:r>",
                    run("Widget")
                )
            ),
            para("", "<w:r><w:endnoteReference w:id=\"4\"/></w:r>"),
        );
        let f = docx(
            "notes",
            &body,
            &[
                ("word/footnotes.xml", footnotes_xml("footnote", &[("2", "café note")])),
                ("word/endnotes.xml", footnotes_xml("endnote", &[("4", "naïve note")])),
            ],
        );
        let doc = f.render();
        let html = body_html(&doc).to_string();
        // The marker: an anchor for the note to come back to, then a superscript
        // number linking down to it.
        assert!(html.contains("<span id=\"of-fn-ref-1\"></span>"), "{html}");
        assert!(html.contains("<a href=\"#of-fn-1\" title=\"#of-fn-1\">"), "{html}");
        assert!(html.contains("vertical-align:super;"), "{html}");
        // The note itself, with its own text, linking back to the marker.
        assert!(html.contains("<div class=\"of-fnotes\">"), "{html}");
        assert!(html.contains("<div class=\"of-fn\" id=\"of-fn-1\">"), "{html}");
        assert!(html.contains("<a class=\"of-fnb\" href=\"#of-fn-ref-1\">1</a>"), "{html}");
        assert!(html.contains("café note"), "{html}");
        // An endnote is numbered in its own sequence, as Word's default spells it.
        assert!(html.contains("<div class=\"of-fn\" id=\"of-en-1\">"), "{html}");
        assert!(html.contains("<a class=\"of-fnb\" href=\"#of-en-ref-1\">i</a>"), "{html}");
        assert!(html.contains("naïve note"), "{html}");
        // The block sits after the text, and Word's separator pseudo-notes are not
        // content.
        assert!(html.find("of-fnotes").unwrap() > html.find("Widget").unwrap(), "{html}");
        assert!(!html.contains("SEPARATOR") && !html.contains("CONTINUATION"), "{html}");
        // Not noted: where the notes sit is visible in the rendered column, and a
        // footer line fires for every document that has a single footnote.
        assert!(doc.notes.is_empty(), "{:?}", doc.notes);
    }

    #[test]
    fn a_reference_with_no_note_behind_it_says_so() {
        let body = para(
            "",
            &format!("{}<w:r><w:footnoteReference w:id=\"9\"/></w:r>", run("café")),
        );
        let f = docx(
            "notes-missing",
            &body,
            &[("word/footnotes.xml", footnotes_xml("footnote", &[("2", "unreferenced")]))],
        );
        let doc = f.render();
        let html = body_html(&doc).to_string();
        // No marker leading nowhere, no block, and a note that text was dropped.
        assert!(!html.contains("of-fnotes"), "{html}");
        assert!(!html.contains("unreferenced"), "{html}");
        assert!(doc.notes.iter().any(|n| n == notes::NOTE_MISSING), "{:?}", doc.notes);
    }

    #[test]
    fn a_text_boxs_paragraphs_follow_the_paragraph_that_anchors_them() {
        // The legacy VML spelling and the modern `wps` one, each anchored to its own
        // paragraph.
        let vml = "<w:r><w:pict><v:shape style=\"width:120pt;height:60pt\"><v:textbox>\
             <w:txbxContent><w:p><w:r><w:t>café in a box</w:t></w:r></w:p></w:txbxContent>\
             </v:textbox></v:shape></w:pict></w:r>";
        let wps = format!(
            "<w:r><w:drawing><wp:inline>{}<a:graphic><a:graphicData \
             uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">\
             <wps:wsp><wps:txbx><w:txbxContent>{}</w:txbxContent></wps:txbx></wps:wsp>\
             </a:graphicData></a:graphic></wp:inline></w:drawing></w:r>",
            extent(1097280, 548640),
            para("", &run("naïve in a shape")),
        );
        let body = format!(
            "{}{}{}",
            para("", &format!("{}{}", run("anchor"), vml)),
            para("", &format!("{}{}", run("second"), wps)),
            para("", &run("after")),
        );
        let doc = docx("txbx", &body, &[]).render();
        let html = body_html(&doc).to_string();
        // The box's text is *there* — the whole point — as a bordered block between
        // the paragraph that anchored it and the next one.
        let at = |needle: &str| html.find(needle).unwrap_or_else(|| panic!("{needle}: {html}"));
        assert!(at("anchor") < at("café in a box"), "{html}");
        assert!(at("café in a box") < at("second"), "{html}");
        assert!(at("naïve in a shape") < at("after"), "{html}");
        assert_eq!(html.matches("<div class=\"of-txbx\">").count(), 2, "{html}");
        // Block content cannot sit inside a `p`: the box follows the paragraph
        // rather than splitting it.
        assert!(html.contains("</p><div class=\"of-txbx\">"), "{html}");
        // The position Word gives the box is not honoured, so the footer says so.
        assert!(doc.notes.iter().any(|n| n == body::NOTE_TXBX), "{:?}", doc.notes);
    }

    #[test]
    fn a_block_level_alternate_content_yields_its_fallback() {
        // Written around whole blocks rather than around runs or inside one, which
        // is where `body::walk` has to know about it.
        let body = format!(
            "<mc:AlternateContent><mc:Choice Requires=\"w14\">{}</mc:Choice>\
             <mc:Fallback>{}</mc:Fallback></mc:AlternateContent>{}",
            para("", &run("choice")),
            para("", &run("café naïve")),
            para("", &run("after")),
        );
        let html = body_html(&docx("altblock", &body, &[]).render()).to_string();
        assert!(html.contains("café naïve"), "{html}");
        assert!(!html.contains("choice"), "{html}");
        // Document order survives the wrapper.
        assert!(html.find("café naïve").unwrap() < html.find("after").unwrap(), "{html}");
    }

    #[test]
    fn hidden_text_is_dropped_before_the_highlighter_sees_it() {
        let body = para(
            "",
            &format!(
                "<w:r><w:rPr><w:vanish/></w:rPr><w:t>café</w:t></w:r>{}",
                run("naïve")
            ),
        );
        let f = docx("vanish", &body, &[]);
        let doc = super::render(f.path(), None, &["café".to_string()]).expect("render");
        assert!(!doc.html.contains("café"), "{}", doc.html);
        assert!(doc.best_mark_id.is_none(), "{:?}", doc.best_mark_id);
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn a_document_indexes_as_one_unnamed_section() {
        let f = docx("index-one", &para("", &run("café")), &[]);
        let sections = extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "");
        // One section means no separator in the flat text, so the index stores a
        // single row and `best_section` answers 0.
        assert_eq!(sections[0].text, "café");
        assert!(!sections[0].text.contains(super::super::SECTION_SEP));
    }

    #[test]
    fn tabs_and_breaks_separate_indexed_words() {
        // The recall bug this fixes: `w:tab` and `w:br` carry no characters, so
        // emitting nothing for them turned a tab-separated row into one token no
        // query could match.
        let body = format!(
            "{}{}",
            para("", "<w:r><w:t>café</w:t><w:tab/><w:t>naïve</w:t></w:r>"),
            para("", "<w:r><w:t>one</w:t><w:br/><w:t>two</w:t></w:r>"),
        );
        let f = docx("index-tab", &body, &[]);
        let text = &extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("café naïve"), "{text:?}");
        assert!(text.contains("one two"), "{text:?}");
    }
}
