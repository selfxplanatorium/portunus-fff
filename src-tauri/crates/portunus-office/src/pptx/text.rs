//! pptx text bodies: the DrawingML property cascade, resolved onto the
//! format-neutral paragraph model in [`super::super::model`].
//!
//! Text properties in a presentation are resolved through a chain, weakest
//! first: `p:defaultTextStyle` (presentation) → the master's `p:txStyles`
//! entry for the shape's kind → the layout placeholder's `a:lstStyle` → the
//! shape's own `a:lstStyle` → the paragraph's `a:pPr` → the run's `a:rPr`.
//! Each link contributes only the properties it actually states, which is why
//! every field below is an `Option` and every merge overwrites nothing it has
//! no value for.
//!
//! What comes out is a `Vec<Para>` holding the *outcome* of that chain, with no
//! DrawingML left in it; the emitters below add only the text box the
//! paragraphs sit in and the pptx class names.
//!
//! Nothing here measures text, so line spacing, autofit scales and bullet
//! hangs are geometric approximations of what PowerPoint computes from real
//! font metrics. See the fidelity notes in the plan.

use super::super::drawingml::color::{parse_color_elem_map, Color};
use super::super::drawingml::theme::{ClrMap, SchemeSlot, Theme};
use super::super::fonts;
use super::super::html::{attr, fmt_px, hundredths_pt_to_px, Style, Writer};
use super::super::listnum::auto_num;
use super::super::model::{
    self, Align, Break, Caps, HtmlStyle, LineHeight, ListMarker, Para, Run, Script, TextRun,
    SINGLE_LINE,
};
use super::super::xml::{child, elems, has_inner_text, inner_text, truthy};
use super::Ctx;
use roxmltree::Node;

/// Font size used when no link in the cascade states one. PowerPoint's own
/// fallback is the master's body style, which is nearly always present; this is
/// only for documents that ship without one.
const DEFAULT_SZ_PT: f32 = 18.0;

/// The pptx spelling of the paragraph model; `scalable` is filled in per body
/// from its autofit.
const HTML: HtmlStyle = HtmlStyle {
    para_class: "pp-p",
    list_class: "pp-p pp-li",
    marker_class: "pp-bu",
    text_class: "pp-tx",
    // A slide has no pages and no columns, so `a:br` is only ever a line break.
    break_class: "",
    // A slide's pictures are shapes, placed absolutely by `shapes.rs`, so no
    // `Graphic` run is ever built here and neither class is ever emitted.
    img_class: "",
    graphic_class: "",
    scalable: false,
};

/// Paragraphs per text body, and runs per paragraph. A generated deck can carry
/// thousands; the shape is a fixed box on a 960px canvas, so nothing past this
/// could be read anyway.
const MAX_PARAS: usize = 400;
const MAX_RUNS: usize = 300;

/// Indent levels DrawingML defines (`a:lvl1pPr` … `a:lvl9pPr`).
pub const LEVELS: usize = 9;

// ── properties ───────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct RunProps {
    pub sz_pt: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub color: Option<Color>,
    /// Already through `fonts::css_font_stack`, i.e. safe to put in CSS.
    pub font: Option<String>,
    /// The raw typeface name, kept for the symbol-font remap.
    pub font_raw: Option<String>,
    pub caps: Option<Caps>,
    pub spc_pt: Option<f32>,
    /// `baseline`: positive is superscript, negative subscript, 0 neither.
    pub baseline: i32,
    /// A run carrying `a:hlinkClick`. Rendered as link-coloured text without an
    /// `href`: the frame has no navigation and no network, so a live link would
    /// be a dead end that looks clickable.
    pub link: bool,
}

#[derive(Clone)]
pub enum Bullet {
    /// Nothing at this link of the cascade — keep whatever the weaker link said.
    Inherit,
    None,
    Char(String),
    /// (`a:buAutoNum@type`, `startAt`)
    AutoNum(String, u32),
}

impl Default for Bullet {
    fn default() -> Self {
        Bullet::Inherit
    }
}

