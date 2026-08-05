//! pptx → styled HTML, one slide per call.
//!
//! Slide identity comes from `ppt/presentation.xml`'s `<p:sldIdLst>`, resolved
//! through the presentation's relationships — part names carry no ordering
//! (`slide1.xml` is routinely the fourth slide of a deck). Enumerating the
//! `ppt/slides/` directory is only the fallback for a package with no slide list.
//!
//! A slide is emitted as one fixed-size canvas of absolutely positioned boxes in
//! px at 96dpi, so the document lays out once and the frontend scales it with a
//! single transform. Three trees are drawn in order — master, layout, slide —
//! which is how PowerPoint composes a slide's background furniture.

mod inherit;
mod shapes;
mod text;

use super::drawingml::color::parse_color_elem_map;
use super::drawingml::fill::{parse_fill_opt, push_fill, Fill};
use super::drawingml::theme::{ClrMap, Theme};
use super::emit::{self, Notes};
use super::highlight::{Marker, Terms};
use super::html::{attr, attrs, emu_to_px, Writer};
use super::media::{MediaBudget, MediaCache};
use super::opc::Rels;
use super::pkg::{self, Budget, Zip};
use super::text::Section;
use super::xml::{child, elems};
use super::{opc, slideshape, xml, OfficeDoc, Shape};
use roxmltree::Node;

/// Byte cap for the emitted body HTML.
pub const HTML_CAP: usize = 6 * 1024 * 1024;

const MAX_SLIDES: usize = 500;
const MAX_TITLE_CHARS: usize = 72;

/// Everything one slide render needs. Grouped so the emitters can take disjoint
/// mutable borrows of the pieces they touch.
pub struct Ctx<'a> {
    pub zip: &'a mut Zip,
    pub budget: &'a mut Budget,
    pub theme: &'a Theme,
    /// The master's colour map (possibly overridden by the layout): what `tx1`,
    /// `bg1` and friends resolve to for this slide.
    pub clr_map: ClrMap,
    pub terms: &'a Terms,
    pub marker: &'a mut Marker,
    pub media: &'a mut MediaCache,
    pub mb: &'a mut MediaBudget,
    pub notes: &'a mut Notes,
    /// Shapes emitted so far, across all three trees — the walk's own bound.
    pub shapes: usize,
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
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();
    let mut notes = Notes::new();

    let pres_part = opc::root_part(&mut zip, &mut budget, "ppt/presentation.xml");
    let pres_xml = pkg::read_entry(&mut zip, &pres_part, &mut budget)?
        .ok_or_else(|| format!("pptx: missing presentation part ({pres_part})"))?;
    let pres_rels = opc::read_rels(&mut zip, &pres_part, &mut budget).unwrap_or_default();
    let pres_doc = xml::parse(&pres_xml)?;
    let pres = pres_doc.root_element();

    let natural = slide_size(pres);
    let slides = slide_parts(pres, &pres_part, &pres_rels, &mut zip);
    if slides.is_empty() {
        return Err("pptx: presentation contains no slides".to_string());
    }
    let last = slides.len().saturating_sub(1) as u32;
    let idx = section.map(|s| s.min(last)).unwrap_or(0);

    // Slide names are positional. A title would be nicer for every entry, but
    // that would mean reading every slide part on every flip; the rendered
    // slide's own title is filled in below, which is the one the UI shows.
    let mut sections: Vec<String> = (1..=slides.len()).map(|i| format!("Slide {i}")).collect();

    let part = slides[idx as usize].clone();
    let query = Terms::new(terms);
    let mut marker = Marker::new();
    let mut media = MediaCache::new();
    let mut mb = MediaBudget::new();

    let out = render_slide(
        &mut zip,
        &mut budget,
        &part,
        pres,
        natural,
        &query,
        &mut marker,
        &mut media,
        &mut mb,
        &mut notes,
        html_cap,
    );
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let msg = emit::degrade_msg(&e, "slide");
            notes.add(&msg);
            SlideOut {
                html: error_body(&msg, natural),
                truncated: true,
                title: None,
            }
        }
    };
    if let Some(t) = out.title.filter(|t| !t.is_empty()) {
        sections[idx as usize] = t;
    }
    for n in mb.notes() {
        notes.add(n);
    }

    Ok(OfficeDoc {
        html: out.html,
        shape: Shape::Slide,
        sections,
        section: idx,
        natural: Some(natural),
        page: None,
        best_mark_id: marker.best_mark_id(),
        truncated: out.truncated,
        notes: notes.into_vec(),
    })
}

