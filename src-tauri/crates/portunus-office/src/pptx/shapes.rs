//! The `p:spTree` walk: shapes, pictures, groups, connectors and graphic frames
//! → absolutely positioned boxes on the slide canvas.
//!
//! Everything is placed in slide coordinates (px at 96dpi), so the emitted
//! document needs no layout of its own and the frontend can scale the whole
//! canvas with one transform. DOM order is document order, which is also
//! PowerPoint's z-order.

use super::super::drawingml::color::{parse_color_elem_map, Color};
use super::super::drawingml::fill::{fill_css, parse_fill_opt, push_fill, BlipMode, Fill, SrcRect};
use super::super::drawingml::geom::{geom_css, parse_geom, parse_group_xfrm, parse_xfrm, GroupXf, Xf, MAX_GROUP_DEPTH};
use super::super::drawingml::line::{line_css, parse_line, Line};
use super::super::emit;
use super::super::html::{attr, attrs, emu_to_px, fmt_px, Style, Writer};
use super::super::media::{self, Media};
use super::super::opc::{self, Rels};
use super::super::xml::{attr_local, child, elems};
use super::inherit::{self, StyleKind};
use super::text::{self, Cascade};
use super::Ctx;
use roxmltree::Node;

/// Shapes emitted per slide. A slide is a fixed box; past this nothing new is
/// legible, and the frame has to lay out every box the walk emits.
const MAX_SHAPES: usize = 400;
const MAX_TABLE_ROWS: usize = 120;
const MAX_TABLE_COLS: usize = 40;

/// A connector thinner than this in one axis is drawn as a straight rule rather
/// than a rotated one, which avoids a sub-pixel rotation on every horizontal
/// line in a deck.
const RULE_EPS: f64 = 2.0;

/// Everything that depends on *which part* is being walked. Master and layout
/// shapes are drawn with the same code as slide shapes, but resolve their images
/// through their own relationships and take no part in placeholder inheritance.
pub struct Env<'a> {
    pub rels: &'a Rels,
    pub part: &'a str,
    /// `p:spTree` of the layout and master, for placeholder inheritance.
    pub layout: Option<Node<'a, 'a>>,
    pub master: Option<Node<'a, 'a>>,
    /// Master root, for `p:txStyles`.
    pub master_root: Option<Node<'a, 'a>>,
    /// `p:defaultTextStyle` from `presentation.xml`.
    pub default_text: Option<Node<'a, 'a>>,
    /// False when walking the layout's or master's own shapes: those are drawn
    /// as they stand, and their placeholders are not drawn at all (an empty
    /// placeholder shows prompt text only while editing).
    pub inherit: bool,
}

pub fn walk(ctx: &mut Ctx, w: &mut Writer, tree: Node, env: &Env, groups: &mut Vec<GroupXf>) {
    for node in elems(tree) {
        if w.is_full() || ctx.shapes >= MAX_SHAPES {
            return;
        }
        match node.tag_name().name() {
            "sp" => {
                // A layout or master placeholder is a prompt, not content.
                if !env.inherit && inherit::is_placeholder(node) {
                    continue;
                }
                ctx.shapes += 1;
                emit_sp(ctx, w, node, env, groups);
            }
            "pic" => {
                ctx.shapes += 1;
                emit_pic(ctx, w, node, env, groups);
            }
            "cxnSp" => {
                ctx.shapes += 1;
                emit_cxn(ctx, w, node, groups);
            }
            "graphicFrame" => {
                if !env.inherit && inherit::is_placeholder(node) {
                    continue;
                }
                ctx.shapes += 1;
                emit_frame(ctx, w, node, env, groups);
            }
            "grpSp" => {
                if inherit::is_hidden(node) || groups.len() >= MAX_GROUP_DEPTH {
                    continue;
                }
                let Some(gx) = child(node, "grpSpPr").and_then(parse_group_xfrm) else {
                    continue;
                };
                groups.push(gx);
                walk(ctx, w, node, env, groups);
                groups.pop();
            }
            // A raster alternative to a metafile, or a newer element with an
            // older fallback: take whichever branch this renderer can draw.
            "AlternateContent" => {
                if let Some(branch) = media::prefer_raster_branch(node) {
                    walk(ctx, w, branch, env, groups);
                }
            }
            _ => {}
        }
    }
}

