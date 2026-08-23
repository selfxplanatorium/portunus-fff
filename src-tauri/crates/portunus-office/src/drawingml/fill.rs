//! DrawingML fill properties (`a:noFill`, `a:solidFill`, `a:gradFill`,
//! `a:blipFill`, `a:pattFill`, `a:grpFill`) and their CSS spelling.
//!
//! Media is deliberately *not* resolved here: a picture fill only carries the
//! relationship id out, and whoever owns the package decides whether and how to
//! inline the part. That keeps this module free of package/IO concerns and stops
//! a document from pulling bytes in during colour resolution.

use crate::drawingml::color::{parse_color_elem, Color};
use crate::drawingml::theme::Theme;
use crate::html::{fmt_deg, fmt_pct, Style};
use crate::xml::{self, child, elems};

/// `a:lin@ang`, `a:gs@pos` and friends: 60000ths of a degree.
const ANG_PER_DEG: f64 = 60_000.0;
/// Positions and crop insets are thousandths of a percent.
const PCT_SCALE: f64 = 100_000.0;

/// Ink coverage assumed for an `a:pattFill`: DrawingML names 28 preset hatches
/// and states no density for any of them, so they all read as half. Formats that
/// do name a density (SpreadsheetML's `patternFill`) pass their own.
pub const PATTERN_INK: f64 = 0.5;

/// A gradient stop. `pos` is a percentage (0..100) so it maps straight onto the
/// CSS colour-stop syntax.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradStop {
    pub pos: f64,
    pub color: Color,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradKind {
    /// Angle already converted to the CSS convention: degrees clockwise from
    /// "to top". See [`css_gradient_deg`].
    Linear { css_deg: f64 },
    /// `a:path` — shape/circle/rect path gradients all collapse to a radial
    /// gradient; `a:fillToRect` (the focus rectangle) is not reproduced.
    Radial,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    pub kind: GradKind,
    /// Sorted by `pos` ascending — CSS requires monotonic stops and producers do
    /// not guarantee document order matches.
    pub stops: Vec<GradStop>,
}

/// `a:srcRect` crop insets, as percentages of the *source* image edge to remove.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SrcRect {
    pub l: f64,
    pub t: f64,
    pub r: f64,
    pub b: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlipMode {
    Stretch,
    Tile,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PictureFill {
    /// The `r:embed` relationship id, to be resolved against the part's `.rels`
    /// by the caller. `r:link` (an external URL) is deliberately dropped: a
    /// preview must not fetch remote media.
    pub embed: String,
    pub crop: Option<SrcRect>,
    pub mode: BlipMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    None,
    Solid(Color),
    Gradient(Gradient),
    Picture(PictureFill),
    /// (foreground, background) of `a:pattFill`.
    Pattern(Color, Color),
}

/// Parse the fill of `node`, which may be the fill element itself or a container
/// that holds one (`a:spPr`, `a:tcPr`, `a:bgPr`, `a:rPr`, …). Only *direct*
/// children are considered, so the `a:solidFill` nested inside a sibling `a:ln`
/// cannot be mistaken for the shape's own fill.
///
/// Absent fill and `a:noFill` both come back as [`Fill::None`]; use
/// [`parse_fill_opt`] when the difference matters (absent means "inherit from the
/// style/placeholder", which only the caller can resolve).
pub fn parse_fill(node: roxmltree::Node<'_, '_>, theme: &Theme, ph: Option<u32>) -> Fill {
    parse_fill_opt(node, theme, ph).unwrap_or(Fill::None)
}

pub fn parse_fill_opt(
    node: roxmltree::Node<'_, '_>,
    theme: &Theme,
    ph: Option<u32>,
) -> Option<Fill> {
    let elem = fill_elem(node)?;
    Some(match elem.tag_name().name() {
        "noFill" => Fill::None,
        "solidFill" => match parse_color_elem(elem, theme, ph) {
            Some(c) => Fill::Solid(c),
            // A solidFill whose colour does not resolve is not "no fill": the
            // caller's inherited fill is the better answer, so report absence.
            None => return None,
        },
        "gradFill" => match parse_gradient(elem, theme, ph) {
            Some(g) => Fill::Gradient(g),
            None => return None,
        },
        "blipFill" => match parse_blip_fill(elem) {
            Some(p) => Fill::Picture(p),
            None => return None,
        },
        "pattFill" => {
            let fg = child(elem, "fgClr")
                .and_then(|n| parse_color_elem(n, theme, ph))
                .unwrap_or(Color::from_rgb(0x000000));
            let bg = child(elem, "bgClr")
                .and_then(|n| parse_color_elem(n, theme, ph))
                .unwrap_or(Color::from_rgb(0xFFFFFF));
            Fill::Pattern(fg, bg)
        }
        // `a:grpFill` inherits the enclosing group's fill, which is not tracked
        // here. Reported as absent so the caller's inheritance wins.
        "grpFill" => return None,
        _ => return None,
    })
}

fn is_fill_tag(local: &str) -> bool {
    matches!(
        local,
        "noFill" | "solidFill" | "gradFill" | "blipFill" | "pattFill" | "grpFill"
    )
}

fn fill_elem<'a>(node: roxmltree::Node<'a, 'a>) -> Option<roxmltree::Node<'a, 'a>> {
    if is_fill_tag(node.tag_name().name()) {
        return Some(node);
    }
    elems(node).find(|n| is_fill_tag(n.tag_name().name()))
}