struct SlideOut {
    html: String,
    truncated: bool,
    title: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn render_slide(
    zip: &mut Zip,
    budget: &mut Budget,
    part: &str,
    pres: Node,
    natural: (f32, f32),
    terms: &Terms,
    marker: &mut Marker,
    media: &mut MediaCache,
    mb: &mut MediaBudget,
    notes: &mut Notes,
    html_cap: usize,
) -> Result<SlideOut, String> {
    // All three parts are read up front: the parsed documents borrow their
    // strings, so the strings have to outlive every node handed to the walk.
    let slide_xml = pkg::read_entry(zip, part, budget)?
        .ok_or_else(|| format!("pptx: missing slide part {part}"))?;
    let slide_rels = opc::read_rels(zip, part, budget).unwrap_or_default();

    let layout_part = opc::part_by_kind_sorted(&slide_rels, part, "/slideLayout");
    let layout_xml = match &layout_part {
        Some(p) => pkg::read_entry(zip, p, budget)?,
        None => None,
    };
    let layout_rels = match &layout_part {
        Some(p) => opc::read_rels(zip, p, budget).unwrap_or_default(),
        None => Rels::new(),
    };

    let master_part = layout_part
        .as_deref()
        .and_then(|lp| opc::part_by_kind_sorted(&layout_rels, lp, "/slideMaster"));
    let master_xml = match &master_part {
        Some(p) => pkg::read_entry(zip, p, budget)?,
        None => None,
    };
    let master_rels = match &master_part {
        Some(p) => opc::read_rels(zip, p, budget).unwrap_or_default(),
        None => Rels::new(),
    };

    let theme = match master_part
        .as_deref()
        .and_then(|mp| opc::part_by_kind_sorted(&master_rels, mp, "/theme"))
    {
        Some(p) => match pkg::read_entry(zip, &p, budget)? {
            Some(x) => Theme::parse(&x).unwrap_or_default(),
            None => Theme::default(),
        },
        None => Theme::default(),
    };

    let slide_doc = xml::parse(&slide_xml)?;
    let slide = slide_doc.root_element();
    if slide.tag_name().name() != "sld" {
        return Err(format!("pptx: {part} is not a slide part"));
    }
    let layout_doc = layout_xml.as_deref().and_then(|x| xml::parse(x).ok());
    let master_doc = master_xml.as_deref().and_then(|x| xml::parse(x).ok());
    let layout = layout_doc.as_ref().map(|d| d.root_element());
    let master = master_doc.as_ref().map(|d| d.root_element());
    if layout.is_none() {
        notes.add(
            "Slide layout unreadable",
        );
    }

    // The master defines the colour map; a layout may override it wholesale
    // (that is how the "dark" variant of a design swaps text and background).
    let clr_map = master
        .and_then(|m| child(m, "clrMap"))
        .map(ClrMap::parse)
        .unwrap_or_default();
    let clr_map = layout
        .and_then(|l| child(l, "clrMapOvr"))
        .and_then(|o| child(o, "overrideClrMapping"))
        .map(ClrMap::parse)
        .unwrap_or(clr_map);

    let mut ctx = shapes_ctx(zip, budget, &theme, clr_map, terms, marker, media, mb, notes);
    let mut w = Writer::new(html_cap);

    // ── canvas ───────────────────────────────────────────────────────────────
    let bg = background(&mut ctx, slide, layout, master);
    let mut css = slideshape::canvas_css(natural);
    let bg_pic = match &bg {
        Some(Fill::Picture(p)) => Some(p.clone()),
        _ => None,
    };
    if let Some(f) = bg.as_ref().filter(|_| bg_pic.is_none()) {
        push_fill(&mut css, f);
    }
    w.open("div", &attrs(&[&attr("class", "pp-doc"), &css.to_attr()]));

    // ── the three trees, back to front ───────────────────────────────────────
    let show_master = truthy_attr(slide, "showMasterSp")
        && layout.map(|l| truthy_attr(l, "showMasterSp")).unwrap_or(true);
    let master_tree = master.and_then(inherit::sp_tree);
    let layout_tree = layout.and_then(inherit::sp_tree);
    let slide_tree = inherit::sp_tree(slide);
    let default_text = child(pres, "defaultTextStyle");

    if let Some(bp) = bg_pic.as_ref() {
        // The background picture belongs to whichever part declared it; in
        // practice that is the layout or master, so its relationships are tried
        // in the same order the fill was.
        let env = bg_env(
            &slide_rels,
            part,
            &layout_rels,
            layout_part.as_deref(),
            &master_rels,
            master_part.as_deref(),
        );
        emit_background_picture(&mut ctx, &mut w, &env, &bp.embed, natural);
    }

    if show_master {
        if let (Some(tree), Some(mp)) = (master_tree, master_part.as_deref()) {
            let env = shapes::Env {
                rels: &master_rels,
                part: mp,
                layout: None,
                master: None,
                master_root: master,
                default_text,
                inherit: false,
            };
            shapes::walk(&mut ctx, &mut w, tree, &env, &mut Vec::new());
        }
    }
    if let (Some(tree), Some(lp)) = (layout_tree, layout_part.as_deref()) {
        let env = shapes::Env {
            rels: &layout_rels,
            part: lp,
            layout: None,
            master: None,
            master_root: master,
            default_text,
            inherit: false,
        };
        shapes::walk(&mut ctx, &mut w, tree, &env, &mut Vec::new());
    }
    let mut title = None;
    if let Some(tree) = slide_tree {
        let env = shapes::Env {
            rels: &slide_rels,
            part,
            layout: layout_tree,
            master: master_tree,
            master_root: master,
            default_text,
            inherit: true,
        };
        shapes::walk(&mut ctx, &mut w, tree, &env, &mut Vec::new());
        title = slide_title(tree);
    }
    w.close();

    let truncated = w.truncated();
    Ok(SlideOut {
        html: emit::wrap_style(slideshape::BASE_CSS, "", w.finish()),
        truncated,
        title,
    })
}

#[allow(clippy::too_many_arguments)]
fn shapes_ctx<'a>(
    zip: &'a mut Zip,
    budget: &'a mut Budget,
    theme: &'a Theme,
    clr_map: ClrMap,
    terms: &'a Terms,
    marker: &'a mut Marker,
    media: &'a mut MediaCache,
    mb: &'a mut MediaBudget,
    notes: &'a mut Notes,
) -> Ctx<'a> {
    Ctx {
        zip,
        budget,
        theme,
        clr_map,
        terms,
        marker,
        media,
        mb,
        notes,
        shapes: 0,
    }
}