// ── shapes ───────────────────────────────────────────────────────────────────

fn emit_sp(ctx: &mut Ctx, w: &mut Writer, sp: Node, env: &Env, groups: &[GroupXf]) {
    if inherit::is_hidden(sp) {
        return;
    }
    let ph = if env.inherit { inherit::ph_of(sp) } else { None };
    let lay = ph
        .as_ref()
        .and_then(|p| env.layout.and_then(|t| inherit::find_ph(t, p)));
    let mas = ph
        .as_ref()
        .and_then(|p| env.master.and_then(|t| inherit::find_ph(t, p)));

    let pr = child(sp, "spPr");
    let lay_pr = lay.and_then(|n| child(n, "spPr"));
    let mas_pr = mas.and_then(|n| child(n, "spPr"));

    // Position: the shape's own, else the placeholder it inherits from. A shape
    // with no transform anywhere has no box to draw.
    let own = pr
        .and_then(parse_xfrm)
        .or_else(|| lay_pr.and_then(parse_xfrm))
        .or_else(|| mas_pr.and_then(parse_xfrm));
    let Some(xf) = own else {
        // Dropping a decorative shape silently is fine; dropping one that
        // carries text is data the reader cannot see, so say so once.
        if child(sp, "txBody").is_some() {
            ctx.notes
                .add("Some text has no position");
        }
        return;
    };
    let xf = Xf::compose_nested(groups, &xf);

    let fill = resolve_fill(ctx, sp, pr, lay_pr, mas_pr);
    let line = resolve_line(ctx, sp, pr, lay_pr, mas_pr);
    // The extent handed to `parse_geom` is the shape's *own*, pre-group one: a
    // `custGeom` path with no coordinate space of its own is in EMU relative to
    // that box, and the composed extent would double-count the group's scale.
    let ext = Some((xf.cx, xf.cy));
    let geom = pr
        .map(|n| parse_geom(n, ext))
        .or_else(|| lay_pr.map(|n| parse_geom(n, ext)))
        .unwrap_or(super::super::drawingml::geom::Geom::Rect);

    let mut css = xf.css();
    css.push_str(&fill_css(&fill));
    if let Some(l) = line.as_ref() {
        css.push_str(&line_css(l));
    }
    css.push_str(&geom_css(&geom, xf.cx, xf.cy));
    // Text colour from the theme style reference, so runs that state no colour
    // of their own inherit through CSS exactly as they do in PowerPoint.
    if let Some(c) = style_ref_color(sp, ctx, "fontRef") {
        css.push_str(&format!("color:{};", c.css()));
    }
    let pic = match &fill {
        Fill::Picture(p) => Some(p.clone()),
        _ => None,
    };
    if pic.is_some() {
        css.push_str("overflow:hidden;");
    }

    w.open("div", &attrs(&[&attr("class", "pp-sp"), &attr("style", &css)]));
    if let Some(p) = pic {
        // A picture fill is drawn as a child image rather than a
        // `background-image`, so the crop maths is shared with `p:pic`.
        emit_image(ctx, w, env, &p.embed, p.crop.as_ref(), p.mode, xf.cx, xf.cy);
    }
    if let Some(tb) = child(sp, "txBody") {
        emit_text(ctx, w, tb, env, ph.as_ref(), lay, mas);
    }
    w.close();
}

/// Fill precedence: the shape's own, then the placeholder chain, then the theme
/// style reference. `parse_fill_opt` distinguishes "states `a:noFill`" from
/// "states nothing", which is what makes the chain work.
fn resolve_fill(
    ctx: &Ctx,
    sp: Node,
    pr: Option<Node>,
    lay_pr: Option<Node>,
    mas_pr: Option<Node>,
) -> Fill {
    for cand in [pr, lay_pr, mas_pr] {
        if let Some(f) = cand.and_then(|n| parse_fill_opt(n, ctx.theme, None)) {
            return f;
        }
    }
    // `p:sp@useBgFill` means "paint the slide's own background fill", which is
    // exactly what showing through does — the background is drawn behind at slide
    // scale, so transparent and re-painting it are the same pixels. It has to be
    // checked before the style reference, or a decorative full-bleed rectangle
    // (which is how PowerPoint writes "cover the layout art") floods the slide
    // with the theme's first accent colour.
    if truthy_attr(sp, "useBgFill") {
        return Fill::None;
    }
    // `a:fillRef` indexes the theme's format scheme, which this renderer does
    // not model; its colour is the visible part of it, so the shape becomes a
    // flat fill in the right hue instead of nothing at all.
    match style_ref_color(sp, ctx, "fillRef") {
        Some(c) => Fill::Solid(c),
        None => Fill::None,
    }
}