// ── gradient ─────────────────────────────────────────────────────────────────

/// `a:lin@ang` (60000ths of a degree) → the CSS `linear-gradient()` angle.
pub fn css_gradient_deg(ang: i64) -> f64 {
    css_gradient_angle(ang as f64 / ANG_PER_DEG)
}

/// A gradient direction in whole degrees → the CSS `linear-gradient()` angle.
///
/// The Office formats measure the gradient *direction* clockwise from the
/// positive x-axis (0 = left→right; because y grows downwards, 90° is
/// top→bottom). CSS measures clockwise from "to top" (0deg = bottom→top,
/// 90deg = left→right). The two share a direction of rotation, so the conversion
/// is a +90° offset — not a negation, which is the usual mistake. SpreadsheetML's
/// `gradientFill@degree` states the same quantity in whole degrees, so it lands
/// here rather than growing a second copy of the rule.
pub fn css_gradient_angle(deg: f64) -> f64 {
    (deg + 90.0).rem_euclid(360.0)
}

fn parse_gradient(
    elem: roxmltree::Node<'_, '_>,
    theme: &Theme,
    ph: Option<u32>,
) -> Option<Gradient> {
    let mut stops: Vec<GradStop> = Vec::new();
    if let Some(lst) = child(elem, "gsLst") {
        for gs in elems(lst).filter(|n| n.tag_name().name() == "gs") {
            let Some(color) = parse_color_elem(gs, theme, ph) else {
                continue;
            };
            // A missing/garbage pos is clamped rather than dropped: losing the
            // colour is worse than misplacing the stop.
            let pos = pct_attr(gs, "pos").unwrap_or(0.0) * 100.0;
            stops.push(GradStop {
                pos: pos.clamp(0.0, 100.0),
                color,
            });
        }
    }
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.pos.partial_cmp(&b.pos).unwrap_or(std::cmp::Ordering::Equal));

    let kind = if child(elem, "path").is_some() {
        GradKind::Radial
    } else {
        let ang = child(elem, "lin")
            .and_then(|l| xml::attr_local(l, "ang"))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        GradKind::Linear {
            css_deg: css_gradient_deg(ang),
        }
    };
    Some(Gradient { kind, stops })
}

// ── picture ──────────────────────────────────────────────────────────────────

