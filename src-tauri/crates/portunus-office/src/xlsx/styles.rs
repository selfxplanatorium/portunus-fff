//! `xl/styles.xml` → one resolved CSS class per cell format.
//!
//! Each `cellXfs` entry is resolved exactly once, into a declaration string that
//! the sheet emits as a class rule. Classes rather than inline styles is not a
//! tidiness preference: a 50k-cell sheet where every cell carries its own
//! `style="font-family:…;border:…"` is megabytes of duplicated bytes, while in
//! practice every cell of a column shares one `xf`.

use super::super::cellstyle::{align_css, border_css, Align, AlignSpec, Rotation};
use super::super::drawingml::color::Color;
use super::super::drawingml::fill::{self, GradKind, GradStop, Gradient};
use super::super::drawingml::theme::{SchemeSlot, Theme};
use super::super::fonts;
use super::super::html::{fmt_px, pt_to_px, Style};
use super::super::numfmt::Format;
use super::super::sheet::StyleTable;
use super::super::xml::{self, attr_bool, attr_f32, attr_u32, child, elems};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

// Every table in styles.xml is document-controlled and unbounded; each gets a cap
// so a hostile part cannot turn style resolution into the expensive part of a
// preview. Real workbooks are orders of magnitude below all of these.
const MAX_XFS: usize = 8192;
const MAX_FONTS: usize = 4096;
const MAX_FILLS: usize = 4096;
const MAX_BORDERS: usize = 4096;
const MAX_NUMFMTS: usize = 1024;
const MAX_GRAD_STOPS: usize = 32;

/// One indent level, in px. Excel defines it as three characters; at the default
/// font's 7px digit width that is 21px, which is visibly too deep next to real
/// spreadsheets, so this uses the ~0.25cm both LibreOffice and Excel's own
/// rendering land closer to.
const INDENT_PX: f32 = 9.0;

/// A resolved cell format: the number format to run values through, plus the CSS
/// declarations for its class.
pub struct CellStyle {
    /// Shared per `numFmtId` — many xfs use the same format code.
    pub fmt: Rc<Format>,
    /// Declarations without braces, e.g. `font-weight:700;text-align:center;`.
    /// Empty when the xf asks for nothing, in which case the cell gets no class.
    pub css: String,
    /// Declarations for an inner `<span>` rather than the cell itself. CSS
    /// transforms do not apply to `display:table-cell`, so rotated text needs one
    /// wrapper element — and only rotated cells pay for it.
    pub inner: String,
    /// The xf puts visible ink on an *empty* cell: a fill or a border. Fonts and
    /// alignment do not count, because they show nothing without text.
    ///
    /// This is what separates a sheet's real extent from the trailing block of
    /// styled-but-blank cells Excel writes out to the bottom of the used range.
    pub paints: bool,
}

impl CellStyle {
    fn general() -> CellStyle {
        CellStyle {
            fmt: Rc::new(Format::parse("General")),
            css: String::new(),
            inner: String::new(),
            paints: false,
        }
    }
}

pub struct Styles {
    xfs: Vec<CellStyle>,
    fallback: CellStyle,
}

impl Styles {
    /// A workbook with no (or an unreadable) styles part: every cell renders with
    /// the General format and no formatting.
    pub fn empty() -> Styles {
        Styles {
            xfs: Vec::new(),
            fallback: CellStyle::general(),
        }
    }