fn resolve_line(
    ctx: &Ctx,
    sp: Node,
    pr: Option<Node>,
    lay_pr: Option<Node>,
    mas_pr: Option<Node>,
) -> Option<Line> {
    for cand in [pr, lay_pr, mas_pr] {
        if let Some(l) = cand.and_then(|n| parse_line(n, ctx.theme, None)) {
            return Some(l);
        }
    }
    // As with `a:fillRef`: the theme's line style list is not modelled, so a
    // referenced outline becomes a hairline in the referenced colour.
    style_ref_color(sp, ctx, "lnRef").map(Line::hairline)
}

/// Colour of one `p:style` reference (`a:fillRef` / `a:lnRef` / `a:fontRef`).
///
/// `idx="0"` is not an index into the theme's style list — it is the explicit
/// "none" entry, which is how a text placeholder says it has no fill and no
/// outline. Treating it as a colour puts a box around every title on the deck.
fn style_ref_color(sp: Node, ctx: &Ctx, which: &str) -> Option<Color> {
    let style = child(sp, "style")?;
    let r = child(style, which)?;
    if r.attribute("idx") == Some("0") {
        return None;
    }
    parse_color_elem_map(r, ctx.theme, &ctx.clr_map, None)
}

fn emit_text(
    ctx: &mut Ctx,
    w: &mut Writer,
    tb: Node,
    env: &Env,
    ph: Option<&inherit::Ph>,
    lay: Option<Node>,
    mas: Option<Node>,
) {
    let kind = ph
        .map(|p| inherit::style_kind(&p.ty))
        .unwrap_or(StyleKind::Other);
    let lay_tb = lay.and_then(|n| child(n, "txBody"));
    let mas_tb = mas.and_then(|n| child(n, "txBody"));

    let mut cas = Cascade::default();
    cas.push(env.default_text);
    cas.push(inherit::tx_style(env.master_root, kind));
    cas.push(mas_tb.and_then(|t| child(t, "lstStyle")));
    cas.push(lay_tb.and_then(|t| child(t, "lstStyle")));
    cas.push(child(tb, "lstStyle"));

    let opts = text::parse_body_pr(&[
        child(tb, "bodyPr"),
        lay_tb.and_then(|t| child(t, "bodyPr")),
        mas_tb.and_then(|t| child(t, "bodyPr")),
    ]);
    if opts.vertical {
        ctx.notes
            .add("Vertical text shown horizontally");
    }
    let paras = text::parse_paras(tb, &cas, &opts, ctx.theme, &ctx.clr_map);
    text::emit_body(ctx, w, &paras, &opts);
}

// ── pictures ─────────────────────────────────────────────────────────────────

fn emit_pic(ctx: &mut Ctx, w: &mut Writer, pic: Node, env: &Env, groups: &[GroupXf]) {
    if inherit::is_hidden(pic) {
        return;
    }
    let pr = child(pic, "spPr");
    let Some(xf) = pr.and_then(parse_xfrm) else {
        return;
    };
    let xf = Xf::compose_nested(groups, &xf);

    let (embed, crop, mode) = match child(pic, "blipFill") {
        Some(bf) => {
            let embed = child(bf, "blip")
                .and_then(|b| attr_local(b, "embed"))
                .unwrap_or("")
                .to_string();
            let crop = child(bf, "srcRect").map(|r| SrcRect {
                l: pct_attr(r, "l"),
                t: pct_attr(r, "t"),
                r: pct_attr(r, "r"),
                b: pct_attr(r, "b"),
            });
            let mode = if child(bf, "tile").is_some() {
                BlipMode::Tile
            } else {
                BlipMode::Stretch
            };
            (embed, crop, mode)
        }
        None => (String::new(), None, BlipMode::Stretch),
    };

    let mut css = xf.css();
    css.push_str("overflow:hidden;");
    let geom = pr
        .map(|n| parse_geom(n, Some((xf.cx, xf.cy))))
        .unwrap_or(super::super::drawingml::geom::Geom::Rect);
    css.push_str(&geom_css(&geom, xf.cx, xf.cy));
    if let Some(l) = pr.and_then(|n| parse_line(n, ctx.theme, None)) {
        css.push_str(&line_css(&l));
    }
    w.open("div", &attrs(&[&attr("class", "pp-sp"), &attr("style", &css)]));
    emit_image(ctx, w, env, &embed, crop.as_ref(), mode, xf.cx, xf.cy);
    w.close();
}