fn parse_blip_fill(elem: roxmltree::Node<'_, '_>) -> Option<PictureFill> {
    let blip = child(elem, "blip")?;
    let embed = xml::attr_local(blip, "embed")?.trim();
    if embed.is_empty() {
        return None;
    }
    let crop = child(elem, "srcRect").map(|r| SrcRect {
        l: inset(r, "l"),
        t: inset(r, "t"),
        r: inset(r, "r"),
        b: inset(r, "b"),
    });
    let mode = if child(elem, "tile").is_some() {
        BlipMode::Tile
    } else {
        // `a:stretch` is the default and by far the common case; an absent mode
        // element behaves like stretch.
        BlipMode::Stretch
    };
    Some(PictureFill {
        embed: embed.to_string(),
        crop,
        mode,
    })
}

fn inset(node: roxmltree::Node<'_, '_>, name: &str) -> f64 {
    (pct_attr(node, name).unwrap_or(0.0) * 100.0).clamp(-100.0, 100.0)
}

// ── CSS ──────────────────────────────────────────────────────────────────────

/// Appends a fill's declarations to `style`.
///
/// Structural on purpose: a caller that needs the pairs must not take
/// [`fill_css`]'s output apart again on `;` and `:`, because a value holding
/// either of those characters (a `url(data:…;base64,…)`, a `color-mix(…)`) does
/// not survive the round trip.
///
/// A picture fill contributes nothing: the caller resolves the relationship and
/// emits its own `background-image`/`<img>`.
pub fn push_fill(style: &mut Style, f: &Fill) {
    match f {
        // Explicit, so an element can override an inherited background.
        Fill::None => style.push("background-color", "transparent"),
        Fill::Solid(c) => style.push("background-color", &c.css()),
        Fill::Gradient(g) => {
            // The flat colour is both a fallback and what shows through if the
            // gradient function is dropped for an unformattable stop.
            if let Some(first) = g.stops.first() {
                style.push("background-color", &first.color.css());
            }
            if let Some(img) = gradient_css(g) {
                style.push("background-image", &img);
            }
        }
        Fill::Pattern(fg, bg) => {
            style.push("background-color", &pattern_color(fg, bg, PATTERN_INK).css())
        }
        Fill::Picture(_) => {}
    }
}

/// The CSS declarations for a fill, as a `prop:value;` run ready to append to a
/// `style` attribute. Use [`push_fill`] instead when the destination is a
/// [`Style`].
pub fn fill_css(f: &Fill) -> String {
    let mut s = Style::new();
    push_fill(&mut s, f);
    s.css().to_string()
}

/// The flat tone a pattern fill reads as: `fg` blended into `bg` at `ink`
/// coverage (0 = all background, 1 = all foreground).
///
/// The preset hatches (`pct5`, `ltUpDiag`, `openDmnd`, …) are not reproduced:
/// each would need its own SVG or repeating-gradient. At preview scale the tone
/// is what the fill contributes to the page, which is what this keeps. The blend
/// is deliberately not gamma-correct — matching Office's own naive blend matters
/// more here than being right about light.
pub fn pattern_color(fg: &Color, bg: &Color, ink: f64) -> Color {
    bg.mix(fg, ink)
}

/// Just the `linear-gradient(...)` / `radial-gradient(...)` function, or `None`
/// if any value would not format finitely — a single `NaN` in the function makes
/// CSS drop the whole declaration.
pub fn gradient_css(g: &Gradient) -> Option<String> {
    if g.stops.len() < 2 {
        return None;
    }
    let mut parts: Vec<String> = Vec::with_capacity(g.stops.len() + 1);
    match g.kind {
        GradKind::Linear { css_deg } => parts.push(fmt_deg(css_deg as f32)?),
        GradKind::Radial => parts.push("circle".to_string()),
    }
    for st in &g.stops {
        let pos = fmt_pct(st.pos as f32)?;
        parts.push(format!("{} {}", st.color.css(), pos));
    }
    let func = match g.kind {
        GradKind::Linear { .. } => "linear-gradient",
        GradKind::Radial => "radial-gradient",
    };
    Some(format!("{}({})", func, parts.join(", ")))
}