    pub fn parse(styles_xml: &str, theme: &Theme) -> Result<Styles, String> {
        let doc = xml::parse(styles_xml)?;
        let root = doc.root_element();

        let numfmts = parse_numfmts(root);
        let fonts: Vec<String> = collect(root, "fonts", "font", MAX_FONTS, |n| parse_font(n, theme));
        let fills: Vec<String> = collect(root, "fills", "fill", MAX_FILLS, |n| parse_fill(n, theme));
        let borders: Vec<String> =
            collect(root, "borders", "border", MAX_BORDERS, |n| parse_border(n, theme));

        // The named-style table the `xfId` of a cellXf points into. Consulted only
        // where a cellXf explicitly switches an aspect *off* (`applyFont="0"`),
        // which per the spec means "take the named style's value instead".
        let base: Vec<(XfRefs, Align)> = match child(root, "cellStyleXfs") {
            Some(n) => elems(n)
                .filter(|c| c.tag_name().name() == "xf")
                .take(MAX_XFS)
                .map(|c| {
                    let align = child(c, "alignment").map(parse_alignment).unwrap_or_default();
                    (xf_refs(c), align)
                })
                .collect(),
            None => Vec::new(),
        };

        let mut cache: HashMap<u16, Rc<Format>> = HashMap::new();
        let mut xfs = Vec::new();
        if let Some(cell_xfs) = child(root, "cellXfs") {
            for node in elems(cell_xfs)
                .filter(|c| c.tag_name().name() == "xf")
                .take(MAX_XFS)
            {
                let r = xf_refs(node);
                let inherit = r.xf_id.and_then(|i| base.get(i as usize)).map(|(b, _)| b);
                let inherit_align = r.xf_id.and_then(|i| base.get(i as usize)).map(|(_, a)| a);
                // `applyX` absent means "apply" — producers omit the flags far more
                // often than they set them, and a cellXf that names a fontId means
                // it.
                let font_id = pick(r.apply_font, r.font_id, inherit.and_then(|b| b.font_id));
                let fill_id = pick(r.apply_fill, r.fill_id, inherit.and_then(|b| b.fill_id));
                let border_id =
                    pick(r.apply_border, r.border_id, inherit.and_then(|b| b.border_id));
                let num_id = pick(r.apply_numfmt, r.num_id, inherit.and_then(|b| b.num_id));

                let mut css = String::new();
                let mut paints = false;
                if let Some(d) = font_id.and_then(|i| fonts.get(i as usize)) {
                    css.push_str(d);
                }
                if let Some(d) = fill_id.and_then(|i| fills.get(i as usize)) {
                    css.push_str(d);
                    paints |= !d.is_empty();
                }
                if let Some(d) = border_id.and_then(|i| borders.get(i as usize)) {
                    css.push_str(d);
                    paints |= !d.is_empty();
                }
                // Alignment inherits as a whole block, not per attribute: the
                // element is either present on the cellXf or taken from the named
                // style, which is how Excel's own UI edits it.
                let align = child(node, "alignment")
                    .filter(|_| r.apply_align != Some(false))
                    .map(parse_alignment);
                let inner = match align {
                    Some(a) => {
                        css.push_str(&a.cell);
                        a.inner
                    }
                    None => match inherit_align {
                        Some(a) => {
                            css.push_str(&a.cell);
                            a.inner.clone()
                        }
                        None => String::new(),
                    },
                };

                let id = num_id.unwrap_or(0).min(u16::MAX as u32) as u16;
                let fmt = cache
                    .entry(id)
                    .or_insert_with(|| Rc::new(format_for(id, &numfmts)))
                    .clone();
                xfs.push(CellStyle {
                    fmt,
                    css,
                    inner,
                    paints,
                });
            }
        }

        Ok(Styles {
            xfs,
            fallback: CellStyle::general(),
        })
    }

    /// The style for a cell's `s` attribute. An out-of-range index is ordinary in
    /// damaged files and degrades to the General format rather than failing.
    pub fn get(&self, id: u32) -> &CellStyle {
        self.xfs.get(id as usize).unwrap_or(&self.fallback)
    }

    /// True when `id` resolves to declarations worth a class attribute.
    pub fn has_css(&self, id: u32) -> bool {
        self.xfs
            .get(id as usize)
            .map(|x| !x.css.is_empty() || !x.inner.is_empty())
            .unwrap_or(false)
    }

    /// True when an *empty* cell of this style still shows something (see
    /// `CellStyle::paints`).
    pub fn paints(&self, id: u32) -> bool {
        self.xfs.get(id as usize).map(|x| x.paints).unwrap_or(false)
    }

    /// True when cells of this style need the inner `<span>` wrapper.
    pub fn has_inner(&self, id: u32) -> bool {
        self.xfs
            .get(id as usize)
            .map(|x| !x.inner.is_empty())
            .unwrap_or(false)
    }

}

impl StyleTable for Styles {
    /// `td.xfN{…}` rules for the styles the sheet actually used.
    ///
    /// The selector is `td.xfN` (one type + one class) so it ties with the base
    /// stylesheet's `.xl-sheet td` gridline and `td.xl-num` alignment rules and
    /// wins on source order — these rules are emitted last. A bare `.xfN` would
    /// lose to both, and a document border would never replace a gridline.
    fn css_block(&self, used: &BTreeSet<u32>) -> String {
        let mut out = String::new();
        for &id in used {
            let Some(x) = self.xfs.get(id as usize) else {
                continue;
            };
            if !x.css.is_empty() {
                out.push_str("td.xf");
                out.push_str(&id.to_string());
                out.push('{');
                out.push_str(&x.css);
                out.push_str("}\n");
            }
            if !x.inner.is_empty() {
                out.push_str("td.xf");
                out.push_str(&id.to_string());
                out.push_str(">span.xr{");
                out.push_str(&x.inner);
                out.push_str("}\n");
            }
        }
        out
    }
}

