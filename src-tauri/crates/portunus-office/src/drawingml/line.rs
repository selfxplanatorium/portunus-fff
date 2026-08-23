//! `a:ln` (outline properties) as a CSS border.
//!
//! CSS borders are a lossy target: no line caps, no join control, no compound
//! lines (`cmpd="dbl"`), and one dash pattern per style keyword. Cap and join are
//! parsed and exposed anyway, because a caller that draws the shape as SVG has
//! somewhere to put them.

use crate::drawingml::color::Color;
use crate::drawingml::fill::{parse_fill_opt, pattern_color, Fill, PATTERN_INK};
use crate::drawingml::theme::Theme;
use crate::html::{emu_to_px, fmt_px};
use crate::xml::{self, child, elems};

/// Width used when `a:ln` carries no `w`. Office's default outline is a hairline
/// (0.75pt ≈ 1px at the CSS reference resolution).
const DEFAULT_WIDTH_PX: f64 = 1.0;

/// Widths are clamped here rather than at emission: a document is free to declare
/// a 40cm outline, and a 1500px border repaints the whole preview.
const MAX_WIDTH_PX: f64 = 96.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dash {
    Solid,
    Dashed,
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Flat,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    Round,
    Bevel,
    Miter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub width_px: f64,
    /// The stroke paint. An `a:ln` with no fill child inherits from the theme's
    /// line style, which needs a style reference this module does not see; it is
    /// reported as [`Fill::None`] and a caller holding the `a:lnRef` should
    /// overwrite it.
    pub fill: Fill,
    pub dash: Dash,
    pub cap: Cap,
    pub join: Join,
}

/// Parse `node` as an `a:ln`, or find one among its direct children (`a:spPr`,
/// `a:tcPr`, `a:lnB`/`a:lnT` on table cells all wrap one).
pub fn parse_line(node: roxmltree::Node<'_, '_>, theme: &Theme, ph: Option<u32>) -> Option<Line> {
    let ln = if is_ln_tag(node.tag_name().name()) {
        node
    } else {
        elems(node).find(|n| n.tag_name().name() == "ln")?
    };

    let width_px = match xml::attr_local(ln, "w").and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(w) => (emu_to_px(w) as f64).clamp(0.0, MAX_WIDTH_PX),
        None => DEFAULT_WIDTH_PX,
    };

    Some(Line {
        width_px,
        fill: parse_fill_opt(ln, theme, ph).unwrap_or(Fill::None),
        dash: parse_dash(ln),
        cap: match xml::attr_local(ln, "cap") {
            Some("rnd") => Cap::Round,
            Some("sq") => Cap::Square,
            _ => Cap::Flat,
        },
        join: if child(ln, "round").is_some() {
            Join::Round
        } else if child(ln, "bevel").is_some() {
            Join::Bevel
        } else {
            // `a:miter` and an absent join both mean mitred.
            Join::Miter
        },
    })
}

/// The elements that *are* an outline. Listed rather than prefix-matched: `a:lnRef`
/// (a style reference) and `a:lnSpc` (paragraph line spacing) both start with
/// "ln" and hold entirely different content.
fn is_ln_tag(local: &str) -> bool {
    matches!(
        local,
        // `a:lnL`/`lnR`/`lnT`/`lnB` and the two diagonals are table-cell borders.
        "ln" | "lnL" | "lnR" | "lnT" | "lnB" | "lnTlToBr" | "lnBlToTr"
    )
}

/// `a:prstDash val` collapsed onto the three border styles CSS has. The dot/dash
/// families are distinguished by which one dominates the pattern; the `sys*`
/// presets are the same patterns at a tighter scale.
fn parse_dash(ln: roxmltree::Node<'_, '_>) -> Dash {
    if let Some(prst) = child(ln, "prstDash").and_then(|d| xml::attr_local(d, "val")) {
        return match prst {
            "solid" => Dash::Solid,
            "dot" | "sysDot" => Dash::Dotted,
            "dash" | "sysDash" | "lgDash" | "dashDot" | "lgDashDot" | "lgDashDotDot"
            | "sysDashDot" | "sysDashDotDot" => Dash::Dashed,
            // Unknown preset: a solid border is the safe read.
            _ => Dash::Solid,
        };
    }
    // `a:custDash` is an explicit stop list; CSS cannot express it, and every
    // custom dash is at least dashed-looking.
    if child(ln, "custDash").is_some() {
        return Dash::Dashed;
    }
    Dash::Solid
}

impl Line {
    /// The outline a theme style reference becomes. `a:lnRef` indexes the theme's
    /// line style list, which is not modelled here, so a referenced outline is
    /// drawn as a hairline in the referenced colour rather than not at all.
    pub fn hairline(color: Color) -> Line {
        Line {
            width_px: DEFAULT_WIDTH_PX,
            fill: Fill::Solid(color),
            dash: Dash::Solid,
            cap: Cap::Flat,
            join: Join::Miter,
        }
    }