#[derive(Clone, Default)]
pub struct ParaProps {
    pub align: Option<Align>,
    pub mar_l: Option<f32>,
    /// First-line offset, normally negative (the bullet hangs left of the text).
    pub indent: Option<f32>,
    pub line_pct: Option<f32>,
    pub line_px: Option<f32>,
    pub before_px: Option<f32>,
    pub after_px: Option<f32>,
    pub bullet: Bullet,
    pub bu_color: Option<Color>,
    pub bu_font: Option<String>,
    pub bu_font_raw: Option<String>,
    pub bu_size_pct: Option<f32>,
    pub rtl: Option<bool>,
    /// `a:defRPr` at this link: the run properties every run in the paragraph
    /// starts from.
    pub run: RunProps,
}

fn emu(v: &str) -> Option<f32> {
    v.parse::<i64>()
        .ok()
        .map(super::super::html::emu_to_px)
        .filter(|f| f.is_finite())
}

fn pct(v: &str) -> Option<f32> {
    v.parse::<f32>().ok().map(|p| p / 100_000.0)
}

/// Resolves `+mj-lt` / `+mn-lt` theme font references and hands back both the
/// CSS stack and the raw name (the symbol remap keys off the raw name).
fn typeface(name: &str, theme: &Theme) -> (String, String) {
    let raw = match name {
        "+mj-lt" | "+mj-ea" | "+mj-cs" => theme.major_font.clone(),
        "+mn-lt" | "+mn-ea" | "+mn-cs" => theme.minor_font.clone(),
        other => other.to_string(),
    };
    (fonts::css_font_stack(&raw), raw)
}

/// Merges one `a:rPr` / `a:defRPr` / `a:endParaRPr` into `p`.
pub fn merge_run(p: &mut RunProps, n: Node, theme: &Theme, map: &ClrMap) {
    if let Some(v) = n.attribute("sz") {
        if let Ok(hp) = v.parse::<i64>() {
            let pt = hp as f32 / 100.0;
            if pt > 0.0 && pt < 4000.0 {
                p.sz_pt = Some(pt);
            }
        }
    }
    if let Some(v) = n.attribute("b") {
        p.bold = Some(truthy(v));
    }
    if let Some(v) = n.attribute("i") {
        p.italic = Some(truthy(v));
    }
    if let Some(v) = n.attribute("u") {
        p.underline = Some(v != "none");
    }
    if let Some(v) = n.attribute("strike") {
        p.strike = Some(v != "noStrike");
    }
    if let Some(v) = n.attribute("cap") {
        p.caps = match v {
            "all" => Some(Caps::All),
            "small" => Some(Caps::Small),
            _ => None,
        };
    }
    if let Some(v) = n.attribute("spc") {
        if let Ok(hp) = v.parse::<i64>() {
            p.spc_pt = Some(hp as f32 / 100.0);
        }
    }
    if let Some(v) = n.attribute("baseline") {
        if let Ok(b) = v.parse::<f32>() {
            p.baseline = if b > 0.0 {
                1
            } else if b < 0.0 {
                -1
            } else {
                0
            };
        }
    }
    if let Some(sf) = child(n, "solidFill") {
        if let Some(c) = parse_color_elem_map(sf, theme, map, None) {
            p.color = Some(c);
        }
    }
    if let Some(latin) = child(n, "latin") {
        if let Some(tf) = latin.attribute("typeface").filter(|t| !t.is_empty()) {
            let (css, raw) = typeface(tf, theme);
            p.font = Some(css);
            p.font_raw = Some(raw);
        }
    }
    if child(n, "hlinkClick").is_some() {
        p.link = true;
    }
}