// ── xf references ────────────────────────────────────────────────────────────

#[derive(Default)]
struct XfRefs {
    num_id: Option<u32>,
    font_id: Option<u32>,
    fill_id: Option<u32>,
    border_id: Option<u32>,
    xf_id: Option<u32>,
    apply_numfmt: Option<bool>,
    apply_font: Option<bool>,
    apply_fill: Option<bool>,
    apply_border: Option<bool>,
    apply_align: Option<bool>,
}

fn xf_refs(node: roxmltree::Node<'_, '_>) -> XfRefs {
    XfRefs {
        num_id: attr_u32(node, "numFmtId"),
        font_id: attr_u32(node, "fontId"),
        fill_id: attr_u32(node, "fillId"),
        border_id: attr_u32(node, "borderId"),
        xf_id: attr_u32(node, "xfId"),
        apply_numfmt: attr_bool(node, "applyNumberFormat"),
        apply_font: attr_bool(node, "applyFont"),
        apply_fill: attr_bool(node, "applyFill"),
        apply_border: attr_bool(node, "applyBorder"),
        apply_align: attr_bool(node, "applyAlignment"),
    }
}

/// `apply` off → the named style's id; otherwise the xf's own.
fn pick(apply: Option<bool>, own: Option<u32>, inherited: Option<u32>) -> Option<u32> {
    if apply == Some(false) {
        return inherited.or(own);
    }
    own.or(inherited)
}

// ── number formats ───────────────────────────────────────────────────────────

fn parse_numfmts(root: roxmltree::Node<'_, '_>) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    let Some(list) = child(root, "numFmts") else {
        return map;
    };
    for n in elems(list)
        .filter(|c| c.tag_name().name() == "numFmt")
        .take(MAX_NUMFMTS)
    {
        let Some(id) = attr_u32(n, "numFmtId") else {
            continue;
        };
        let Some(code) = xml::attr_local(n, "formatCode") else {
            continue;
        };
        if id <= u16::MAX as u32 {
            map.insert(id as u16, code.to_string());
        }
    }
    map
}

/// A custom code (any id, though Excel only writes ids ≥ 164) wins over the
/// built-in table, because a workbook may legally redefine a built-in id.
fn format_for(id: u16, custom: &HashMap<u16, String>) -> Format {
    if let Some(code) = custom.get(&id) {
        return Format::parse(code);
    }
    Format::builtin(id).unwrap_or_else(|| Format::parse("General"))
}

// ── fonts ────────────────────────────────────────────────────────────────────

fn parse_font(node: roxmltree::Node<'_, '_>, theme: &Theme) -> String {
    let mut s = Style::new();
    // `name` in styles.xml, `rFont` in a rich-text run's rPr — same meaning.
    if let Some(name) = child(node, "name")
        .or_else(|| child(node, "rFont"))
        .and_then(|n| xml::attr_local(n, "val"))
    {
        s.push("font-family", &fonts::css_font_stack(name));
    }
    let sup_sub = child(node, "vertAlign").and_then(|n| xml::attr_local(n, "val"));
    if let Some(pt) = child(node, "sz").and_then(|n| attr_f32(n, "val")) {
        // Super/subscript text is drawn smaller; Excel does not store the reduced
        // size, it derives it.
        let pt = if sup_sub.is_some() { pt * 0.66 } else { pt };
        s.push_opt("font-size", fmt_px(pt_to_px(pt)));
    }
    match sup_sub {
        Some("superscript") => s.push("vertical-align", "super"),
        Some("subscript") => s.push("vertical-align", "sub"),
        _ => {}
    }
    if flag(node, "b") {
        s.push("font-weight", "700");
    }
    if flag(node, "i") {
        s.push("font-style", "italic");
    }
    // Underline and strike share one CSS property, so they are combined rather
    // than emitted twice (the second declaration would win and drop the first).
    let underline = child(node, "u")
        .map(|n| xml::attr_local(n, "val").unwrap_or("single") != "none")
        .unwrap_or(false);
    let mut deco = String::new();
    if underline {
        deco.push_str("underline");
    }
    if flag(node, "strike") {
        if !deco.is_empty() {
            deco.push(' ');
        }
        deco.push_str("line-through");
    }
    s.push("text-decoration", &deco);
    if let Some(c) = child(node, "color").and_then(|n| parse_color(n, theme)) {
        s.push("color", &c.css());
    }
    s.css().to_string()
}

// ── fills ────────────────────────────────────────────────────────────────────