/// `a:srcRect` insets are thousandths of a percent.
fn pct_attr(n: Node, name: &str) -> f64 {
    n.attribute(name)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v / 1000.0)
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 99.0)
}

/// The image itself, sized so that the *cropped* region fills the shape box.
/// The parent clips, so an oversized image is how a crop is expressed.
fn emit_image(
    ctx: &mut Ctx,
    w: &mut Writer,
    env: &Env,
    embed: &str,
    crop: Option<&SrcRect>,
    mode: BlipMode,
    cx: f64,
    cy: f64,
) {
    let want = cx.max(1.0).min(4096.0).round() as u32;
    let m = if embed.is_empty() {
        Media::Placeholder("image unavailable")
    } else {
        fetch_media(ctx, env, embed, want)
    };
    match m {
        Media::DataUri(uri) => {
            let mut st = Style::new();
            match crop {
                // Visible fraction of each axis; the full image is that much
                // larger than the box, and shifted left/up by the leading inset.
                Some(c) => {
                    let fw = (100.0 - c.l - c.r).max(1.0) / 100.0;
                    let fh = (100.0 - c.t - c.b).max(1.0) / 100.0;
                    let iw = cx / fw;
                    let ih = cy / fh;
                    st.push_opt("left", fmt_px(-(c.l / 100.0 * iw) as f32));
                    st.push_opt("top", fmt_px(-(c.t / 100.0 * ih) as f32));
                    st.push_opt("width", fmt_px(iw as f32));
                    st.push_opt("height", fmt_px(ih as f32));
                }
                None => {
                    st.push("left", "0");
                    st.push("top", "0");
                    st.push("width", "100%");
                    st.push("height", "100%");
                }
            }
            if matches!(mode, BlipMode::Tile) {
                // A tiled fill is not an image element; approximate with the
                // untiled bitmap stretched over the box and say so once.
            }
            w.void(
                "img",
                &attrs(&[
                    &attr("class", "pp-img"),
                    &attr("src", &uri),
                    &attr("alt", ""),
                    &st.to_attr(),
                ]),
            );
        }
        Media::Placeholder(reason) => placeholder(w, reason),
    }
}

/// A placeholder box filling the shape it sits in — the shape carries the
/// geometry, so the box only has to cover it.
fn placeholder(w: &mut Writer, label: &str) {
    emit::placeholder(w, "pp-ph", emit::FILL_PARENT, label);
}

fn fetch_media(ctx: &mut Ctx, env: &Env, rid: &str, want_px: u32) -> Media {
    let Some(part) = resolve_rel(env, rid) else {
        return Media::Placeholder("image unavailable");
    };
    ctx.media.get(ctx.zip, ctx.budget, ctx.mb, &part, want_px)
}

fn resolve_rel(env: &Env, rid: &str) -> Option<String> {
    let r = env.rels.get(rid)?;
    if r.external {
        return None;
    }
    opc::resolve_target(env.part, &r.target)
}

// ── connectors ───────────────────────────────────────────────────────────────