/// `showMasterSp` defaults to true when absent.
fn truthy_attr(n: Node, name: &str) -> bool {
    match n.attribute(name) {
        Some(v) => !matches!(v, "0" | "false"),
        None => true,
    }
}

// ── background ───────────────────────────────────────────────────────────────

/// `p:cSld/p:bg` of the slide, else the layout's, else the master's. `p:bgRef`
/// indexes the theme's background fill styles, which are not modelled; its
/// colour is used as a solid fill, which is what the overwhelming majority of
/// `bgRef` backgrounds actually are.
fn background(ctx: &mut Ctx, slide: Node, layout: Option<Node>, master: Option<Node>) -> Option<Fill> {
    for root in [Some(slide), layout, master].into_iter().flatten() {
        let Some(bg) = child(root, "cSld").and_then(|c| child(c, "bg")) else {
            continue;
        };
        if let Some(pr) = child(bg, "bgPr") {
            if let Some(f) = parse_fill_opt(pr, ctx.theme, None) {
                return Some(f);
            }
        }
        if let Some(r) = child(bg, "bgRef") {
            if let Some(c) = parse_color_elem_map(r, ctx.theme, &ctx.clr_map, None) {
                return Some(Fill::Solid(c));
            }
        }
    }
    None
}

/// Relationship lookup for a background picture: the fill was resolved from
/// whichever of the three parts declared it, so the embed id is tried against
/// each in the same order.
struct BgEnv<'a> {
    parts: Vec<(&'a Rels, &'a str)>,
}

fn bg_env<'a>(
    slide_rels: &'a Rels,
    slide_part: &'a str,
    layout_rels: &'a Rels,
    layout_part: Option<&'a str>,
    master_rels: &'a Rels,
    master_part: Option<&'a str>,
) -> BgEnv<'a> {
    let mut parts = vec![(slide_rels, slide_part)];
    if let Some(p) = layout_part {
        parts.push((layout_rels, p));
    }
    if let Some(p) = master_part {
        parts.push((master_rels, p));
    }
    BgEnv { parts }
}

fn emit_background_picture(
    ctx: &mut Ctx,
    w: &mut Writer,
    env: &BgEnv,
    embed: &str,
    natural: (f32, f32),
) {
    for (rels, part) in &env.parts {
        let Some(r) = rels.get(embed) else { continue };
        if r.external {
            continue;
        }
        let Some(target) = opc::resolve_target(part, &r.target) else {
            continue;
        };
        let want = natural.0.clamp(1.0, 4096.0) as u32;
        if let super::media::Media::DataUri(uri) =
            ctx.media.get(ctx.zip, ctx.budget, ctx.mb, &target, want)
        {
            w.void(
                "img",
                &attrs(&[&attr("class", "pp-bg"), &attr("src", &uri), &attr("alt", "")]),
            );
            return;
        }
    }
}

// ── content index ────────────────────────────────────────────────────────────