fn parse_fill(node: roxmltree::Node<'_, '_>, theme: &Theme) -> String {
    let mut s = Style::new();
    if let Some(p) = child(node, "patternFill") {
        let pattern = xml::attr_local(p, "patternType").unwrap_or("none");
        // The Excel trap: for `solid`, the *foreground* colour is the fill and
        // bgColor is unused. Reading bgColor here paints most highlighted cells
        // white.
        let fg = child(p, "fgColor").and_then(|n| parse_color(n, theme));
        let bg = child(p, "bgColor").and_then(|n| parse_color(n, theme));
        match pattern {
            "none" => {}
            "solid" => {
                if let Some(c) = fg.or(bg) {
                    s.push("background-color", &c.css());
                }
            }
            other => {
                // The same flat-tint approximation a DrawingML `a:pattFill` gets,
                // except that SpreadsheetML names the ink density of its patterns
                // where DrawingML does not, so the coverage is passed in.
                let fg = fg.unwrap_or(Color::from_rgb(0x000000));
                let bg = bg.unwrap_or(Color::from_rgb(0xFFFFFF));
                s.push(
                    "background-color",
                    &fill::pattern_color(&fg, &bg, pattern_ink(other)).css(),
                );
            }
        }
        return s.css().to_string();
    }
    if let Some(g) = child(node, "gradientFill") {
        if let Some(grad) = parse_gradient_fill(g, theme) {
            if let Some(css) = fill::gradient_css(&grad) {
                s.push("background-image", &css);
            }
        }
    }
    s.css().to_string()
}

/// A SpreadsheetML `gradientFill` as a DrawingML [`Gradient`], so both formats
/// spell their gradients out through one CSS writer — including the +90° angle
/// rule, which the two had a copy of each.
///
/// The elements are not the same: stop positions are 0..1 fractions rather than
/// thousandths of a percent, the angle sits on `degree` in whole degrees rather
/// than on `a:lin` in 60000ths, and a `type="path"` gradient carries a
/// `left`/`right`/`top`/`bottom` focus rectangle rather than an `a:fillToRect`.
/// The focus rectangle is dropped either way — a CSS radial gradient has nowhere
/// to put it — so a path gradient becomes [`GradKind::Radial`] exactly as
/// `a:path` does.
fn parse_gradient_fill(g: roxmltree::Node<'_, '_>, theme: &Theme) -> Option<Gradient> {
    let stops: Vec<GradStop> = elems(g)
        .filter(|n| n.tag_name().name() == "stop")
        .take(MAX_GRAD_STOPS)
        .filter_map(|n| {
            let pos = attr_f32(n, "position").unwrap_or(0.0) as f64;
            let color = child(n, "color").and_then(|c| parse_color(c, theme))?;
            Some(GradStop {
                pos: pos.clamp(0.0, 1.0) * 100.0,
                color,
            })
        })
        .collect();
    if stops.is_empty() {
        return None;
    }
    // Document order is kept: Excel writes the stops in ascending position, and
    // reordering them would move a colour the producer put first.
    let kind = if xml::attr_local(g, "type") == Some("path") {
        GradKind::Radial
    } else {
        GradKind::Linear {
            css_deg: fill::css_gradient_angle(attr_f32(g, "degree").unwrap_or(0.0) as f64),
        }
    };
    Some(Gradient { kind, stops })
}

/// Ink coverage of a pattern type, 0 (all background) to 1 (all foreground).
fn pattern_ink(pattern: &str) -> f64 {
    match pattern {
        "darkGray" => 0.75,
        "mediumGray" => 0.5,
        "lightGray" => 0.25,
        "gray125" => 0.125,
        "gray0625" => 0.0625,
        // The line patterns (horizontal/vertical/diagonal grids and trellises)
        // all sit near half coverage; distinguishing them without a real pattern
        // brush buys nothing.
        "darkHorizontal" | "darkVertical" | "darkDown" | "darkUp" | "darkGrid"
        | "darkTrellis" => 0.5,
        "lightHorizontal" | "lightVertical" | "lightDown" | "lightUp" | "lightGrid"
        | "lightTrellis" => 0.25,
        _ => 0.5,
    }
}

// ── borders ──────────────────────────────────────────────────────────────────

fn parse_border(node: roxmltree::Node<'_, '_>, theme: &Theme) -> String {
    let mut s = Style::new();
    for (elem, prop) in [
        ("left", "border-left"),
        ("right", "border-right"),
        ("top", "border-top"),
        ("bottom", "border-bottom"),
    ] {
        let Some(e) = child(node, elem) else { continue };
        let Some(style) = xml::attr_local(e, "style") else {
            continue;
        };
        let Some((width, kind)) = border_css(style) else {
            continue;
        };
        // An edge with no colour is Excel's automatic black, not "no border".
        let color = child(e, "color")
            .and_then(|c| parse_color(c, theme))
            .unwrap_or(Color::from_rgb(0x000000));
        s.push(prop, &format!("{width} {kind} {}", color.css()));
    }
    // `diagonal` borders are deliberately dropped: CSS has no diagonal border, and
    // faking one with a gradient background would collide with the cell's fill.
    s.css().to_string()
}