/// A connector is a line, not a box: drawing its bounding rectangle would put a
/// rectangle where the deck has a diagonal. Axis-aligned connectors become a
/// rule; anything else is a zero-height rule rotated onto the diagonal.
fn emit_cxn(ctx: &mut Ctx, w: &mut Writer, sp: Node, groups: &[GroupXf]) {
    if inherit::is_hidden(sp) {
        return;
    }
    let pr = child(sp, "spPr");
    let Some(xf) = pr.and_then(parse_xfrm) else {
        return;
    };
    let xf = Xf::compose_nested(groups, &xf);
    let line = pr
        .and_then(|n| parse_line(n, ctx.theme, None))
        .or_else(|| style_ref_color(sp, ctx, "lnRef").map(Line::hairline));
    let Some(line) = line.filter(|l| l.is_visible()) else {
        return;
    };
    // `line_css` emits a border on all four sides; a rule needs one edge, so the
    // width and colour are taken from the parsed line directly.
    let color = line.stroke_color();
    let dash = match line.dash {
        super::super::drawingml::line::Dash::Dashed => "dashed",
        super::super::drawingml::line::Dash::Dotted => "dotted",
        super::super::drawingml::line::Dash::Solid => "solid",
    };
    let mut st = Style::new();
    let (dx, dy) = (
        if xf.flip_h { -xf.cx } else { xf.cx },
        if xf.flip_v { -xf.cy } else { xf.cy },
    );
    if xf.cy.abs() < RULE_EPS || xf.cx.abs() < RULE_EPS {
        let vertical = xf.cx.abs() < RULE_EPS;
        st.push_opt("left", fmt_px(xf.x as f32));
        st.push_opt("top", fmt_px(xf.y as f32));
        st.push_opt("width", fmt_px(if vertical { 0.0 } else { xf.cx as f32 }));
        st.push_opt("height", fmt_px(if vertical { xf.cy as f32 } else { 0.0 }));
        st.push(
            if vertical { "border-left" } else { "border-top" },
            &rule(line.width_px, dash, &color),
        );
    } else {
        let len = (dx * dx + dy * dy).sqrt();
        let deg = dy.atan2(dx).to_degrees();
        st.push_opt("left", fmt_px(xf.x as f32));
        st.push_opt("top", fmt_px(xf.y as f32));
        st.push_opt("width", fmt_px(len as f32));
        st.push("height", "0");
        st.push("transform-origin", "0 0");
        st.push("transform", &format!("rotate({deg:.2}deg)"));
        st.push("border-top", &rule(line.width_px, dash, &color));
    }
    w.open("div", &attrs(&[&attr("class", "pp-sp"), &st.to_attr()]));
    w.close();
}

// ── graphic frames: tables, charts, diagrams ─────────────────────────────────

fn emit_frame(ctx: &mut Ctx, w: &mut Writer, fr: Node, env: &Env, groups: &[GroupXf]) {
    if inherit::is_hidden(fr) {
        return;
    }
    let Some(xf) = child(fr, "xfrm").and_then(|n| parse_xfrm(n)) else {
        return;
    };
    let xf = Xf::compose_nested(groups, &xf);
    let data = child(fr, "graphic").and_then(|g| child(g, "graphicData"));
    let uri = data.and_then(|d| d.attribute("uri")).unwrap_or("");

    let mut css = xf.css();
    if uri.contains("/table") {
        w.open("div", &attrs(&[&attr("class", "pp-sp"), &attr("style", &css)]));
        match data.and_then(|d| child(d, "tbl")) {
            Some(tbl) => emit_table(ctx, w, tbl, env),
            None => placeholder(w, "table unavailable"),
        }
        w.close();
        return;
    }

    // Charts, SmartArt diagrams and embedded objects all need a renderer this
    // preview does not have. The box is still drawn at the right geometry, so
    // the slide's composition survives. The two kinds a deck is actually built
    // around say so in the footer as well; the rest are self-explanatory.
    if uri.contains("/chart") {
        ctx.notes
            .add("Charts not drawn");
    } else if uri.contains("/diagram") {
        ctx.notes.add("SmartArt not drawn");
    }
    css.push_str("box-sizing:border-box;");
    w.open("div", &attrs(&[&attr("class", "pp-sp"), &attr("style", &css)]));
    placeholder(w, emit::graphic_label(uri));
    w.close();
}