/// Merges one `a:pPr` / `a:lvlNpPr` into `p`, including its `a:defRPr`.
pub fn merge_para(p: &mut ParaProps, n: Node, theme: &Theme, map: &ClrMap) {
    if let Some(v) = n.attribute("algn") {
        p.align = match v {
            "l" => Some(Align::Left),
            "ctr" => Some(Align::Center),
            "r" => Some(Align::Right),
            "just" | "justLow" => Some(Align::Justify),
            "dist" | "thaiDist" => Some(Align::Justify),
            _ => None,
        };
    }
    if let Some(v) = n.attribute("marL").and_then(emu) {
        p.mar_l = Some(v);
    }
    if let Some(v) = n.attribute("indent").and_then(emu) {
        p.indent = Some(v);
    }
    if let Some(v) = n.attribute("rtl") {
        p.rtl = Some(truthy(v));
    }
    if let Some(ln) = child(n, "lnSpc") {
        if let Some(sp) = child(ln, "spcPct").and_then(|e| e.attribute("val")).and_then(pct) {
            p.line_pct = Some(sp);
            p.line_px = None;
        } else if let Some(v) = child(ln, "spcPts").and_then(|e| e.attribute("val")) {
            if let Ok(hp) = v.parse::<i64>() {
                p.line_px = Some(hundredths_pt_to_px(hp));
                p.line_pct = None;
            }
        }
    }
    for (tag, slot) in [("spcBef", 0usize), ("spcAft", 1usize)] {
        if let Some(sp) = child(n, tag) {
            // Percentage space-before is a fraction of a line, which needs the
            // resolved font size; it is applied at emit time via `line_px`-free
            // points only, so a percentage is dropped rather than guessed.
            let px = child(sp, "spcPts")
                .and_then(|e| e.attribute("val"))
                .and_then(|v| v.parse::<i64>().ok())
                .map(hundredths_pt_to_px);
            if let Some(px) = px {
                if slot == 0 {
                    p.before_px = Some(px);
                } else {
                    p.after_px = Some(px);
                }
            }
        }
    }
    if child(n, "buNone").is_some() {
        p.bullet = Bullet::None;
    }
    if let Some(bc) = child(n, "buChar") {
        if let Some(ch) = bc.attribute("char").filter(|c| !c.is_empty()) {
            p.bullet = Bullet::Char(ch.chars().take(4).collect());
        }
    }
    if let Some(an) = child(n, "buAutoNum") {
        let ty = an.attribute("type").unwrap_or("arabicPeriod").to_string();
        let start = an
            .attribute("startAt")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1)
            .clamp(1, 32_767);
        p.bullet = Bullet::AutoNum(ty, start);
    }
    if let Some(cf) = child(n, "buClr") {
        if let Some(c) = parse_color_elem_map(cf, theme, map, None) {
            p.bu_color = Some(c);
        }
    }
    if let Some(bf) = child(n, "buFont") {
        if let Some(tf) = bf.attribute("typeface").filter(|t| !t.is_empty()) {
            let (css, raw) = typeface(tf, theme);
            p.bu_font = Some(css);
            p.bu_font_raw = Some(raw);
        }
    }
    if let Some(bs) = child(n, "buSzPct") {
        if let Some(v) = bs.attribute("val").and_then(pct) {
            p.bu_size_pct = Some(v.clamp(0.25, 4.0));
        }
    }
    if let Some(dr) = child(n, "defRPr") {
        merge_run(&mut p.run, dr, theme, map);
    }
}

// ── the cascade ──────────────────────────────────────────────────────────────

/// The chain of `a:lstStyle`-shaped nodes for one shape, weakest link first.
/// Each node is expected to carry `a:lvl1pPr` … `a:lvl9pPr` children.
#[derive(Default)]
pub struct Cascade<'a> {
    pub sources: Vec<Node<'a, 'a>>,
}

impl<'a> Cascade<'a> {
    pub fn push(&mut self, n: Option<Node<'a, 'a>>) {
        if let Some(n) = n {
            self.sources.push(n);
        }
    }

    /// Resolved properties for one indent level, before the paragraph's own
    /// `a:pPr` and the runs' `a:rPr` are applied.
    pub fn level(&self, lvl: usize, theme: &Theme, map: &ClrMap) -> ParaProps {
        let mut p = ParaProps::default();
        for src in &self.sources {
            // Level 1 properties act as the base for deeper levels in Office
            // when a level is skipped, so apply lvl1 first and then the
            // requested level on top of it.
            if lvl > 0 {
                if let Some(n) = lvl_node(*src, 0) {
                    merge_para(&mut p, n, theme, map);
                }
            }
            if let Some(n) = lvl_node(*src, lvl) {
                merge_para(&mut p, n, theme, map);
            }
        }
        p
    }
}