// ── alignment ────────────────────────────────────────────────────────────────

fn parse_alignment(node: roxmltree::Node<'_, '_>) -> Align {
    let horiz = xml::attr_local(node, "horizontal").unwrap_or("general");
    let indent = attr_u32(node, "indent").unwrap_or(0).min(250);
    align_css(&AlignSpec {
        horizontal: horiz,
        vertical: xml::attr_local(node, "vertical"),
        wrap: attr_bool(node, "wrapText").unwrap_or(false),
        indent_px: INDENT_PX * indent as f32,
        rotation: attr_u32(node, "textRotation").and_then(rotation),
    })
}

/// `textRotation`: 0..90 is counter-clockwise, 91..180 encodes clockwise as
/// 90+angle, and 255 is Excel's "stacked" vertical text.
fn rotation(rot: u32) -> Option<Rotation> {
    Some(match rot {
        0 => return None,
        255 => Rotation::Stacked,
        1..=90 => Rotation::Ccw(rot as f32),
        91..=180 => Rotation::Ccw(-((rot - 90) as f32)),
        _ => return None,
    })
}

// ── colours ──────────────────────────────────────────────────────────────────

/// A SpreadsheetML `<color>`/`<fgColor>`/`<bgColor>`. This is *not* a DrawingML
/// colour element: the attribute set (`rgb`/`indexed`/`theme`/`tint`/`auto`) and
/// the tint algorithm are specific to SpreadsheetML, so it gets its own parser
/// and only the theme lookup is shared.
pub fn parse_color(node: roxmltree::Node<'_, '_>, theme: &Theme) -> Option<Color> {
    // `auto="1"` means "the system window text/background colour", which in a
    // preview is whatever the default already is.
    if attr_bool(node, "auto").unwrap_or(false) {
        return None;
    }
    let mut color = if let Some(hex) = xml::attr_local(node, "rgb") {
        parse_argb(hex)?
    } else if let Some(i) = attr_u32(node, "theme") {
        Color::from_rgb(theme.color(theme_slot(i)?))
    } else if let Some(i) = attr_u32(node, "indexed") {
        Color::from_rgb(indexed_color(i)?)
    } else {
        return None;
    };
    if let Some(t) = attr_f32(node, "tint").filter(|t| t.is_finite() && *t != 0.0) {
        color = apply_tint(color, t as f64);
    }
    Some(color)
}

/// `<color theme="N">` index → colour-scheme slot.
///
/// The first four are **swapped** relative to the order the slots appear in
/// `theme1.xml`: index 0 is "Background 1" (`lt1`) and index 1 is "Text 1"
/// (`dk1`). Reading them in file order inverts every themed colour in the
/// workbook — black text becomes white on white.
fn theme_slot(i: u32) -> Option<SchemeSlot> {
    Some(match i {
        0 => SchemeSlot::Lt1,
        1 => SchemeSlot::Dk1,
        2 => SchemeSlot::Lt2,
        3 => SchemeSlot::Dk2,
        4 => SchemeSlot::Accent1,
        5 => SchemeSlot::Accent2,
        6 => SchemeSlot::Accent3,
        7 => SchemeSlot::Accent4,
        8 => SchemeSlot::Accent5,
        9 => SchemeSlot::Accent6,
        10 => SchemeSlot::Hlink,
        11 => SchemeSlot::FolHlink,
        _ => return None,
    })
}

/// SpreadsheetML `tint`: -1 darkens to black, +1 lightens to white, defined by
/// the spec as an operation on HLS *luminance* (saturation is preserved), which
/// is why this is not `Color::mix` towards black/white — an RGB blend washes
/// saturated theme accents out.
fn apply_tint(c: Color, tint: f64) -> Color {
    let (h, l, s) = rgb_to_hls(c.r(), c.g(), c.b());
    let l = if tint < 0.0 {
        l * (1.0 + tint.clamp(-1.0, 0.0))
    } else {
        let t = tint.clamp(0.0, 1.0);
        l * (1.0 - t) + t
    };
    let (r, g, b) = hls_to_rgb(h, l.clamp(0.0, 1.0), s);
    Color {
        rgb: ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        alpha: c.alpha,
    }
}