fn emit_table(ctx: &mut Ctx, w: &mut Writer, tbl: Node, env: &Env) {
    if child(tbl, "tblPr")
        .and_then(|p| child(p, "tableStyleId"))
        .is_some()
    {
    }
    let cols: Vec<f64> = child(tbl, "tblGrid")
        .map(|g| {
            elems(g)
                .filter(|c| c.tag_name().name() == "gridCol")
                .take(MAX_TABLE_COLS)
                .map(|c| {
                    c.attribute("w")
                        .and_then(|v| v.parse::<i64>().ok())
                        .map(|v| emu_to_px(v) as f64)
                        .unwrap_or(0.0)
                })
                .collect()
        })
        .unwrap_or_default();

    w.open("table", &attr("class", "pp-tbl"));
    if !cols.is_empty() {
        w.open("colgroup", "");
        for width in &cols {
            let mut st = Style::new();
            st.push_opt("width", fmt_px(*width as f32));
            w.void("col", &st.to_attr());
        }
        w.close();
    }

    for row in elems(tbl)
        .filter(|c| c.tag_name().name() == "tr")
        .take(MAX_TABLE_ROWS)
    {
        if w.is_full() {
            break;
        }
        let mut st = Style::new();
        if let Some(h) = row.attribute("h").and_then(|v| v.parse::<i64>().ok()) {
            st.push_opt("height", fmt_px(emu_to_px(h)));
        }
        w.open("tr", &st.to_attr());
        for cell in elems(row)
            .filter(|c| c.tag_name().name() == "tc")
            .take(MAX_TABLE_COLS)
        {
            // A merged-away cell exists in the XML but is covered by the
            // spanning one; emitting it would push the row out by a column.
            if truthy_attr(cell, "hMerge") || truthy_attr(cell, "vMerge") {
                continue;
            }
            emit_cell(ctx, w, cell, env);
        }
        w.close();
    }
    w.close();
}

/// A `width style colour` border shorthand. The width goes through `fmt_px` so a
/// stroke width that came out of an EMU division does not land in the CSS as
/// seventeen decimal places.
fn rule(width_px: f64, dash: &str, color: &str) -> String {
    let w = fmt_px((width_px.max(0.75)) as f32).unwrap_or_else(|| "1px".to_string());
    format!("{w} {dash} {color}")
}

fn truthy_attr(n: Node, name: &str) -> bool {
    n.attribute(name)
        .map(|v| matches!(v, "1" | "true"))
        .unwrap_or(false)
}

fn emit_cell(ctx: &mut Ctx, w: &mut Writer, cell: Node, env: &Env) {
    let tc_pr = child(cell, "tcPr");
    let mut st = Style::new();
    if let Some(pr) = tc_pr {
        if let Some(f) = parse_fill_opt(pr, ctx.theme, None) {
            push_fill(&mut st, &f);
        }
        for (tag, side) in [
            ("lnL", "border-left"),
            ("lnR", "border-right"),
            ("lnT", "border-top"),
            ("lnB", "border-bottom"),
        ] {
            if let Some(l) = child(pr, tag).and_then(|n| parse_line(n, ctx.theme, None)) {
                if l.is_visible() {
                    st.push(side, &rule(l.width_px, "solid", &l.stroke_color()));
                }
            }
        }
        st.push(
            "vertical-align",
            match pr.attribute("anchor") {
                Some("ctr") => "middle",
                Some("b") => "bottom",
                _ => "top",
            },
        );
    }
    let mut extra = Vec::new();
    let span = cell
        .attribute("gridSpan")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 1);
    let rowspan = cell
        .attribute("rowSpan")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 1);
    if let Some(s) = span {
        extra.push(attr("colspan", &s.min(MAX_TABLE_COLS as u32).to_string()));
    }
    if let Some(s) = rowspan {
        extra.push(attr("rowspan", &s.min(MAX_TABLE_ROWS as u32).to_string()));
    }
    let style_attr = st.to_attr();
    let refs: Vec<&str> = extra
        .iter()
        .map(|s| s.as_str())
        .chain(std::iter::once(style_attr.as_str()))
        .collect();
    w.open("td", &attrs(&refs));
    if let Some(tb) = child(cell, "txBody") {
        let mut cas = Cascade::default();
        cas.push(inherit::tx_style(env.master_root, StyleKind::Other));
        cas.push(child(tb, "lstStyle"));
        let opts = text::parse_body_pr(&[child(tb, "bodyPr")]);
        let paras = text::parse_paras(tb, &cas, &opts, ctx.theme, &ctx.clr_map);
        text::emit_paras(ctx, w, &paras, &opts);
    }
    w.close();
}