    /// Whether this outline paints anything. A gradient or pattern stroke counts:
    /// [`line_css`] flattens it to a single colour.
    pub fn is_visible(&self) -> bool {
        self.width_px > 0.0 && self.flat_color().is_some()
    }

    /// The single colour CSS gets. Gradient strokes collapse to their first stop
    /// and pattern strokes to the fg/bg blend, because `border-color` takes one
    /// colour.
    fn flat_color(&self) -> Option<String> {
        match &self.fill {
            Fill::Solid(c) => Some(c.css()),
            Fill::Gradient(g) => g.stops.first().map(|s| s.color.css()),
            Fill::Pattern(fg, bg) => Some(pattern_color(fg, bg, PATTERN_INK).css()),
            Fill::None | Fill::Picture(_) => None,
        }
    }

    /// The stroke colour for a caller that paints one edge itself — a CSS rule on
    /// a single border, an SVG stroke — rather than going through [`line_css`].
    ///
    /// Deliberately not `flat_color`: only a solid stroke has a colour to give,
    /// and black keeps the edge visible where dropping it would lose the line
    /// altogether.
    pub fn stroke_color(&self) -> String {
        match &self.fill {
            Fill::Solid(c) => c.css(),
            _ => "#000".to_string(),
        }
    }
}

/// The `border` declaration for an outline, or `border:none;` when it paints
/// nothing (explicit, so it can override an inherited border).
pub fn line_css(l: &Line) -> String {
    let Some(color) = l.flat_color() else {
        return "border:none;".to_string();
    };
    // A sub-pixel border can round away to nothing in WebKit, which loses the
    // outline entirely; a declared outline is always worth at least one pixel.
    let Some(w) = fmt_px(l.width_px.max(1.0) as f32) else {
        return "border:none;".to_string();
    };
    let style = match l.dash {
        Dash::Solid => "solid",
        Dash::Dashed => "dashed",
        Dash::Dotted => "dotted",
    };
    format!("border:{} {} {};", w, style, color)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;

    fn theme() -> Theme {
        let mut t = Theme::default();
        t.colors.accent1 = 0x4472C4;
        t
    }

    fn line_of(body: &str) -> Option<Line> {
        let src = format!("<a:spPr {}>{}</a:spPr>", NS, body);
        let doc = xml::parse(&src).expect("fixture parses");
        parse_line(doc.root_element(), &theme(), None)
    }

    #[test]
    fn width_converts_from_emu_and_defaults_to_a_hairline() {
        // 12700 EMU = 1pt = 1.333px.
        let l = line_of(r#"<a:ln w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#)
            .unwrap();
        assert!((l.width_px - 4.0 / 3.0).abs() < 1e-4, "{}", l.width_px);
        assert_eq!(line_css(&l), "border:1.33px solid #000000;");
        // 28575 EMU = 2.25pt = 3px.
        let l = line_of(r#"<a:ln w="28575"><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:ln>"#)
            .unwrap();
        assert_eq!(line_css(&l), "border:3px solid #ff0000;");
        // No `w`: hairline.
        let l = line_of(r#"<a:ln><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#)
            .unwrap();
        assert_eq!(l.width_px, 1.0);
    }

    #[test]
    fn preset_dashes_map_onto_the_three_css_styles() {
        let styles = [
            ("solid", Dash::Solid),
            ("dot", Dash::Dotted),
            ("sysDot", Dash::Dotted),
            ("dash", Dash::Dashed),
            ("sysDash", Dash::Dashed),
            ("lgDash", Dash::Dashed),
            ("dashDot", Dash::Dashed),
            ("lgDashDotDot", Dash::Dashed),
            ("Widget", Dash::Solid),
        ];
        for (val, want) in styles {
            let l = line_of(&format!(
                r#"<a:ln w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:prstDash val="{}"/></a:ln>"#,
                val
            ))
            .unwrap();
            assert_eq!(l.dash, want, "prstDash {}", val);
        }
        let dotted = line_of(
            r#"<a:ln w="19050"><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:prstDash val="sysDot"/></a:ln>"#,
        )
        .unwrap();
        // 19050 EMU = 1.5pt = 2px.
        assert_eq!(line_css(&dotted), "border:2px dotted #000000;");
        // custDash has no CSS spelling but is never solid.
        let cust = line_of(
            r#"<a:ln w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:custDash><a:ds d="300000" sp="200000"/></a:custDash></a:ln>"#,
        )
        .unwrap();
        assert_eq!(cust.dash, Dash::Dashed);
    }

    #[test]
    fn cap_and_join_are_exposed() {
        let l = line_of(
            r#"<a:ln w="12700" cap="rnd"><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:bevel/></a:ln>"#,
        )
        .unwrap();
        assert_eq!(l.cap, Cap::Round);
        assert_eq!(l.join, Join::Bevel);
        let l = line_of(
            r#"<a:ln w="12700" cap="sq"><a:solidFill><a:srgbClr val="000000"/></a:solidFill><a:round/></a:ln>"#,
        )
        .unwrap();
        assert_eq!(l.cap, Cap::Square);
        assert_eq!(l.join, Join::Round);
        // Defaults.
        let l = line_of(r#"<a:ln w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#)
            .unwrap();
        assert_eq!(l.cap, Cap::Flat);
        assert_eq!(l.join, Join::Miter);
    }

    #[test]
    fn gradient_stroke_flattens_to_its_first_stop() {
        let l = line_of(
            r#"<a:ln w="12700"><a:gradFill><a:gsLst>
                 <a:gs pos="0"><a:schemeClr val="accent1"/></a:gs>
                 <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
               </a:gsLst><a:lin ang="0"/></a:gradFill></a:ln>"#,
        )
        .unwrap();
        assert_eq!(line_css(&l), "border:1.33px solid #4472c4;");
        assert!(l.is_visible());
    }

    #[test]
    fn no_fill_and_zero_width_paint_nothing() {
        let l = line_of(r#"<a:ln w="12700"><a:noFill/></a:ln>"#).unwrap();
        assert_eq!(l.fill, Fill::None);
        assert!(!l.is_visible());
        assert_eq!(line_css(&l), "border:none;");
        // An `a:ln` with no fill child inherits, which this module reads as None.
        let l = line_of(r#"<a:ln w="12700"/>"#).unwrap();
        assert_eq!(line_css(&l), "border:none;");
        // A zero-width but coloured outline still gets a visible 1px border,
        // matching how Office renders a hairline.
        let l = line_of(r#"<a:ln w="0"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#)
            .unwrap();
        assert_eq!(l.width_px, 0.0);
        assert_eq!(line_css(&l), "border:1px solid #000000;");
    }

    #[test]
    fn alpha_in_the_stroke_survives_into_rgba() {
        let l = line_of(
            r#"<a:ln w="12700"><a:solidFill><a:srgbClr val="FF0000"><a:alpha val="50000"/></a:srgbClr></a:solidFill></a:ln>"#,
        )
        .unwrap();
        assert_eq!(line_css(&l), "border:1.33px solid rgba(255, 0, 0, 0.5);");
    }

    #[test]
    fn absent_or_malformed_ln_never_panics() {
        assert!(line_of(r#"<a:prstGeom prst="rect"/>"#).is_none());
        for body in [
            r#"<a:ln w="café"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#,
            r#"<a:ln w="99999999999999999999"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#,
            r#"<a:ln w="-12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#,
            r#"<a:ln w="914400000"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#,
            "<a:ln/>",
        ] {
            let l = line_of(body).unwrap_or_else(|| panic!("{} must parse", body));
            let css = line_css(&l);
            assert!(!css.contains("NaN"), "{} → {}", body, css);
            assert!(l.width_px.is_finite() && l.width_px <= MAX_WIDTH_PX, "{}", body);
        }
        // An absurd declared width is clamped rather than repainting the world.
        let huge = line_of(
            r#"<a:ln w="914400000"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln>"#,
        )
        .unwrap();
        assert_eq!(huge.width_px, MAX_WIDTH_PX);
    }

    #[test]
    fn a_hairline_is_one_px_solid_in_the_given_colour() {
        let l = Line::hairline(Color::from_rgb(0x4472C4));
        assert_eq!(l.width_px, DEFAULT_WIDTH_PX);
        assert_eq!(line_css(&l), "border:1px solid #4472c4;");
        assert_eq!(l.stroke_color(), "#4472c4");
    }

    #[test]
    fn stroke_color_falls_back_to_black_for_a_non_solid_paint() {
        // A one-edge rule takes one colour and has no fallback of its own, so a
        // gradient or picture stroke becomes black rather than nothing.
        let l = line_of(
            r#"<a:ln w="12700"><a:gradFill><a:gsLst>
                 <a:gs pos="0"><a:schemeClr val="accent1"/></a:gs>
                 <a:gs pos="100000"><a:srgbClr val="FFFFFF"/></a:gs>
               </a:gsLst></a:gradFill></a:ln>"#,
        )
        .unwrap();
        assert_eq!(l.stroke_color(), "#000");
        // ...while the border shorthand still flattens to the first stop.
        assert_eq!(line_css(&l), "border:1.33px solid #4472c4;");
    }

    #[test]
    fn pattern_stroke_uses_the_blend() {
        let l = line_of(
            r#"<a:ln w="12700"><a:pattFill prst="pct50">
                 <a:fgClr><a:srgbClr val="000000"/></a:fgClr>
                 <a:bgClr><a:srgbClr val="FFFFFF"/></a:bgClr>
               </a:pattFill></a:ln>"#,
        )
        .unwrap();
        assert_eq!(
            l.fill,
            Fill::Pattern(Color::from_rgb(0x000000), Color::from_rgb(0xFFFFFF))
        );
        assert_eq!(line_css(&l), "border:1.33px solid #808080;");
    }
}