fn rgb_to_hls(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, l, 0.0);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, l, s)
}

fn hls_to_rgb(h: f64, l: f64, s: f64) -> (u8, u8, u8) {
    if s <= 0.0 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let ch = |t: f64| -> u8 {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (ch(h + 1.0 / 3.0), ch(h), ch(h - 1.0 / 3.0))
}

/// `AARRGGBB` (or a bare `RRGGBB`). The alpha channel is honoured because
/// conditional-formatting fills use it.
fn parse_argb(hex: &str) -> Option<Color> {
    let h = hex.trim();
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => u32::from_str_radix(h, 16).ok().map(Color::from_rgb),
        8 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            Some(Color {
                rgb: v & 0x00FF_FFFF,
                alpha: ((v >> 24) & 0xFF) as f64 / 255.0,
            })
        }
        _ => None,
    }
}

/// The legacy 56-colour indexed palette. Kept as a table because `indexed` is
/// still what Excel writes for anything set from the pre-2007 colour picker, and
/// there is no formula behind the values.
pub fn indexed_color(i: u32) -> Option<u32> {
    const PALETTE: [u32; 56] = [
        0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF, 0x000000,
        0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF, 0x800000, 0x008000,
        0x000080, 0x808000, 0x800080, 0x008080, 0xC0C0C0, 0x808080, 0x9999FF, 0x993366, 0xFFFFCC,
        0xCCFFFF, 0x660066, 0xFF8080, 0x0066CC, 0xCCCCFF, 0x000080, 0xFF00FF, 0xFFFF00, 0x00FFFF,
        0x800080, 0x800000, 0x008080, 0x0000FF, 0x00CCFF, 0xCCFFFF, 0xCCFFCC, 0xFFFF99, 0x99CCFF,
        0xFF99CC, 0xCC99FF, 0xFFCC99, 0x3366FF, 0x33CCCC, 0x99CC00, 0xFFCC00, 0xFF9900, 0xFF6600,
        0x666699, 0x969696,
    ];
    // 64 and 65 are the system foreground/background, which have no fixed value.
    PALETTE.get(i as usize).copied()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Parses the `<parent><child/>…` tables, in document order (the index *is* the
/// id, so order is the whole contract).
fn collect<'a, T>(
    root: roxmltree::Node<'a, 'a>,
    parent: &str,
    item: &str,
    cap: usize,
    f: impl Fn(roxmltree::Node<'a, 'a>) -> T,
) -> Vec<T> {
    match child(root, parent) {
        Some(list) => elems(list)
            .filter(|n| n.tag_name().name() == item)
            .take(cap)
            .map(f)
            .collect(),
        None => Vec::new(),
    }
}

/// A boolean child element: present means true unless it carries `val="0"`.
fn flag(node: roxmltree::Node<'_, '_>, local: &str) -> bool {
    match child(node, local) {
        Some(n) => attr_bool(n, "val").unwrap_or(true),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(body: &str) -> Styles {
        let xml = format!(
            "<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{body}</styleSheet>"
        );
        Styles::parse(&xml, &Theme::default()).expect("styles parse")
    }

    #[test]
    fn custom_numfmt_beats_the_builtin_table() {
        let s = styles(
            r#"<numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy&quot;-&quot;mm"/></numFmts>
               <cellXfs count="2"><xf numFmtId="164" applyNumberFormat="1"/><xf numFmtId="14"/></cellXfs>"#,
        );
        assert_eq!(s.get(0).fmt.apply(45678.0), "2025-01");
        assert!(s.get(0).fmt.is_date());
        // Built-in 14 is m/d/yyyy.
        assert_eq!(s.get(1).fmt.apply(45678.0), "1/21/2025");
        // Out of range and unstyled cells both fall back to General.
        assert_eq!(s.get(99).fmt.apply(1.5), "1.5");
    }

    #[test]
    fn font_fill_border_and_alignment_become_one_declaration_string() {
        let s = styles(
            r#"<fonts count="1"><font><b/><i/><u/><strike/><sz val="12"/><name val="Calibri"/><color rgb="FFFF0000"/></font></fonts>
               <fills count="1"><fill><patternFill patternType="solid"><fgColor rgb="FF00FF00"/></patternFill></fill></fills>
               <borders count="1"><border><bottom style="medium"><color rgb="FF0000FF"/></bottom></border></borders>
               <cellXfs count="1"><xf fontId="0" fillId="0" borderId="0" applyFont="1" applyFill="1" applyBorder="1" applyAlignment="1"><alignment horizontal="center" vertical="top" wrapText="1" indent="2"/></xf></cellXfs>"#,
        );
        let css = &s.get(0).css;
        assert!(css.contains("font-weight:700;"), "{css}");
        assert!(css.contains("font-style:italic;"), "{css}");
        assert!(css.contains("text-decoration:underline line-through;"), "{css}");
        assert!(css.contains("font-size:16px;"), "{css}"); // 12pt
        assert!(css.contains("color:#ff0000;"), "{css}");
        assert!(css.contains("background-color:#00ff00;"), "{css}");
        assert!(css.contains("border-bottom:2px solid #0000ff;"), "{css}");
        assert!(css.contains("text-align:center;"), "{css}");
        assert!(css.contains("vertical-align:top;"), "{css}");
        assert!(css.contains("white-space:pre-wrap;"), "{css}");
        assert!(css.contains("padding-left:18px;"), "{css}");
        assert!(s.has_css(0));
        let block = s.css_block(&BTreeSet::from([0u32]));
        // The selector must carry a type so it ties with the base gridline rule
        // and wins on order.
        assert!(block.starts_with("td.xf0{"), "{block}");
        assert!(block.contains("border-bottom:2px solid #0000ff;"));
    }

    #[test]
    fn solid_fill_reads_the_foreground_colour() {
        // The classic inversion: for patternType="solid" Excel paints fgColor and
        // ignores bgColor.
        let s = styles(
            r#"<fills count="1"><fill><patternFill patternType="solid"><fgColor rgb="FFFFFF00"/><bgColor indexed="64"/></patternFill></fill></fills>
               <cellXfs count="1"><xf fillId="0"/></cellXfs>"#,
        );
        assert!(s.get(0).css.contains("background-color:#ffff00;"));
    }

    #[test]
    fn theme_colour_indices_swap_the_first_two_slots() {
        // theme="1" is Text 1 (dk1 = black), theme="0" is Background 1 (lt1 =
        // white). Reading them in theme1.xml order inverts the whole workbook.
        let s = styles(
            r#"<fonts count="2"><font><color theme="1"/></font><font><color theme="0"/></font></fonts>
               <cellXfs count="2"><xf fontId="0"/><xf fontId="1"/></cellXfs>"#,
        );
        assert!(s.get(0).css.contains("color:#000000;"), "{}", s.get(0).css);
        assert!(s.get(1).css.contains("color:#ffffff;"), "{}", s.get(1).css);
    }

    #[test]
    fn tint_moves_luminance_and_keeps_hue() {
        // Positive tint lightens, negative darkens, and a saturated accent stays
        // saturated (an RGB blend towards white would grey it out).
        let base = Color::from_rgb(0x4472C4);
        let light = apply_tint(base, 0.6);
        let dark = apply_tint(base, -0.5);
        let lum = |c: Color| 0.299 * c.r() as f64 + 0.587 * c.g() as f64 + 0.114 * c.b() as f64;
        assert!(lum(light) > lum(base), "{light:?}");
        assert!(lum(dark) < lum(base), "{dark:?}");
        assert!(light.b() > light.r(), "hue must stay blue: {light:?}");
        // The extremes are exactly white and black.
        assert_eq!(apply_tint(base, 1.0).rgb, 0xFFFFFF);
        assert_eq!(apply_tint(base, -1.0).rgb, 0x000000);
        // A grey has no hue to preserve and must not gain one.
        let g = apply_tint(Color::from_rgb(0x808080), 0.5);
        assert_eq!(g.r(), g.g());
        assert_eq!(g.g(), g.b());
    }

    #[test]
    fn colours_come_from_rgb_indexed_or_theme() {
        assert_eq!(parse_argb("FFFF0000").map(|c| c.rgb), Some(0xFF0000));
        assert_eq!(parse_argb("00FF00").map(|c| c.rgb), Some(0x00FF00));
        assert_eq!(parse_argb("80FF0000").map(|c| c.alpha), Some(128.0 / 255.0));
        assert_eq!(parse_argb("nothex"), None);
        assert_eq!(parse_argb("FFF"), None);
        assert_eq!(indexed_color(2), Some(0xFF0000));
        assert_eq!(indexed_color(64), None); // system foreground
        assert_eq!(indexed_color(9999), None);
        assert_eq!(theme_slot(12), None);
    }

    #[test]
    fn apply_flags_off_fall_back_to_the_named_style() {
        let s = styles(
            r#"<fonts count="2"><font><b/></font><font><i/></font></fonts>
               <cellStyleXfs count="1"><xf fontId="1"/></cellStyleXfs>
               <cellXfs count="2">
                 <xf fontId="0" xfId="0" applyFont="1"/>
                 <xf fontId="0" xfId="0" applyFont="0"/>
               </cellXfs>"#,
        );
        assert!(s.get(0).css.contains("font-weight:700;"));
        assert!(s.get(1).css.contains("font-style:italic;"));
        assert!(!s.get(1).css.contains("font-weight"));
    }

    #[test]
    fn missing_and_malformed_tables_degrade_instead_of_failing() {
        // No tables at all: every xf resolves, with no declarations.
        let s = styles(r#"<cellXfs count="1"><xf numFmtId="0" fontId="7" fillId="9"/></cellXfs>"#);
        assert_eq!(s.get(0).css, "");
        assert!(!s.has_css(0));
        assert_eq!(s.css_block(&BTreeSet::from([0u32])), "");
        // Garbage attribute values are ignored rather than propagated.
        let s = styles(
            r#"<fonts count="1"><font><sz val="not-a-number"/><color rgb="zz"/></font></fonts>
               <cellXfs count="1"><xf fontId="0"><alignment textRotation="9999" indent="x"/></xf></cellXfs>"#,
        );
        assert_eq!(s.get(0).css, "");
        // An empty styles part still answers every lookup.
        let e = Styles::empty();
        assert_eq!(e.get(3).fmt.apply(2.0), "2");
        assert!(Styles::parse("<not xml", &Theme::default()).is_err());
    }

    #[test]
    fn pattern_and_gradient_fills_are_approximated_not_dropped() {
        let s = styles(
            r#"<fills count="2">
                 <fill><patternFill patternType="lightGray"><fgColor rgb="FF000000"/><bgColor rgb="FFFFFFFF"/></patternFill></fill>
                 <fill><gradientFill degree="90"><stop position="0"><color rgb="FFFF0000"/></stop><stop position="1"><color rgb="FF0000FF"/></stop></gradientFill></fill>
               </fills>
               <cellXfs count="2"><xf fillId="0"/><xf fillId="1"/></cellXfs>"#,
        );
        // 25% ink over white is a light grey, not black and not white.
        assert!(s.get(0).css.starts_with("background-color:#bf"), "{}", s.get(0).css);
        // degree="90" is top→bottom, which CSS spells 180deg.
        let g = &s.get(1).css;
        assert!(g.contains("linear-gradient(180deg, #ff0000 0%, #0000ff 100%)"), "{g}");
    }

    #[test]
    fn path_gradients_become_radial_and_degenerate_ones_emit_nothing() {
        let s = styles(
            r#"<fills count="3">
                 <fill><gradientFill type="path" left="0.5" right="0.5" top="0.5" bottom="0.5">
                   <stop position="0"><color rgb="FFFFFFFF"/></stop>
                   <stop position="1"><color rgb="FF000000"/></stop></gradientFill></fill>
                 <fill><gradientFill><stop position="0"><color rgb="FFFF0000"/></stop></gradientFill></fill>
                 <fill><gradientFill degree="not-a-number"><stop position="café"><color rgb="FF00FF00"/></stop>
                   <stop position="1"><color rgb="FF0000FF"/></stop></gradientFill></fill>
               </fills>
               <cellXfs count="3"><xf fillId="0"/><xf fillId="1"/><xf fillId="2"/></cellXfs>"#,
        );
        assert!(
            s.get(0)
                .css
                .contains("radial-gradient(circle, #ffffff 0%, #000000 100%)"),
            "{}",
            s.get(0).css
        );
        // One stop is not a gradient CSS can express, so no declaration at all
        // rather than a function the browser drops.
        assert_eq!(s.get(1).css, "");
        // Garbage angle and position fall back rather than poisoning the value.
        let g = &s.get(2).css;
        assert!(g.contains("linear-gradient(90deg, #00ff00 0%, #0000ff 100%)"), "{g}");
    }

    #[test]
    fn text_rotation_decodes_excels_two_half_ranges() {
        assert_eq!(rotation(0), None);
        assert_eq!(rotation(45), Some(Rotation::Ccw(45.0)));
        assert_eq!(rotation(90), Some(Rotation::Ccw(90.0)));
        // 91..180 is 90+clockwise angle, so 135 is 45° the other way.
        assert_eq!(rotation(135), Some(Rotation::Ccw(-45.0)));
        assert_eq!(rotation(255), Some(Rotation::Stacked));
        assert_eq!(rotation(9999), None);
    }

    #[test]
    fn a_colourless_border_edge_is_automatic_black() {
        let s = styles(
            r#"<borders count="1"><border><left style="thin"/></border></borders>
               <cellXfs count="1"><xf borderId="0"/></cellXfs>"#,
        );
        assert!(s.get(0).css.contains("border-left:1px solid #000000;"));
    }
}