/// `a:lvl{n+1}pPr` child of an `a:lstStyle`-shaped node.
pub fn lvl_node<'a>(src: Node<'a, 'a>, lvl: usize) -> Option<Node<'a, 'a>> {
    let want = format!("lvl{}pPr", lvl.min(LEVELS - 1) + 1);
    elems(src).find(|c| c.tag_name().name() == want)
}

// ── body properties ──────────────────────────────────────────────────────────

pub struct BodyOpts {
    /// Flex `justify-content` for the vertical anchor.
    pub anchor: &'static str,
    /// `a:bodyPr@anchorCtr`: centre the text block horizontally in the box.
    pub anchor_ctr: bool,
    /// Text insets, px: left, top, right, bottom.
    pub ins: (f32, f32, f32, f32),
    pub wrap: bool,
    /// `a:normAutofit@fontScale`, 1.0 when absent.
    pub font_scale: f32,
    /// `a:normAutofit@lnSpcReduction` as a fraction.
    pub ln_reduce: f32,
    /// True for `a:bodyPr@vert` values that rotate the text; only noted, not
    /// rendered.
    pub vertical: bool,
    /// `a:normAutofit` is present: the box shrinks its text to fit. PowerPoint
    /// bakes a `fontScale` computed against *its* font metrics, which are not the
    /// substituted ones, so the frame re-fits the box after layout. See the
    /// autofit pass in `officeBootstrap`.
    pub autofit: bool,
}

impl Default for BodyOpts {
    fn default() -> Self {
        BodyOpts {
            anchor: "flex-start",
            anchor_ctr: false,
            // DrawingML's own defaults: 0.1in left/right, 0.05in top/bottom.
            ins: (9.6, 4.8, 9.6, 4.8),
            wrap: true,
            font_scale: 1.0,
            ln_reduce: 0.0,
            vertical: false,
            autofit: false,
        }
    }
}

/// `a:bodyPr` resolved over the placeholder chain, **most specific first**
/// (shape, layout, master).
///
/// Merged attribute by attribute rather than element-wins: producers write an
/// empty `<a:bodyPr/>` on the shape constantly, and letting it win would reset
/// the anchor to top and the insets to the DrawingML defaults on every inheriting
/// placeholder — which is how a bottom-anchored title ends up drawn over the body
/// text beneath it.
pub fn parse_body_pr(chain: &[Option<Node>]) -> BodyOpts {
    let mut o = BodyOpts::default();
    for n in chain.iter().rev().flatten() {
        merge_body_pr(&mut o, *n);
    }
    o
}

fn merge_body_pr(o: &mut BodyOpts, n: Node) {
    if let Some(v) = n.attribute("anchor") {
        o.anchor = match v {
            "ctr" => "center",
            "b" => "flex-end",
            "just" | "dist" => "space-between",
            _ => "flex-start",
        };
    }
    if let Some(v) = n.attribute("anchorCtr") {
        o.anchor_ctr = truthy(v);
    }
    for (name, slot) in [("lIns", 0), ("tIns", 1), ("rIns", 2), ("bIns", 3)] {
        if let Some(px) = n.attribute(name).and_then(emu) {
            let px = px.clamp(0.0, 400.0);
            match slot {
                0 => o.ins.0 = px,
                1 => o.ins.1 = px,
                2 => o.ins.2 = px,
                _ => o.ins.3 = px,
            }
        }
    }
    if let Some(v) = n.attribute("wrap") {
        o.wrap = v != "none";
    }
    if let Some(v) = n.attribute("vert") {
        o.vertical = v != "horz";
    }
    if let Some(af) = child(n, "normAutofit") {
        o.autofit = true;
        // A stated scale replaces an inherited one; an unstated one means "no
        // shrink was needed at authoring time", not "keep the parent's".
        o.font_scale = af
            .attribute("fontScale")
            .and_then(pct)
            .map(|s| s.clamp(0.25, 1.0))
            .unwrap_or(1.0);
        o.ln_reduce = af
            .attribute("lnSpcReduction")
            .and_then(pct)
            .map(|r| r.clamp(0.0, 0.8))
            .unwrap_or(0.0);
    } else if child(n, "noAutofit").is_some() || child(n, "spAutoFit").is_some() {
        // Stated explicitly at this level: the text overflows (or the box grows,
        // which a fixed preview box cannot do), so no shrink.
        o.autofit = false;
        o.font_scale = 1.0;
        o.ln_reduce = 0.0;
    }
}