/// Flat text for the content index: one section per slide, in `p:sldIdLst` order
/// — the same order [`render`] uses, so section 3 of the index is the slide the
/// reader flips to for `section = 3`.
///
/// A slide whose part cannot be read keeps its place with empty text rather than
/// being skipped: dropping it would shift every later slide's index by one, which
/// is worse than a slide the search cannot find. Speaker notes live in their own
/// `notesSlide` parts and are not indexed — the preview does not draw them.
pub(super) fn extract_sections(path: &str) -> Result<Vec<Section>, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();

    let pres_part = opc::root_part(&mut zip, &mut budget, "ppt/presentation.xml");
    let pres_xml = pkg::read_entry(&mut zip, &pres_part, &mut budget)?
        .ok_or_else(|| format!("pptx: missing presentation part ({pres_part})"))?;
    let pres_rels = opc::read_rels(&mut zip, &pres_part, &mut budget).unwrap_or_default();
    let pres_doc = xml::parse(&pres_xml)?;
    let parts = slide_parts(pres_doc.root_element(), &pres_part, &pres_rels, &mut zip);
    if parts.is_empty() {
        return Err("pptx: presentation contains no slides".to_string());
    }

    let mut out = Vec::with_capacity(parts.len());
    for part in &parts {
        let text = match pkg::read_entry(&mut zip, part, &mut budget) {
            Ok(Some(x)) => xml::xml_text(&x, &["p"], &["t"]).unwrap_or_default(),
            // Budget exhausted or part unreadable: an empty section, not a missing
            // one. Slide names are positional, so a title is not read here — it is
            // in the slide's own text either way.
            _ => String::new(),
        };
        out.push(Section::new("", text));
    }
    Ok(out)
}

// ── presentation ─────────────────────────────────────────────────────────────

fn slide_size(pres: Node) -> (f32, f32) {
    let Some(sz) = child(pres, "sldSz") else {
        return slideshape::DEFAULT_SLIDE;
    };
    let num = |name: &str| {
        sz.attribute(name)
            .and_then(|v| v.parse::<i64>().ok())
            .map(emu_to_px)
            .filter(|v| slideshape::in_range(*v))
    };
    match (num("cx"), num("cy")) {
        (Some(w), Some(h)) => (w, h),
        _ => slideshape::DEFAULT_SLIDE,
    }
}

/// Slide parts in presentation order. Falls back to the `ppt/slides/` directory
/// in natural filename order for a package with no `p:sldIdLst`.
fn slide_parts(pres: Node, pres_part: &str, rels: &Rels, zip: &mut Zip) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(lst) = child(pres, "sldIdLst") {
        for id in elems(lst) {
            if id.tag_name().name() != "sldId" {
                continue;
            }
            let Some(rid) = rel_id(id) else { continue };
            let Some(r) = rels.get(rid) else { continue };
            if r.external {
                continue;
            }
            if let Some(p) = opc::resolve_target(pres_part, &r.target) {
                out.push(p);
            }
            if out.len() >= MAX_SLIDES {
                break;
            }
        }
    }
    if out.is_empty() {
        let mut names: Vec<String> = zip
            .file_names()
            .filter(|n| {
                n.starts_with("ppt/slides/slide")
                    && n.ends_with(".xml")
                    && !n.contains("_rels")
            })
            .map(|n| n.to_string())
            .collect();
        names.sort_by(|a, b| xml::natural_cmp(a, b));
        names.truncate(MAX_SLIDES);
        out = names;
    }
    out
}

/// The relationship id of a `p:sldId` — `r:id`, not the sibling `id`. Matching
/// on the local name alone finds the slide's own numeric id first, which
/// resolves against nothing.
fn rel_id<'a>(n: Node<'a, 'a>) -> Option<&'a str> {
    n.attributes()
        .find(|a| a.name() == "id" && !a.namespace().unwrap_or("").is_empty())
        .map(|a| a.value())
}