fn pct_attr(node: roxmltree::Node<'_, '_>, name: &str) -> Option<f64> {
    let raw = xml::attr_local(node, name)?.trim();
    let (text, scale) = match raw.strip_suffix('%') {
        Some(stripped) => (stripped.trim(), 100.0),
        None => (raw, PCT_SCALE),
    };
    let v: f64 = text.parse().ok()?;
    v.is_finite().then(|| v / scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = concat!(
        r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#
    );

    fn theme() -> Theme {
        let mut t = Theme::default();
        t.colors.accent1 = 0x4472C4;
        t
    }

    fn fill_of(body: &str, ph: Option<u32>) -> Fill {
        let src = format!("<a:spPr {}>{}</a:spPr>", NS, body);
        let doc = xml::parse(&src).expect("fixture parses");
        parse_fill(doc.root_element(), &theme(), ph)
    }

    #[test]
    fn solid_and_no_fill() {
        assert_eq!(
            fill_of(r#"<a:solidFill><a:srgbClr val="4472C4"/></a:solidFill>"#, None),
            Fill::Solid(Color::from_rgb(0x4472C4))
        );
        assert_eq!(fill_of("<a:noFill/>", None), Fill::None);
        // Absent fill collapses to None through `parse_fill`…
        assert_eq!(fill_of(r#"<a:prstGeom prst="rect"/>"#, None), Fill::None);
        // …but `parse_fill_opt` keeps "inherit" distinguishable.
        let src = format!(r#"<a:spPr {}><a:prstGeom prst="rect"/></a:spPr>"#, NS);
        let doc = xml::parse(&src).unwrap();
        assert_eq!(parse_fill_opt(doc.root_element(), &theme(), None), None);
        let src = format!("<a:spPr {}><a:noFill/></a:spPr>", NS);
        let doc = xml::parse(&src).unwrap();
        assert_eq!(
            parse_fill_opt(doc.root_element(), &theme(), None),
            Some(Fill::None)
        );
    }

    #[test]
    fn a_line_fill_is_not_mistaken_for_the_shape_fill() {
        // `a:ln` also contains a solidFill; only direct children may match.
        let f = fill_of(
            r#"<a:noFill/><a:ln><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:ln>"#,
            None,
        );
        assert_eq!(f, Fill::None);
    }

    #[test]
    fn solid_fill_honours_ph_clr() {
        assert_eq!(
            fill_of(
                r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#,
                Some(0x00FF00)
            ),
            Fill::Solid(Color::from_rgb(0x00FF00))
        );
    }

    #[test]
    fn lin_angle_maps_to_the_css_convention() {
        // ang=0 is left→right in DrawingML; CSS says that with 90deg.
        assert_eq!(css_gradient_deg(0), 90.0);
        // ang=5400000 (90°) is top→bottom; CSS "to bottom" is 180deg.
        assert_eq!(css_gradient_deg(5_400_000), 180.0);
        // And the two remaining cardinals, for the wrap-around.
        assert_eq!(css_gradient_deg(10_800_000), 270.0); // right→left
        assert_eq!(css_gradient_deg(16_200_000), 0.0); // bottom→top
        // Negative and out-of-range angles wrap instead of escaping the range.
        assert_eq!(css_gradient_deg(-5_400_000), 0.0);
        assert_eq!(css_gradient_deg(21_600_000 * 3), 90.0);
    }

    #[test]
    fn linear_gradient_stops_and_angle_reach_css() {
        let f = fill_of(
            r#"<a:gradFill>
                 <a:gsLst>
                   <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
                   <a:gs pos="0"><a:srgbClr val="000000"/></a:gs>
                 </a:gsLst>
                 <a:lin ang="5400000"/>
               </a:gradFill>"#,
            None,
        );
        let Fill::Gradient(g) = &f else {
            panic!("expected a gradient, got {:?}", f)
        };
        assert_eq!(g.kind, GradKind::Linear { css_deg: 180.0 });
        // Document order was reversed; stops must come out monotonic.
        assert_eq!(g.stops[0].pos, 0.0);
        assert_eq!(g.stops[0].color.rgb, 0x000000);
        assert_eq!(g.stops[1].pos, 100.0);
        let css = fill_css(&f);
        assert!(
            css.contains("background-image:linear-gradient(180deg, #000000 0%, #ffffff 100%);"),
            "{}",
            css
        );
        // The flat fallback keeps something visible if the function is unsupported.
        assert!(css.starts_with("background-color:#000000;"), "{}", css);
    }

    #[test]
    fn path_gradient_becomes_radial() {
        let f = fill_of(
            r#"<a:gradFill>
                 <a:gsLst>
                   <a:gs pos="0"><a:schemeClr val="accent1"/></a:gs>
                   <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
                 </a:gsLst>
                 <a:path path="circle"><a:fillToRect l="50000" t="50000"/></a:path>
               </a:gradFill>"#,
            None,
        );
        let Fill::Gradient(g) = &f else {
            panic!("expected a gradient")
        };
        assert_eq!(g.kind, GradKind::Radial);
        assert!(fill_css(&f).contains("radial-gradient(circle, #4472c4 0%, #ffffff 100%)"));
    }

    #[test]
    fn gradient_with_alpha_stops_uses_rgba() {
        let f = fill_of(
            r#"<a:gradFill>
                 <a:gsLst>
                   <a:gs pos="0"><a:srgbClr val="FF0000"><a:alpha val="0"/></a:srgbClr></a:gs>
                   <a:gs pos="100000"><a:srgbClr val="FF0000"/></a:gs>
                 </a:gsLst>
                 <a:lin ang="0"/>
               </a:gradFill>"#,
            None,
        );
        let css = fill_css(&f);
        assert!(css.contains("rgba(255, 0, 0, 0)"), "{}", css);
        assert!(css.contains("90deg"), "{}", css);
    }

    #[test]
    fn degenerate_gradients_do_not_emit_a_function() {
        // One stop: not a gradient CSS can express, but the colour still shows.
        let f = fill_of(
            r#"<a:gradFill><a:gsLst><a:gs pos="0"><a:srgbClr val="123456"/></a:gs></a:gsLst></a:gradFill>"#,
            None,
        );
        let css = fill_css(&f);
        assert_eq!(css, "background-color:#123456;");
        // No stops at all: nothing to draw, so the fill reads as inherit.
        assert_eq!(
            fill_of("<a:gradFill><a:gsLst/></a:gradFill>", None),
            Fill::None
        );
    }

    #[test]
    fn blip_fill_exposes_the_relationship_and_crop() {
        let f = fill_of(
            r#"<a:blipFill rotWithShape="1">
                 <a:blip r:embed="rId7"/>
                 <a:srcRect l="10000" t="0" r="25000" b="5000"/>
                 <a:stretch><a:fillRect/></a:stretch>
               </a:blipFill>"#,
            None,
        );
        let Fill::Picture(p) = &f else {
            panic!("expected a picture fill, got {:?}", f)
        };
        assert_eq!(p.embed, "rId7");
        assert_eq!(p.mode, BlipMode::Stretch);
        assert_eq!(
            p.crop,
            Some(SrcRect {
                l: 10.0,
                t: 0.0,
                r: 25.0,
                b: 5.0
            })
        );
        // Media is the caller's business, so no CSS comes out of here.
        assert_eq!(fill_css(&f), "");

        let tiled = fill_of(
            r#"<a:blipFill><a:blip r:embed="rId1"/><a:tile tx="0" ty="0"/></a:blipFill>"#,
            None,
        );
        let Fill::Picture(p) = &tiled else {
            panic!("expected a picture fill")
        };
        assert_eq!(p.mode, BlipMode::Tile);
        assert_eq!(p.crop, None);

        // An external (`r:link`) image is not fetched, so it is not a fill.
        assert_eq!(
            fill_of(
                r#"<a:blipFill><a:blip r:link="rId9"/><a:stretch/></a:blipFill>"#,
                None
            ),
            Fill::None
        );
        assert_eq!(fill_of("<a:blipFill/>", None), Fill::None);
    }

    #[test]
    fn pattern_fill_blends_foreground_over_background() {
        let f = fill_of(
            r#"<a:pattFill prst="ltUpDiag">
                 <a:fgClr><a:srgbClr val="000000"/></a:fgClr>
                 <a:bgClr><a:srgbClr val="FFFFFF"/></a:bgClr>
               </a:pattFill>"#,
            None,
        );
        assert_eq!(
            f,
            Fill::Pattern(Color::from_rgb(0x000000), Color::from_rgb(0xFFFFFF))
        );
        assert_eq!(fill_css(&f), "background-color:#808080;");
        // Missing fg/bg default to black on white rather than failing the fill.
        assert_eq!(
            fill_of(r#"<a:pattFill prst="pct50"/>"#, None),
            Fill::Pattern(Color::from_rgb(0x000000), Color::from_rgb(0xFFFFFF))
        );
    }

    #[test]
    fn no_fill_css_is_explicit_transparent() {
        assert_eq!(fill_css(&Fill::None), "background-color:transparent;");
    }

    #[test]
    fn push_fill_does_not_round_trip_through_a_css_string() {
        // What the callers of `fill_css` used to do to get pairs back: split the
        // declarations on `;`, then each one on `:`. A value carrying either
        // character loses everything past it, so the pairs have to be pushed
        // structurally instead.
        let hostile = "url(data:image/png;base64,AA)";
        let mut round_tripped = Style::new();
        let mut direct = Style::new();
        direct.push("background-image", hostile);
        for decl in direct.css().split(';').filter(|d| !d.is_empty()) {
            if let Some((k, v)) = decl.split_once(':') {
                round_tripped.push(k, v);
            }
        }
        assert_ne!(round_tripped.css(), direct.css());
        assert!(!round_tripped.css().contains("base64"), "{}", round_tripped.css());

        // `push_fill` emits the same declarations `fill_css` spells out, without
        // ever making a string a caller has to parse back.
        for f in [
            Fill::None,
            Fill::Solid(Color::from_rgb(0x4472C4)),
            Fill::Pattern(Color::from_rgb(0x000000), Color::from_rgb(0xFFFFFF)),
            Fill::Picture(PictureFill {
                embed: "rId1".to_string(),
                crop: None,
                mode: BlipMode::Stretch,
            }),
            Fill::Gradient(Gradient {
                kind: GradKind::Linear { css_deg: 90.0 },
                stops: vec![
                    GradStop {
                        pos: 0.0,
                        color: Color::from_rgb(0x000000),
                    },
                    GradStop {
                        pos: 100.0,
                        color: Color::from_rgb(0xFFFFFF),
                    },
                ],
            }),
        ] {
            let mut s = Style::new();
            push_fill(&mut s, &f);
            assert_eq!(s.css(), fill_css(&f), "{:?}", f);
        }
    }

    #[test]
    fn malformed_input_never_panics_or_leaks_nan() {
        for body in [
            "",
            "<a:solidFill/>",
            r#"<a:solidFill><a:srgbClr val="naïve"/></a:solidFill>"#,
            r#"<a:gradFill><a:gsLst><a:gs pos="café"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="1e400"><a:srgbClr val="00FF00"/></a:gs></a:gsLst><a:lin ang="not-a-number"/></a:gradFill>"#,
            r#"<a:gradFill><a:gsLst><a:gs pos="-9999999"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="9999999"><a:srgbClr val="00FF00"/></a:gs></a:gsLst><a:lin ang="99999999999999"/></a:gradFill>"#,
            r#"<a:blipFill><a:blip r:embed=""/></a:blipFill>"#,
            "<a:grpFill/>",
        ] {
            let css = fill_css(&fill_of(body, None));
            assert!(!css.contains("NaN"), "{} → {}", body, css);
            assert!(!css.contains("inf"), "{} → {}", body, css);
        }
        // The clamped-position gradient still produces monotonic 0%..100% stops.
        let f = fill_of(
            r#"<a:gradFill><a:gsLst><a:gs pos="-9999999"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="9999999"><a:srgbClr val="00FF00"/></a:gs></a:gsLst></a:gradFill>"#,
            None,
        );
        assert!(fill_css(&f).contains("#ff0000 0%, #00ff00 100%"));
    }
}