// ── ooxml → model ────────────────────────────────────────────────────────────

/// Resolves every `a:p` of a text body against the cascade, into paragraphs
/// that carry no DrawingML.
///
/// The autofit font scale is folded into the sizes here rather than at emit
/// time: it applies to the paragraph's size and to each run's equally, and the
/// emitter's "does this run differ from its paragraph?" test only holds if both
/// sides have already been scaled.
pub fn parse_paras(
    tb: Node,
    cas: &Cascade,
    opts: &BodyOpts,
    theme: &Theme,
    map: &ClrMap,
) -> Vec<Para> {
    // One counter per indent level for `a:buAutoNum`; a deeper level restarts
    // whenever a shallower paragraph intervenes, which is how PowerPoint
    // numbers nested lists.
    let mut counters = [0u32; LEVELS];
    let mut out = Vec::new();
    for para in elems(tb) {
        if para.tag_name().name() != "p" {
            continue;
        }
        if out.len() >= MAX_PARAS {
            break;
        }
        out.push(parse_para(para, cas, opts, theme, map, &mut counters));
    }
    out
}

fn parse_para(
    para: Node,
    cas: &Cascade,
    opts: &BodyOpts,
    theme: &Theme,
    map: &ClrMap,
    counters: &mut [u32; LEVELS],
) -> Para {
    let ppr = child(para, "pPr");
    let lvl = ppr
        .and_then(|n| n.attribute("lvl"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(LEVELS - 1);
    let mut pp = cas.level(lvl, theme, map);
    if let Some(n) = ppr {
        merge_para(&mut pp, n, theme, map);
    }

    // Runs first, so the paragraph's own font size can be the first run's
    // (it sizes the bullet and an otherwise empty line).
    let nodes: Vec<Node> = elems(para)
        .filter(|c| matches!(c.tag_name().name(), "r" | "br" | "fld"))
        .take(MAX_RUNS)
        .collect();
    let first_rpr = nodes
        .iter()
        .find(|r| r.tag_name().name() == "r")
        .and_then(|r| child(*r, "rPr"));
    let mut base = pp.run.clone();
    if let Some(n) = first_rpr {
        merge_run(&mut base, n, theme, map);
    }
    if nodes.is_empty() {
        // An empty paragraph is a spacer whose height comes from
        // `a:endParaRPr`.
        if let Some(n) = child(para, "endParaRPr") {
            merge_run(&mut base, n, theme, map);
        }
    }
    let base_pt = base.sz_pt.unwrap_or(DEFAULT_SZ_PT) * opts.font_scale;

    // A blank line in a bulleted list carries no bullet in PowerPoint and does
    // not consume a number either, so an empty paragraph asks for neither —
    // which is also why the counters are only advanced when one is asked for.
    let marker = if nodes.iter().any(|r| any_text(*r)) {
        bullet_label(&pp, lvl, counters).map(|label| ListMarker {
            label,
            color: pp.bu_color,
            // A symbol bullet font is only useful for the glyph the remap could
            // not resolve; the remapped glyph reads better in the text font.
            font: pp
                .bu_font
                .clone()
                .filter(|_| !fonts::is_symbol_font(pp.bu_font_raw.as_deref().unwrap_or(""))),
            size_pt: pp.bu_size_pct.map(|p| base_pt * p),
        })
    } else {
        None
    };

    let mut runs = Vec::with_capacity(nodes.len());
    for r in nodes {
        match r.tag_name().name() {
            "br" => runs.push(Run::Break(Break::Line)),
            // A field (slide number, date) renders its cached text; the live
            // value is not computable here.
            "r" | "fld" => {
                let mut rp = pp.run.clone();
                if let Some(n) = child(r, "rPr") {
                    merge_run(&mut rp, n, theme, map);
                }
                let text = run_text(r, &rp);
                runs.push(Run::Text(parse_run(text, &rp, base_pt, opts.font_scale, theme)));
            }
            _ => {}
        }
    }

    Para {
        runs,
        size_pt: base_pt,
        align: pp.align,
        indent_px: pp.mar_l.unwrap_or(0.0),
        first_line_px: pp.indent.unwrap_or(0.0),
        line: match pp.line_px {
            Some(px) => LineHeight::Exact(px),
            None => LineHeight::Multiple(
                pp.line_pct.unwrap_or(1.0) * SINGLE_LINE * (1.0 - opts.ln_reduce),
            ),
        },
        space_before_px: pp.before_px.unwrap_or(0.0),
        space_after_px: pp.after_px.unwrap_or(0.0),
        marker,
        rtl: pp.rtl == Some(true),
        // DrawingML paints the shape, not the paragraph: a slide's fills and
        // outlines are emitted by the shape renderer.
        ..Default::default()
    }
}

fn parse_run(text: String, rp: &RunProps, base_pt: f32, scale: f32, theme: &Theme) -> TextRun {
    TextRun {
        text,
        // A run that states no size of its own renders at the paragraph's, so
        // that is what it carries — the model has no "inherit" to express.
        size_pt: rp.sz_pt.map(|v| v * scale).unwrap_or(base_pt),
        bold: rp.bold == Some(true),
        italic: rp.italic == Some(true),
        underline: rp.underline == Some(true) || rp.link,
        strike: rp.strike == Some(true),
        // An unstyled hyperlink takes the theme's link colour; anything else
        // inherits the shape's.
        color: rp
            .color
            .or_else(|| rp.link.then(|| Color::from_rgb(theme.color(SchemeSlot::Hlink)))),
        font: rp.font.clone(),
        caps: rp.caps,
        letter_spacing_pt: rp.spc_pt.unwrap_or(0.0),
        script: match rp.baseline {
            1 => Some(Script::Super),
            -1 => Some(Script::Sub),
            _ => None,
        },
        // DrawingML has no run highlight.
        highlight: None,
        // Deliberately never an `<a>`: a slide's link is styled like one (above)
        // but carries no destination, because `a:hlinkClick` on a slide most often
        // targets another slide in the deck — a jump this preview cannot make, and
        // a clickable dead end reads worse than link-coloured text.
        link: None,
    }
}

/// Whether a run carries any non-whitespace text. Checked before the bullet is
/// resolved, so it must not allocate the text to find out.
fn any_text(r: Node) -> bool {
    if !matches!(r.tag_name().name(), "r" | "fld") {
        return false;
    }
    elems(r)
        .filter(|t| t.tag_name().name() == "t")
        .any(has_inner_text)
}

/// Concatenated `a:t` text of a run, with symbol-font code points mapped to
/// their Unicode equivalents so Wingdings bullets and Symbol Greek do not
/// render as boxes.
fn run_text(r: Node, rp: &RunProps) -> String {
    let mut s = String::new();
    for t in elems(r).filter(|t| t.tag_name().name() == "t") {
        inner_text(t, &mut s);
    }
    let symbol = rp
        .font_raw
        .as_deref()
        .map(fonts::is_symbol_font)
        .unwrap_or(false);
    if !symbol {
        return s;
    }
    let font = rp.font_raw.as_deref().unwrap_or("");
    s.chars()
        .map(|c| fonts::remap(font, c).unwrap_or(c))
        .collect()
}

/// The bullet this paragraph shows, advancing the auto-number counters. `None` for
/// an unbulleted paragraph — which still resets the counters, because a plain
/// paragraph between two numbered ones restarts the numbering in PowerPoint.
fn bullet_label(pp: &ParaProps, lvl: usize, counters: &mut [u32; LEVELS]) -> Option<String> {
    let label = match &pp.bullet {
        Bullet::Inherit | Bullet::None => {
            counters[lvl] = 0;
            return None;
        }
        Bullet::Char(c) => {
            let raw = pp.bu_font_raw.as_deref().unwrap_or("");
            if fonts::is_symbol_font(raw) {
                c.chars().map(|ch| fonts::remap(raw, ch).unwrap_or(ch)).collect()
            } else {
                c.clone()
            }
        }
        Bullet::AutoNum(ty, start) => {
            let n = if counters[lvl] == 0 {
                *start
            } else {
                counters[lvl].saturating_add(1)
            };
            counters[lvl] = n;
            for c in counters.iter_mut().skip(lvl + 1) {
                *c = 0;
            }
            auto_num(ty, n)
        }
    };
    if let Bullet::Char(_) = pp.bullet {
        counters[lvl] = 0;
    }
    for c in counters.iter_mut().skip(lvl + 1) {
        *c = 0;
    }
    Some(label)
}

// ── model → html ─────────────────────────────────────────────────────────────

/// Emits the absolutely-positioned text box of a shape, then its paragraphs.
/// The insets come off the shape's box.
pub fn emit_body(ctx: &mut Ctx, w: &mut Writer, paras: &[Para], opts: &BodyOpts) {
    let mut st = Style::new();
    st.push_opt("left", fmt_px(opts.ins.0));
    st.push_opt("top", fmt_px(opts.ins.1));
    st.push_opt("right", fmt_px(opts.ins.2));
    st.push_opt("bottom", fmt_px(opts.ins.3));
    st.push("justify-content", opts.anchor);
    if opts.anchor_ctr {
        st.push("align-items", "center");
    }
    if !opts.wrap {
        st.push("white-space", "pre");
    }
    // `data-af` is what the frame's autofit pass looks for; the paragraph sizes it
    // scales are written as `calc(… * var(--af,1))`.
    let af = if opts.autofit {
        attr("data-af", "1")
    } else {
        String::new()
    };
    w.open(
        "div",
        &super::super::html::attrs(&[&attr("class", "pp-tb"), &af, &st.to_attr()]),
    );
    // The paragraphs live in their own block so the autofit pass can measure how
    // tall the text actually is: a bottom-anchored box that overflows does so
    // *upwards*, where `scrollHeight` cannot see it.
    w.open("div", &super::super::html::attrs(&[&attr("class", "pp-tbi")]));
    emit_paras(ctx, w, paras, opts);
    w.close();
    w.close();
}

/// Emits just the paragraphs of a text body — used directly by table cells,
/// which supply their own box.
pub fn emit_paras(ctx: &mut Ctx, w: &mut Writer, paras: &[Para], opts: &BodyOpts) {
    let st = HtmlStyle {
        scalable: opts.autofit,
        ..HTML
    };
    model::emit_paras(w, paras, &st, ctx.marker, ctx.terms);
}

/// Flat text of a text body, for slide titles and the section list.
pub fn plain_text(tb: Node, limit: usize) -> String {
    let mut out = String::new();
    for para in elems(tb) {
        if para.tag_name().name() != "p" {
            continue;
        }
        let mut line = String::new();
        for r in elems(para) {
            match r.tag_name().name() {
                "r" | "fld" => {
                    for t in elems(r).filter(|t| t.tag_name().name() == "t") {
                        inner_text(t, &mut line);
                    }
                }
                "br" => line.push(' '),
                _ => {}
            }
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
        if out.chars().count() >= limit {
            break;
        }
    }
    let clipped = out.chars().count() > limit;
    let mut s: String = out.chars().take(limit).collect();
    if clipped {
        s = s.trim_end().to_string();
        s.push('…');
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_applies_level_one_under_deeper_levels() {
        let xml = "<lstStyle xmlns='a'><lvl1pPr algn='ctr'><defRPr sz='2000'/></lvl1pPr>\
                   <lvl2pPr marL='914400'/></lstStyle>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let mut cas = Cascade::default();
        cas.push(Some(doc.root_element()));
        let theme = Theme::default();
        let map = ClrMap::default();
        let p = cas.level(1, &theme, &map);
        // lvl2 states only marL, so alignment and size come from lvl1.
        assert_eq!(p.align, Some(Align::Center));
        assert_eq!(p.run.sz_pt, Some(20.0));
        assert_eq!(p.mar_l.map(|v| v.round()), Some(96.0));
    }

    #[test]
    fn plain_text_joins_paragraphs_and_clips() {
        let xml = "<txBody xmlns='a'><p><r><t>café</t></r></p><p><r><t>naïve</t></r></p></txBody>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        assert_eq!(plain_text(doc.root_element(), 64), "café naïve");
        assert_eq!(plain_text(doc.root_element(), 5), "café…");
    }
}