/// Text of the slide's title placeholder, for the section list.
fn slide_title(tree: Node) -> Option<String> {
    for sp in elems(tree) {
        if sp.tag_name().name() != "sp" {
            continue;
        }
        let Some(ph) = inherit::ph_of(sp) else { continue };
        if !matches!(inherit::style_kind(&ph.ty), inherit::StyleKind::Title) {
            continue;
        }
        let tb = child(sp, "txBody")?;
        let t = text::plain_text(tb, MAX_TITLE_CHARS);
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

// ── output plumbing ──────────────────────────────────────────────────────────

/// Body for a slide that could not be rendered: an empty canvas at the right
/// size with the reason on it, so the reader's geometry still works.
fn error_body(msg: &str, natural: (f32, f32)) -> String {
    slideshape::error_body(msg, natural)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// A minimal pptx package on disk, deleted when the test ends.
    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    impl Fixture {
        fn new(tag: &str, entries: &[(&str, String)]) -> Fixture {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "portunus-pptx-{tag}-{}-{n}.pptx",
                std::process::id()
            ));
            let f = std::fs::File::create(&path).expect("fixture file");
            let mut z = zip::ZipWriter::new(f);
            let mut seen = std::collections::HashSet::new();
            for (name, body) in entries {
                if !seen.insert(*name) {
                    continue;
                }
                z.start_file::<_, ()>(*name, zip::write::FileOptions::default())
                    .expect("zip entry");
                z.write_all(body.as_bytes()).expect("zip write");
            }
            z.finish().expect("zip finish");
            Fixture(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 path")
        }

        fn render(&self, section: Option<u32>) -> OfficeDoc {
            super::render(self.path(), section, &[]).expect("render succeeds")
        }
    }

    /// The rendered markup without the stylesheet. Assertions about what a shape
    /// paints have to look at the shape: BASE_CSS mentions `border:` and
    /// `overflow:hidden` itself, so a naive `contains` on the whole document
    /// passes for the wrong reason.
    fn body(doc: &OfficeDoc) -> &str {
        doc.html.split_once("</style>").map(|(_, b)| b).unwrap_or(&doc.html)
    }

    fn rels(items: &[(&str, &str, &str)]) -> String {
        let mut s = String::from(
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        );
        for (id, kind, target) in items {
            s.push_str(&format!(
                "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}\" Target=\"{target}\"/>"
            ));
        }
        s.push_str("</Relationships>");
        s
    }

    fn sld(body: &str) -> String {
        format!(
            "<p:sld xmlns:p=\"p\" xmlns:a=\"a\" xmlns:r=\"r\"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"
        )
    }

    /// A placeholder shape carrying its own geometry — the common case, and the
    /// one that does not depend on a layout being present.
    fn ph_sp(ty: &str, text: &str) -> String {
        format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"t\"/><p:nvPr><p:ph type=\"{ty}\"/></p:nvPr></p:nvSpPr>\
             <p:spPr><a:xfrm><a:off x=\"914400\" y=\"457200\"/><a:ext cx=\"3657600\" cy=\"914400\"/></a:xfrm></p:spPr>\
             <p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"
        )
    }

    /// The same shape with no transform at all, for the degradation path.
    fn ph_sp_no_xfrm(ty: &str, text: &str) -> String {
        format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"t\"/><p:nvPr><p:ph type=\"{ty}\"/></p:nvPr></p:nvSpPr>\
             <p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"
        )
    }

    /// Two slides, the second listed first, so a renderer that reads part names
    /// instead of the slide list shows the wrong slide.
    fn deck(tag: &str) -> Fixture {
        Fixture::new(
            tag,
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"12192000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId2\"/><p:sldId id=\"257\" r:id=\"rId1\"/></p:sldIdLst>\
                     </p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[
                        ("rId1", "slide", "slides/slide1.xml"),
                        ("rId2", "slide", "slides/slide2.xml"),
                    ]),
                ),
                (
                    "ppt/slides/slide1.xml",
                    sld(&ph_sp("title", "naïve second")),
                ),
                (
                    "ppt/slides/slide2.xml",
                    sld(&ph_sp("title", "café first")),
                ),
            ],
        )
    }

    #[test]
    fn slide_order_follows_the_slide_list_not_part_names() {
        let f = deck("order");
        let doc = f.render(None);
        assert_eq!(doc.sections.len(), 2);
        assert!(doc.html.contains("café first"), "{}", doc.html);
        // The title of the rendered slide names its section entry.
        assert_eq!(doc.sections[0], "café first");
        assert_eq!(doc.sections[1], "Slide 2");
        let second = f.render(Some(1));
        assert!(second.html.contains("naïve second"), "{}", second.html);
        assert_eq!(second.section, 1);
    }

    #[test]
    fn slide_size_becomes_the_canvas() {
        let doc = deck("size").render(None);
        // 12192000 EMU = 1280px, 6858000 = 720px (16:9 at 96dpi).
        assert_eq!(doc.natural, Some((1280.0, 720.0)));
        assert!(doc.html.contains("width:1280px"), "{}", doc.html);
        assert!(matches!(doc.shape, Shape::Slide));
    }

    #[test]
    fn out_of_range_section_clamps_to_the_last_slide() {
        let doc = deck("clamp").render(Some(99));
        assert_eq!(doc.section, 1);
    }

    #[test]
    fn missing_slide_part_degrades_to_a_note() {
        let f = Fixture::new(
            "nopart",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
            ],
        );
        let doc = f.render(None);
        assert!(doc.truncated);
        assert!(!doc.notes.is_empty());
        assert!(doc.html.contains("office-note"), "{}", doc.html);
    }

    #[test]
    fn deck_without_a_slide_list_falls_back_to_part_order() {
        let f = Fixture::new(
            "fallback",
            &[
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/></p:presentation>"
                        .to_string(),
                ),
                ("ppt/slides/slide2.xml", sld(&ph_sp("title", "second"))),
                ("ppt/slides/slide10.xml", sld(&ph_sp("title", "tenth"))),
            ],
        );
        let doc = f.render(None);
        assert_eq!(doc.sections.len(), 2);
        // Natural order, so slide2 precedes slide10.
        assert!(doc.html.contains("second"), "{}", doc.html);
    }

    /// A one-slide deck with a full slide → layout → master chain, so the text
    /// cascade and the master's own shapes are both exercised.
    fn chained(tag: &str, slide_body: &str, master_extra: &str) -> Fixture {
        Fixture::new(
            tag,
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
                ("ppt/slides/slide1.xml", sld(slide_body)),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    rels(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                (
                    "ppt/slideLayouts/slideLayout1.xml",
                    "<p:sldLayout xmlns:p=\"p\" xmlns:a=\"a\"><p:cSld><p:spTree/></p:cSld></p:sldLayout>"
                        .to_string(),
                ),
                (
                    "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                    rels(&[("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")]),
                ),
                (
                    "ppt/slideMasters/slideMaster1.xml",
                    format!(
                        "<p:sldMaster xmlns:p=\"p\" xmlns:a=\"a\"><p:cSld><p:spTree/></p:cSld>\
                         <p:clrMap bg1=\"lt1\" tx1=\"dk1\"/><p:txStyles>{master_extra}</p:txStyles></p:sldMaster>"
                    ),
                ),
            ],
        )
    }

    #[test]
    fn body_text_size_comes_from_the_master_style() {
        let f = chained(
            "cascade",
            &ph_sp("body", "café"),
            "<p:bodyStyle><a:lvl1pPr algn=\"r\"><a:defRPr sz=\"2800\"/></a:lvl1pPr></p:bodyStyle>",
        );
        let doc = f.render(None);
        // 28pt = 37.33px at 96dpi, and the master's alignment applies too.
        assert!(doc.html.contains("font-size:37.33px"), "{}", doc.html);
        assert!(doc.html.contains("text-align:right"), "{}", doc.html);
    }

    #[test]
    fn group_children_are_placed_in_slide_coordinates() {
        // Group at (96,96) sized 192x96 over a 96x48 child space: everything
        // inside is scaled 2x and offset by the group's origin.
        let body = "<p:grpSp><p:nvGrpSpPr><p:cNvPr id=\"3\"/><p:nvPr/></p:nvGrpSpPr>\
                    <p:grpSpPr><a:xfrm><a:off x=\"914400\" y=\"914400\"/><a:ext cx=\"1828800\" cy=\"914400\"/>\
                    <a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"914400\" cy=\"457200\"/></a:xfrm></p:grpSpPr>\
                    <p:sp><p:nvSpPr><p:cNvPr id=\"4\"/><p:nvPr/></p:nvSpPr>\
                    <p:spPr><a:xfrm><a:off x=\"457200\" y=\"228600\"/><a:ext cx=\"457200\" cy=\"228600\"/></a:xfrm></p:spPr>\
                    </p:sp></p:grpSp>";
        let doc = chained("group", body, "").render(None);
        assert!(doc.html.contains("left:192px"), "{}", doc.html);
        assert!(doc.html.contains("top:144px"), "{}", doc.html);
        assert!(doc.html.contains("width:96px"), "{}", doc.html);
    }

    #[test]
    fn a_table_frame_becomes_a_table() {
        let body = "<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"5\"/><p:nvPr/></p:nvGraphicFramePr>\
                    <p:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"1828800\" cy=\"914400\"/></p:xfrm>\
                    <a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/table\">\
                    <a:tbl><a:tblGrid><a:gridCol w=\"914400\"/><a:gridCol w=\"914400\"/></a:tblGrid>\
                    <a:tr h=\"457200\"><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>café</a:t></a:r></a:p></a:txBody></a:tc>\
                    <a:tc hMerge=\"1\"><a:txBody><a:bodyPr/><a:p><a:r><a:t>naïve</a:t></a:r></a:p></a:txBody></a:tc>\
                    </a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>";
        let doc = chained("table", body, "").render(None);
        assert!(doc.html.contains("class=\"pp-tbl\""), "{}", doc.html);
        assert!(doc.html.contains("café"), "{}", doc.html);
        // A merged-away cell must not be emitted, or the row gains a column.
        assert!(!doc.html.contains("naïve"), "{}", doc.html);
        assert!(doc.html.contains("width:96px"), "{}", doc.html);
    }

    #[test]
    fn a_blank_line_in_a_bulleted_list_carries_no_bullet() {
        let body_sp = "<p:sp><p:nvSpPr><p:cNvPr id=\"8\"/><p:nvPr><p:ph type=\"body\"/></p:nvPr></p:nvSpPr>\
             <p:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"3657600\" cy=\"1828800\"/></a:xfrm></p:spPr>\
             <p:txBody><a:bodyPr/>\
             <a:p><a:r><a:t>café</a:t></a:r></a:p>\
             <a:p><a:endParaRPr sz=\"1800\"/></a:p>\
             <a:p><a:r><a:t>naïve</a:t></a:r></a:p>\
             </p:txBody></p:sp>";
        let doc = chained(
            "blankbullet",
            body_sp,
            "<p:bodyStyle><a:lvl1pPr marL=\"342900\" indent=\"-342900\">\
             <a:buChar char=\"•\"/><a:defRPr sz=\"1800\"/></a:lvl1pPr></p:bodyStyle>",
        )
        .render(None);
        let html = body(&doc);
        assert_eq!(html.matches("pp-bu").count(), 2, "{html}");
        // The blank paragraph still holds its line height.
        assert!(html.contains('\u{00a0}'), "{html}");
    }

    #[test]
    fn a_zero_style_reference_paints_nothing() {
        // `idx="0"` is the theme style list's explicit "none" entry, not an index -
        // it is how a text placeholder states that it has no fill and no outline.
        let sp = |idx: &str| {
            format!(
                "<p:sp><p:nvSpPr><p:cNvPr id=\"7\"/><p:nvPr/></p:nvSpPr>\
                 <p:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"914400\" cy=\"914400\"/></a:xfrm></p:spPr>\
                 <p:style><a:lnRef idx=\"{idx}\"><a:schemeClr val=\"accent1\"/></a:lnRef>\
                 <a:fillRef idx=\"{idx}\"><a:schemeClr val=\"accent1\"/></a:fillRef></p:style></p:sp>"
            )
        };
        let plain = chained("lnref0", &sp("0"), "").render(None);
        let none = body(&plain);
        assert!(!none.contains("border:"), "{none}");
        let ref2 = chained("lnref2", &sp("2"), "").render(None);
        let styled = body(&ref2);
        assert!(styled.contains("border:"), "{styled}");
    }

    #[test]
    fn use_bg_fill_lets_the_slide_background_show_through() {
        // A full-bleed `useBgFill` rectangle is how PowerPoint writes "cover the
        // layout's art with the slide's own background". Falling through to the
        // style reference instead floods the slide with the first accent colour.
        let sp = |extra: &str| {
            format!(
                "<p:sp {extra}><p:nvSpPr><p:cNvPr id=\"9\"/><p:nvPr/></p:nvSpPr>\
                 <p:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"9144000\" cy=\"6858000\"/></a:xfrm></p:spPr>\
                 <p:style><a:fillRef idx=\"1\"><a:schemeClr val=\"accent1\"/></a:fillRef></p:style></p:sp>"
            )
        };
        let doc = chained("usebg", &sp("useBgFill=\"1\""), "").render(None);
        let html = body(&doc);
        assert!(html.contains("background-color:transparent"), "{html}");
        // Without the attribute the same shape still takes its accent fill.
        let doc = chained("nousebg", &sp(""), "").render(None);
        assert!(!body(&doc).contains("background-color:transparent"), "{}", doc.html);
    }

    #[test]
    fn an_empty_body_pr_keeps_the_inherited_anchor_and_autofit() {
        // Producers write `<a:bodyPr/>` on the shape constantly. Letting it win
        // outright resets a bottom-anchored title to the top of its box, which is
        // what draws it over the body text beneath.
        let f = Fixture::new(
            "bodypr",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
                ("ppt/slides/slide1.xml", sld(&ph_sp("title", "café"))),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    rels(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                (
                    "ppt/slideLayouts/slideLayout1.xml",
                    "<p:sldLayout xmlns:p=\"p\" xmlns:a=\"a\"><p:cSld><p:spTree>\
                     <p:sp><p:nvSpPr><p:cNvPr id=\"2\"/><p:nvPr><p:ph type=\"title\"/></p:nvPr></p:nvSpPr>\
                     <p:spPr/><p:txBody><a:bodyPr anchor=\"b\"><a:normAutofit/></a:bodyPr></p:txBody>\
                     </p:sp></p:spTree></p:cSld></p:sldLayout>"
                        .to_string(),
                ),
            ],
        );
        let doc = f.render(None);
        let html = body(&doc);
        assert!(html.contains("justify-content:flex-end"), "{html}");
        // ...and the autofit the layout declares reaches the frame, with every
        // size inside the box scalable by the pass that runs there.
        assert!(html.contains("data-af=\"1\""), "{html}");
        assert!(html.contains("font-size:calc("), "{html}");
    }

    #[test]
    fn a_chart_frame_becomes_a_labelled_placeholder() {
        let body = "<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"6\"/><p:nvPr/></p:nvGraphicFramePr>\
                    <p:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"914400\" cy=\"914400\"/></p:xfrm>\
                    <a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"/>\
                    </a:graphic></p:graphicFrame>";
        let doc = chained("chart", body, "").render(None);
        assert!(doc.html.contains("pp-ph"), "{}", doc.html);
        assert!(doc.html.contains("chart"), "{}", doc.html);
        assert!(
            doc.notes.iter().any(|n| n.contains("Charts")),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn text_without_a_position_is_reported_rather_than_dropped_silently() {
        let doc = chained("nogeom", &ph_sp_no_xfrm("body", "café"), "").render(None);
        assert!(
            doc.notes.iter().any(|n| n.contains("no position")),
            "{:?}",
            doc.notes
        );
    }

    #[test]
    fn placeholder_geometry_comes_from_the_layout() {
        let f = Fixture::new(
            "inherit",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
                ("ppt/slides/slide1.xml", sld(&ph_sp_no_xfrm("title", "café"))),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    rels(&[("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
                ),
                (
                    "ppt/slideLayouts/slideLayout1.xml",
                    format!(
                        "<p:sldLayout xmlns:p=\"p\" xmlns:a=\"a\"><p:cSld><p:spTree>\
                         <p:sp><p:nvSpPr><p:cNvPr id=\"2\"/><p:nvPr><p:ph type=\"title\"/></p:nvPr></p:nvSpPr>\
                         <p:spPr><a:xfrm><a:off x=\"457200\" y=\"274638\"/><a:ext cx=\"8229600\" cy=\"1143000\"/></a:xfrm></p:spPr>\
                         </p:sp></p:spTree></p:cSld></p:sldLayout>"
                    ),
                ),
            ],
        );
        let doc = f.render(None);
        // 457200 EMU = 48px, 274638 ≈ 28.83px, 8229600 = 864px.
        assert!(doc.html.contains("left:48px"), "{}", doc.html);
        assert!(doc.html.contains("width:864px"), "{}", doc.html);
        assert!(doc.html.contains("café"), "{}", doc.html);
    }

    // ── content index ───────────────────────────────────────────────────────

    #[test]
    fn indexed_sections_follow_the_slide_list_not_part_names() {
        // `deck` stores the first slide as `slide2.xml`. The old extractor sorted
        // `ppt/slides/` by name, so its section 0 was the renderer's section 1 —
        // every preview opened from a content hit would show the other slide.
        let f = deck("index-order");
        let sections = extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), f.render(None).sections.len());
        assert!(sections[0].text.contains("café first"), "{:?}", sections[0].text);
        assert!(sections[1].text.contains("naïve second"), "{:?}", sections[1].text);
        // A slide's name is not indexed: its title is already in its own text.
        assert_eq!(sections[0].name, "");
    }

    #[test]
    fn speaker_notes_are_not_indexed() {
        // The preview does not draw a notes page, so a hit there could never be
        // highlighted — and every slide of a deck carries one.
        let f = Fixture::new(
            "index-notes",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
                ("ppt/slides/slide1.xml", sld(&ph_sp("title", "café"))),
                (
                    "ppt/notesSlides/notesSlide1.xml",
                    sld("<p:sp><p:txBody><a:p><a:r><a:t>naïve-note</a:t></a:r></a:p></p:txBody></p:sp>"),
                ),
            ],
        );
        let sections = extract_sections(f.path()).expect("extract");
        assert_eq!(sections.len(), 1);
        assert!(sections[0].text.contains("café"));
        assert!(!sections[0].text.contains("naïve-note"), "{:?}", sections[0].text);
    }

    #[test]
    fn a_line_break_separates_indexed_words() {
        // `<a:br/>` carries no characters. Emitting nothing for it fused the runs
        // on either side into one token, which no query could ever match.
        let f = Fixture::new(
            "index-br",
            &[
                (
                    "_rels/.rels",
                    rels(&[("rId1", "officeDocument", "ppt/presentation.xml")]),
                ),
                (
                    "ppt/presentation.xml",
                    "<p:presentation xmlns:p=\"p\" xmlns:r=\"r\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
                     <p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                        .to_string(),
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    rels(&[("rId1", "slide", "slides/slide1.xml")]),
                ),
                (
                    "ppt/slides/slide1.xml",
                    sld(
                        "<p:sp><p:txBody><a:p>\
                           <a:r><a:t>café</a:t></a:r><a:br/><a:r><a:t>naïve</a:t></a:r>\
                         </a:p></p:txBody></p:sp>",
                    ),
                ),
            ],
        );
        let text = &extract_sections(f.path()).expect("extract")[0].text;
        assert!(text.contains("café naïve"), "{text:?}");
    }
}
