//! DrawingML colour resolution: the `a:*Clr` element family, the child transform
//! list, and the CSS spelling of the result.
//!
//! The transform pipeline here is an *approximation* of Office's. ECMA-376 says
//! which transform applies but not in what precision, and Office rounds inside
//! its own colour model at every step. Doing tint/shade in linear RGB and
//! lum/sat/hue in HSL lands within a few percent per channel — invisible in a
//! preview, but the hex output is not bit-exact with PowerPoint and is not meant
//! to be.

use crate::drawingml::theme::{self, ClrMap, SchemeSlot, Theme};
use crate::xml::{self, child, elems};

/// Every percentage-typed `val` in this family is thousandths of a percent:
/// `val="60000"` is 60%.
const PCT_SCALE: f64 = 100_000.0;

/// Every angle-typed value (`a:hueOff@val`, `a:hslClr@hue`) is 60000ths of a
/// degree.
const ANG_PER_DEG: f64 = 60_000.0;

/// The *unresolved* base of a colour: what the document named, before the theme
/// or the placeholder argument is consulted. Kept separate from [`Color`] so a
/// caller that only needs to know "is this the placeholder colour?" (theme fill
/// styles, table styles) does not have to invent a placeholder value first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRef {
    Srgb(u32),
    Scheme(SchemeSlot),
    /// `a:schemeClr val="phClr"` — filled in by the instantiating context, never
    /// by the theme.
    Placeholder,
    None,
}

/// A fully resolved colour: `0xRRGGBB` plus the accumulated alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub rgb: u32,
    /// 0..=1.
    pub alpha: f64,
}

impl Color {
    pub fn from_rgb(rgb: u32) -> Color {
        Color {
            rgb: rgb & 0xFF_FFFF,
            alpha: 1.0,
        }
    }

    pub fn r(&self) -> u8 {
        (self.rgb >> 16) as u8
    }

    pub fn g(&self) -> u8 {
        (self.rgb >> 8) as u8
    }

    pub fn b(&self) -> u8 {
        self.rgb as u8
    }

    /// Opaque `#rrggbb`, or `rgba()` once alpha drops below full. The threshold
    /// is not exactly 1.0 because alpha accumulates through multiplications
    /// (`a:alphaMod`), and a 0.9999 should not cost an `rgba()` per colour.
    /// A non-finite alpha degrades to opaque rather than printing `NaN`, which
    /// would make CSS drop the whole declaration.
    pub fn css(&self) -> String {
        let a = clamp_unit(self.alpha);
        if a >= 0.999 {
            return self.hex();
        }
        format!(
            "rgba({}, {}, {}, {})",
            self.r(),
            self.g(),
            self.b(),
            fmt_alpha(a)
        )
    }

    /// Always `#rrggbb`, dropping alpha — for the properties whose CSS spelling
    /// has nowhere to put it.
    pub fn hex(&self) -> String {
        format!("#{:06x}", self.rgb & 0xFF_FFFF)
    }

    /// Blend towards `other` by `t`. Used by the pattern-fill approximation in
    /// `fill.rs`; deliberately not gamma-correct (see `fill::pattern_color`).
    pub fn mix(&self, other: &Color, t: f64) -> Color {
        let t = clamp_unit(t);
        let ch = |a: u8, b: u8| -> u32 {
            let v = a as f64 * (1.0 - t) + b as f64 * t;
            v.clamp(0.0, 255.0).round() as u32
        };
        Color {
            rgb: (ch(self.r(), other.r()) << 16)
                | (ch(self.g(), other.g()) << 8)
                | ch(self.b(), other.b()),
            alpha: clamp_unit(self.alpha * (1.0 - t) + other.alpha * t),
        }
    }
}

/// Resolve a colour under the default colour map.
///
/// `node` may be the `a:*Clr` element itself or a container that holds one
/// (`a:solidFill`, `a:fgClr`, `a:gs`, `a:highlight`, …): producers wrap colours
/// in a dozen different parents and each holds exactly one colour child.
///
/// `ph` supplies the value for `a:schemeClr val="phClr"`. `None` means the caller
/// has no placeholder in scope, and a `phClr` reference then fails to resolve
/// rather than silently picking a theme slot.
pub fn parse_color_elem(
    node: roxmltree::Node<'_, '_>,
    theme: &Theme,
    ph: Option<u32>,
) -> Option<Color> {
    parse_color_elem_map(node, theme, &ClrMap::default(), ph)
}

/// As [`parse_color_elem`], but honouring a slide master's `p:clrMap`, which may
/// swap `tx1`/`bg1` (a dark master inverts them).
pub fn parse_color_elem_map(
    node: roxmltree::Node<'_, '_>,
    theme: &Theme,
    map: &ClrMap,
    ph: Option<u32>,
) -> Option<Color> {
    let elem = color_elem(node)?;
    let rgb = match color_ref(elem, map) {
        ColorRef::Srgb(v) => v,
        ColorRef::Scheme(slot) => theme.color(slot),
        ColorRef::Placeholder => ph?,
        ColorRef::None => return None,
    };
    // Transforms run on floats and are packed back to 8-bit only at the end:
    // packing between a `lumMod`/`lumOff` pair quantises visibly.
    let mut c = unpack(rgb);
    let mut alpha = 1.0f64;
    apply_transforms(elem, &mut c, &mut alpha);
    Some(Color {
        rgb: pack(c),
        alpha: clamp_unit(alpha),
    })
}

/// The base colour reference of one `a:*Clr` element, before theme lookup. The
/// computed forms (`scrgbClr`, `hslClr`) collapse to `Srgb` here, which costs one
/// 8-bit round trip before the transforms run — acceptable, and it keeps every
/// downstream caller on a single representation.
pub fn color_ref(elem: roxmltree::Node<'_, '_>, map: &ClrMap) -> ColorRef {
    match elem.tag_name().name() {
        "srgbClr" => match xml::attr_local(elem, "val").and_then(theme::parse_hex_rgb) {
            Some(v) => ColorRef::Srgb(v),
            None => ColorRef::None,
        },
        "schemeClr" => {
            let Some(val) = xml::attr_local(elem, "val") else {
                return ColorRef::None;
            };
            if val == "phClr" {
                return ColorRef::Placeholder;
            }
            match SchemeSlot::resolve(val, map) {
                Some(slot) => ColorRef::Scheme(slot),
                None => ColorRef::None,
            }
        }
        // `lastClr` is what the producing application last saw for the system
        // colour and is the only portable answer; the name table is a fallback.
        "sysClr" => {
            if let Some(v) = xml::attr_local(elem, "lastClr").and_then(theme::parse_hex_rgb) {
                return ColorRef::Srgb(v);
            }
            match xml::attr_local(elem, "val").and_then(theme::sys_color) {
                Some(v) => ColorRef::Srgb(v),
                None => ColorRef::None,
            }
        }
        "prstClr" => match xml::attr_local(elem, "val").and_then(preset_color) {
            Some(v) => ColorRef::Srgb(v),
            None => ColorRef::None,
        },
        // scRGB channels are *linear* percentages, not sRGB ones.
        "scrgbClr" => {
            let ch = |name: &str| pct_attr(elem, name).unwrap_or(0.0);
            ColorRef::Srgb(pack([
                linear_to_srgb(ch("r")),
                linear_to_srgb(ch("g")),
                linear_to_srgb(ch("b")),
            ]))
        }
        "hslClr" => {
            let h = attr_f64(elem, "hue").unwrap_or(0.0) / ANG_PER_DEG;
            let s = pct_attr(elem, "sat").unwrap_or(0.0);
            let l = pct_attr(elem, "lum").unwrap_or(0.0);
            ColorRef::Srgb(pack(hsl_to_rgb([h, s, l])))
        }
        _ => ColorRef::None,
    }
}

fn is_color_tag(local: &str) -> bool {
    matches!(
        local,
        "srgbClr" | "schemeClr" | "sysClr" | "prstClr" | "scrgbClr" | "hslClr"
    )
}

fn color_elem<'a>(node: roxmltree::Node<'a, 'a>) -> Option<roxmltree::Node<'a, 'a>> {
    if is_color_tag(node.tag_name().name()) {
        return Some(node);
    }
    elems(node).find(|n| is_color_tag(n.tag_name().name()))
}

/// Convenience for the very common `<… ><a:solidFill><a:srgbClr/></a:solidFill>`
/// shape where the caller wants the colour, not a [`super::fill::Fill`].
pub fn solid_color(node: roxmltree::Node<'_, '_>, theme: &Theme, ph: Option<u32>) -> Option<Color> {
    parse_color_elem(child(node, "solidFill")?, theme, ph)
}

// ── transforms ───────────────────────────────────────────────────────────────

/// Applies the child transform elements **in document order**. The order is
/// load-bearing: `lumMod 50%` then `lumOff 50%` is a different colour from the
/// same pair reversed, and Office writes both orders.
fn apply_transforms(elem: roxmltree::Node<'_, '_>, c: &mut [f64; 3], alpha: &mut f64) {
    for t in elems(elem) {
        let name = t.tag_name().name();
        let v = pct_attr(t, "val");
        match name {
            // Tint/shade are defined on linear light. Blending gamma-encoded
            // sRGB instead makes a 50% tint noticeably too dark.
            "tint" => {
                let v = clamp_unit(v.unwrap_or(1.0));
                for x in c.iter_mut() {
                    *x = linear_to_srgb(srgb_to_linear(*x) * v + (1.0 - v));
                }
            }
            "shade" => {
                let v = clamp_unit(v.unwrap_or(1.0));
                for x in c.iter_mut() {
                    *x = linear_to_srgb(srgb_to_linear(*x) * v);
                }
            }
            "lumMod" | "lumOff" | "satMod" | "satOff" | "hueMod" | "hueOff" => {
                let mut hsl = rgb_to_hsl(*c);
                match name {
                    "lumMod" => hsl[2] *= v.unwrap_or(1.0),
                    "lumOff" => hsl[2] += v.unwrap_or(0.0),
                    "satMod" => hsl[1] *= v.unwrap_or(1.0),
                    "satOff" => hsl[1] += v.unwrap_or(0.0),
                    "hueMod" => hsl[0] *= v.unwrap_or(1.0),
                    // hueOff is an angle, not a percentage.
                    "hueOff" => hsl[0] += attr_f64(t, "val").unwrap_or(0.0) / ANG_PER_DEG,
                    _ => {}
                }
                // Clamping between steps (rather than only at the end) is what
                // Office does, and it is why lumOff-then-lumMod differs from the
                // reverse order.
                hsl[1] = clamp_unit(hsl[1]);
                hsl[2] = clamp_unit(hsl[2]);
                *c = hsl_to_rgb(hsl);
            }
            "alpha" => *alpha = clamp_unit(v.unwrap_or(1.0)),
            "alphaMod" => *alpha = clamp_unit(*alpha * v.unwrap_or(1.0)),
            "alphaOff" => *alpha = clamp_unit(*alpha + v.unwrap_or(0.0)),
            // NTSC luma weighting, matching Office's grayscale.
            "gray" => {
                let y = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
                *c = [y, y, y];
            }
            // Approximated as a 180° hue rotation at constant saturation and
            // luminance; Office defines `comp` inside its own colour model.
            "comp" => {
                let mut hsl = rgb_to_hsl(*c);
                hsl[0] += 180.0;
                *c = hsl_to_rgb(hsl);
            }
            "inv" => {
                for x in c.iter_mut() {
                    *x = 1.0 - *x;
                }
            }
            // `a:gamma` / `a:invGamma` are deliberately ignored: they re-encode
            // the channel, and applying one without the matching inverse — which
            // producers do not reliably pair — is worse than leaving it alone.
            _ => {}
        }
    }
}

// ── numeric helpers ──────────────────────────────────────────────────────────

fn attr_f64(node: roxmltree::Node<'_, '_>, name: &str) -> Option<f64> {
    let v: f64 = xml::attr_local(node, name)?.trim().parse().ok()?;
    v.is_finite().then_some(v)
}

/// A percentage-typed attribute as a 0..1 fraction. ECMA-376 2nd edition also
/// permits the literal `"50%"` spelling, which some 2010+ producers emit.
fn pct_attr(node: roxmltree::Node<'_, '_>, name: &str) -> Option<f64> {
    let raw = xml::attr_local(node, name)?.trim();
    let (text, scale) = match raw.strip_suffix('%') {
        Some(stripped) => (stripped.trim(), 100.0),
        None => (raw, PCT_SCALE),
    };
    let v: f64 = text.parse().ok()?;
    v.is_finite().then(|| v / scale)
}

fn clamp_unit(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        // Non-finite must never reach CSS; full opacity is the least surprising
        // substitute for a missing multiplier.
        1.0
    }
}

fn unpack(rgb: u32) -> [f64; 3] {
    [
        ((rgb >> 16) & 0xFF) as f64 / 255.0,
        ((rgb >> 8) & 0xFF) as f64 / 255.0,
        (rgb & 0xFF) as f64 / 255.0,
    ]
}

fn pack(c: [f64; 3]) -> u32 {
    let ch = |v: f64| -> u32 {
        if !v.is_finite() {
            return 0;
        }
        (v.clamp(0.0, 1.0) * 255.0).round() as u32
    };
    (ch(c[0]) << 16) | (ch(c[1]) << 8) | ch(c[2])
}

fn fmt_alpha(a: f64) -> String {
    let mut s = format!("{:.3}", a);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

pub fn srgb_to_linear(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// `[hue in degrees 0..360, saturation 0..1, luminance 0..1]`.
fn rgb_to_hsl(c: [f64; 3]) -> [f64; 3] {
    let (r, g, b) = (c[0], c[1], c[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d <= f64::EPSILON {
        // Achromatic: hue is undefined and the divisors below are zero.
        return [0.0, 0.0, l];
    }
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
    };
    [(h * 60.0).rem_euclid(360.0), s, l]
}

fn hsl_to_rgb(hsl: [f64; 3]) -> [f64; 3] {
    let h = hsl[0].rem_euclid(360.0) / 360.0;
    let s = clamp_unit(hsl[1]);
    let l = clamp_unit(hsl[2]);
    if s <= 0.0 {
        return [l, l, l];
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    [
        hue_channel(p, q, h + 1.0 / 3.0),
        hue_channel(p, q, h),
        hue_channel(p, q, h - 1.0 / 3.0),
    ]
}

fn hue_channel(p: f64, q: f64, t: f64) -> f64 {
    let t = t.rem_euclid(1.0);
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

// ── preset colours ───────────────────────────────────────────────────────────

/// `a:prstClr val` — the ST_PresetColorVal names, which are the SVG/X11 colour
/// names in camelCase. Matched case-insensitively so the CSS spelling
/// (`darkblue`) and the VML-era short aliases (`dkGray`) resolve too.
pub fn preset_color(name: &str) -> Option<u32> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "aliceblue" => 0xF0F8FF,
        "antiquewhite" => 0xFAEBD7,
        "aqua" => 0x00FFFF,
        "aquamarine" => 0x7FFFD4,
        "azure" => 0xF0FFFF,
        "beige" => 0xF5F5DC,
        "bisque" => 0xFFE4C4,
        "black" => 0x000000,
        "blanchedalmond" => 0xFFEBCD,
        "blue" => 0x0000FF,
        "blueviolet" => 0x8A2BE2,
        "brown" => 0xA52A2A,
        "burlywood" => 0xDEB887,
        "cadetblue" => 0x5F9EA0,
        "chartreuse" => 0x7FFF00,
        "chocolate" => 0xD2691E,
        "coral" => 0xFF7F50,
        "cornflowerblue" => 0x6495ED,
        "cornsilk" => 0xFFF8DC,
        "crimson" => 0xDC143C,
        "cyan" => 0x00FFFF,
        "darkblue" | "dkblue" => 0x00008B,
        "darkcyan" | "dkcyan" => 0x008B8B,
        "darkgoldenrod" => 0xB8860B,
        "darkgray" | "darkgrey" | "dkgray" | "dkgrey" => 0xA9A9A9,
        "darkgreen" | "dkgreen" => 0x006400,
        "darkkhaki" => 0xBDB76B,
        "darkmagenta" | "dkmagenta" => 0x8B008B,
        "darkolivegreen" => 0x556B2F,
        "darkorange" => 0xFF8C00,
        "darkorchid" => 0x9932CC,
        "darkred" | "dkred" => 0x8B0000,
        "darksalmon" => 0xE9967A,
        "darkseagreen" => 0x8FBC8F,
        "darkslateblue" => 0x483D8B,
        "darkslategray" | "darkslategrey" => 0x2F4F4F,
        "darkturquoise" => 0x00CED1,
        "darkviolet" => 0x9400D3,
        "deeppink" => 0xFF1493,
        "deepskyblue" => 0x00BFFF,
        "dimgray" | "dimgrey" => 0x696969,
        "dodgerblue" => 0x1E90FF,
        "firebrick" => 0xB22222,
        "floralwhite" => 0xFFFAF0,
        "forestgreen" => 0x228B22,
        "fuchsia" => 0xFF00FF,
        "gainsboro" => 0xDCDCDC,
        "ghostwhite" => 0xF8F8FF,
        "gold" => 0xFFD700,
        "goldenrod" => 0xDAA520,
        "gray" | "grey" => 0x808080,
        "green" => 0x008000,
        "greenyellow" => 0xADFF2F,
        "honeydew" => 0xF0FFF0,
        "hotpink" => 0xFF69B4,
        "indianred" => 0xCD5C5C,
        "indigo" => 0x4B0082,
        "ivory" => 0xFFFFF0,
        "khaki" => 0xF0E68C,
        "lavender" => 0xE6E6FA,
        "lavenderblush" => 0xFFF0F5,
        "lawngreen" => 0x7CFC00,
        "lemonchiffon" => 0xFFFACD,
        "lightblue" | "ltblue" => 0xADD8E6,
        "lightcoral" => 0xF08080,
        "lightcyan" | "ltcyan" => 0xE0FFFF,
        "lightgoldenrodyellow" => 0xFAFAD2,
        "lightgray" | "lightgrey" | "ltgray" | "ltgrey" => 0xD3D3D3,
        "lightgreen" | "ltgreen" => 0x90EE90,
        "lightpink" => 0xFFB6C1,
        "lightsalmon" => 0xFFA07A,
        "lightseagreen" => 0x20B2AA,
        "lightskyblue" => 0x87CEFA,
        "lightslategray" | "lightslategrey" => 0x778899,
        "lightsteelblue" => 0xB0C4DE,
        "lightyellow" | "ltyellow" => 0xFFFFE0,
        "lime" => 0x00FF00,
        "limegreen" => 0x32CD32,
        "linen" => 0xFAF0E6,
        "magenta" => 0xFF00FF,
        "maroon" => 0x800000,
        "mediumaquamarine" => 0x66CDAA,
        "mediumblue" => 0x0000CD,
        "mediumorchid" => 0xBA55D3,
        "mediumpurple" => 0x9370DB,
        "mediumseagreen" => 0x3CB371,
        "mediumslateblue" => 0x7B68EE,
        "mediumspringgreen" => 0x00FA9A,
        "mediumturquoise" => 0x48D1CC,
        "mediumvioletred" => 0xC71585,
        "midnightblue" => 0x191970,
        "mintcream" => 0xF5FFFA,
        "mistyrose" => 0xFFE4E1,
        "moccasin" => 0xFFE4B5,
        "navajowhite" => 0xFFDEAD,
        "navy" => 0x000080,
        "oldlace" => 0xFDF5E6,
        "olive" => 0x808000,
        "olivedrab" => 0x6B8E23,
        "orange" => 0xFFA500,
        "orangered" => 0xFF4500,
        "orchid" => 0xDA70D6,
        "palegoldenrod" => 0xEEE8AA,
        "palegreen" => 0x98FB98,
        "paleturquoise" => 0xAFEEEE,
        "palevioletred" => 0xDB7093,
        "papayawhip" => 0xFFEFD5,
        "peachpuff" => 0xFFDAB9,
        "peru" => 0xCD853F,
        "pink" => 0xFFC0CB,
        "plum" => 0xDDA0DD,
        "powderblue" => 0xB0E0E6,
        "purple" => 0x800080,
        "red" => 0xFF0000,
        "rosybrown" => 0xBC8F8F,
        "royalblue" => 0x4169E1,
        "saddlebrown" => 0x8B4513,
        "salmon" => 0xFA8072,
        "sandybrown" => 0xF4A460,
        "seagreen" => 0x2E8B57,
        "seashell" => 0xFFF5EE,
        "sienna" => 0xA0522D,
        "silver" => 0xC0C0C0,
        "skyblue" => 0x87CEEB,
        "slateblue" => 0x6A5ACD,
        "slategray" | "slategrey" => 0x708090,
        "snow" => 0xFFFAFA,
        "springgreen" => 0x00FF7F,
        "steelblue" => 0x4682B4,
        "tan" => 0xD2B48C,
        "teal" => 0x008080,
        "thistle" => 0xD8BFD8,
        "tomato" => 0xFF6347,
        "turquoise" => 0x40E0D0,
        "violet" => 0xEE82EE,
        "wheat" => 0xF5DEB3,
        "white" => 0xFFFFFF,
        "whitesmoke" => 0xF5F5F5,
        "yellow" => 0xFFFF00,
        "yellowgreen" => 0x9ACD32,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;

    /// accent1 is the slot the transform expectations below are computed from;
    /// everything else stays at the Office default.
    fn test_theme() -> Theme {
        let mut t = Theme::default();
        t.colors.accent1 = 0x4472C4;
        t.colors.dk1 = 0x000000;
        t.colors.lt1 = 0xFFFFFF;
        t
    }

    fn parse(xml_src: &str, ph: Option<u32>) -> Option<Color> {
        let doc = xml::parse(xml_src).expect("fixture parses");
        parse_color_elem(doc.root_element(), &test_theme(), ph)
    }

    fn clr(body: &str) -> String {
        format!(r#"<a:srgbClr {} val="808080">{}</a:srgbClr>"#, NS, body)
    }

    /// Channel-wise tolerance: the expectations are hand-computed to six decimal
    /// places, and the final 8-bit step can land either side of a .5 boundary.
    fn assert_near(got: u32, want: u32, tol: i32) {
        for shift in [16, 8, 0] {
            let a = ((got >> shift) & 0xFF) as i32;
            let b = ((want >> shift) & 0xFF) as i32;
            assert!(
                (a - b).abs() <= tol,
                "got #{:06x}, want #{:06x} (±{} per channel)",
                got,
                want,
                tol
            );
        }
    }

    #[test]
    fn parses_every_base_color_form() {
        assert_eq!(
            parse(&format!(r#"<a:srgbClr {} val="4472C4"/>"#, NS), None)
                .unwrap()
                .rgb,
            0x4472C4
        );
        assert_eq!(
            parse(&format!(r#"<a:schemeClr {} val="accent1"/>"#, NS), None)
                .unwrap()
                .rgb,
            0x4472C4
        );
        // tx1 aliases onto dk1 under the default colour map.
        assert_eq!(
            parse(&format!(r#"<a:schemeClr {} val="tx1"/>"#, NS), None)
                .unwrap()
                .rgb,
            0x000000
        );
        // lastClr wins over the system-colour name table.
        assert_eq!(
            parse(
                &format!(r#"<a:sysClr {} val="window" lastClr="EEECE1"/>"#, NS),
                None
            )
            .unwrap()
            .rgb,
            0xEEECE1
        );
        assert_eq!(
            parse(&format!(r#"<a:sysClr {} val="windowText"/>"#, NS), None)
                .unwrap()
                .rgb,
            0x000000
        );
        assert_eq!(
            parse(&format!(r#"<a:prstClr {} val="cornflowerBlue"/>"#, NS), None)
                .unwrap()
                .rgb,
            0x6495ED
        );
        // scRGB is linear: full-scale red is still #ff0000, but 50% linear green
        // encodes to ~#bc, not #80.
        assert_eq!(
            parse(
                &format!(r#"<a:scrgbClr {} r="100000" g="0" b="0"/>"#, NS),
                None
            )
            .unwrap()
            .rgb,
            0xFF0000
        );
        assert_near(
            parse(
                &format!(r#"<a:scrgbClr {} r="0" g="50000" b="0"/>"#, NS),
                None,
            )
            .unwrap()
            .rgb,
            0x00BC00,
            1,
        );
        // hue 0, sat 100%, lum 50% is pure red.
        assert_eq!(
            parse(
                &format!(r#"<a:hslClr {} hue="0" sat="100000" lum="50000"/>"#, NS),
                None
            )
            .unwrap()
            .rgb,
            0xFF0000
        );
    }

    #[test]
    fn unparseable_or_unknown_base_yields_none() {
        assert!(parse(&format!(r#"<a:srgbClr {} val="naïve"/>"#, NS), None).is_none());
        assert!(parse(&format!(r#"<a:srgbClr {}/>"#, NS), None).is_none());
        assert!(parse(&format!(r#"<a:schemeClr {} val="café"/>"#, NS), None).is_none());
        assert!(parse(&format!(r#"<a:prstClr {} val="Widget"/>"#, NS), None).is_none());
        assert!(parse(&format!(r#"<a:noFill {}/>"#, NS), None).is_none());
    }

    #[test]
    fn color_is_found_inside_a_container() {
        let src = format!(
            r#"<a:solidFill {}><a:srgbClr val="FF0000"/></a:solidFill>"#,
            NS
        );
        assert_eq!(parse(&src, None).unwrap().rgb, 0xFF0000);
        // …and through the `solid_color` shortcut one level further out.
        let sp = format!(
            r#"<a:spPr {}><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></a:spPr>"#,
            NS
        );
        let doc = xml::parse(&sp).unwrap();
        assert_eq!(
            solid_color(doc.root_element(), &test_theme(), None)
                .unwrap()
                .rgb,
            0x00FF00
        );
    }

    #[test]
    fn ph_clr_resolves_from_the_argument_not_the_theme() {
        let c = parse(
            &format!(r#"<a:schemeClr {} val="phClr"/>"#, NS),
            Some(0x123456),
        )
        .expect("phClr resolves from the argument");
        assert_eq!(c.rgb, 0x123456);
        assert_ne!(c.rgb, test_theme().colors.accent1);
        // Transforms still apply on top of the placeholder value.
        let faded = parse(
            &format!(
                r#"<a:schemeClr {} val="phClr"><a:alpha val="40000"/></a:schemeClr>"#,
                NS
            ),
            Some(0x123456),
        )
        .unwrap();
        assert_eq!(faded.rgb, 0x123456);
        assert!((faded.alpha - 0.4).abs() < 1e-9);
        // No placeholder in scope: unresolvable rather than a theme guess.
        assert!(parse(&format!(r#"<a:schemeClr {} val="phClr"/>"#, NS), None).is_none());
    }

    #[test]
    fn lum_mod_and_lum_off_match_hand_computed_hsl() {
        // accent1 #4472C4 → rgb(0.266667, 0.447059, 0.768627)
        //   max = b = 0.768627, min = r = 0.266667
        //   L = (max+min)/2              = 0.517647
        //   d = max-min                  = 0.501961
        //   S = d/(2-max-min)   (L>0.5)  = 0.501961/0.964706 = 0.520325
        //   H = 60*(4 + (r-g)/d)         = 60*(4 - 0.359375) = 218.4375°
        // lumMod 75%: L = 0.517647*0.75  = 0.388235
        // lumOff 25%: L = 0.388235+0.25  = 0.638235
        // back to RGB (H=218.4375, S=0.520325, L=0.638235):
        //   q = L + S - L*S              = 0.826471
        //   p = 2L - q                   = 0.450000
        //   r = p                        = 0.450000 → 114.75 → 0x73
        //   g = p + (q-p)*0.359375       = 0.585294 → 149.25 → 0x95
        //   b = q                        = 0.826471 → 210.75 → 0xD3
        let c = parse(
            &format!(
                r#"<a:schemeClr {} val="accent1"><a:lumMod val="75000"/><a:lumOff val="25000"/></a:schemeClr>"#,
                NS
            ),
            None,
        )
        .unwrap();
        assert_near(c.rgb, 0x7395D3, 1);
    }

    #[test]
    fn transform_order_is_document_order() {
        let a = parse(&clr(r#"<a:lumMod val="50000"/><a:lumOff val="50000"/>"#), None).unwrap();
        let b = parse(&clr(r#"<a:lumOff val="50000"/><a:lumMod val="50000"/>"#), None).unwrap();
        assert_ne!(a.rgb, b.rgb, "the pair must not commute");
        // #808080 is achromatic with L = 0.501961, so lum maps straight to a grey.
        // a: L = 0.501961*0.5 + 0.5 = 0.750980 → 191.5 → 0xC0
        assert_near(a.rgb, 0xC0C0C0, 1);
        // b: L = min(0.501961+0.5, 1.0) = 1.0, then *0.5 = 0.5 → 127.5 → 0x80
        assert_near(b.rgb, 0x808080, 1);
    }

    #[test]
    fn tint_and_shade_run_in_linear_space() {
        // #808080 → 0.501961 sRGB → linear ((0.501961+0.055)/1.055)^2.4 = 0.215861
        // tint 50%: 0.5*0.215861 + 0.5 = 0.607930
        //           → sRGB 1.055*0.607930^(1/2.4) - 0.055 = 0.802406 → 204.6 → 0xCD
        let t = parse(&clr(r#"<a:tint val="50000"/>"#), None).unwrap();
        assert_near(t.rgb, 0xCDCDCD, 1);
        // A gamma-space tint would give 0.5*0.501961 + 0.5 = 0.750980 → 0xC0,
        // visibly darker; guard against a regression to that.
        assert_ne!(t.rgb & 0xFF, 0xC0);

        // shade 50%: 0.215861*0.5 = 0.107930
        //           → sRGB 1.055*0.107930^(1/2.4) - 0.055 = 0.362263 → 92.4 → 0x5C
        let s = parse(&clr(r#"<a:shade val="50000"/>"#), None).unwrap();
        assert_near(s.rgb, 0x5C5C5C, 1);
        assert_ne!(s.rgb & 0xFF, 0x40); // the gamma-space answer
    }

    #[test]
    fn alpha_transforms_accumulate() {
        let c = parse(
            &clr(r#"<a:alpha val="80000"/><a:alphaMod val="50000"/>"#),
            None,
        )
        .unwrap();
        assert!((c.alpha - 0.4).abs() < 1e-9, "alpha was {}", c.alpha);
    }

    #[test]
    fn percent_literal_spelling_is_accepted() {
        let c = parse(&clr(r#"<a:alpha val="50%"/>"#), None).unwrap();
        assert!((c.alpha - 0.5).abs() < 1e-9);
    }

    #[test]
    fn hostile_transforms_do_not_panic_and_stay_in_gamut() {
        for t in [
            r#"<a:satMod val="400000"/>"#,
            r#"<a:satOff val="-90000"/>"#,
            r#"<a:hueMod val="200000"/>"#,
            r#"<a:hueOff val="10800000"/>"#,
            r#"<a:hueOff val="-99999999999"/>"#,
            "<a:gray/>",
            "<a:comp/>",
            "<a:inv/>",
            "<a:gamma/>",
            r#"<a:alphaOff val="-40000"/>"#,
            r#"<a:tint val="naïve"/>"#,
            r#"<a:shade val="1e400"/>"#,
            "<a:lumMod/>",
            r#"<a:lumMod val="99999999"/>"#,
        ] {
            let c = parse(
                &format!(r#"<a:srgbClr {} val="4472C4">{}</a:srgbClr>"#, NS, t),
                None,
            )
            .unwrap_or_else(|| panic!("{} must still resolve", t));
            assert!(c.rgb <= 0xFF_FFFF, "{} escaped the gamut", t);
            assert!((0.0..=1.0).contains(&c.alpha), "{} broke alpha", t);
            assert!(!c.css().contains("NaN"), "{} leaked NaN into CSS", t);
        }
        // inv of #4472C4 is #bb8d3b.
        let inv = parse(
            &format!(r#"<a:srgbClr {} val="4472C4"><a:inv/></a:srgbClr>"#, NS),
            None,
        )
        .unwrap();
        assert_eq!(inv.rgb, 0xBB8D3B);
    }

    #[test]
    fn css_emits_rgba_below_full_alpha() {
        assert_eq!(Color::from_rgb(0x4472C4).css(), "#4472c4");
        assert_eq!(
            Color {
                rgb: 0xFF0000,
                alpha: 0.5
            }
            .css(),
            "rgba(255, 0, 0, 0.5)"
        );
        assert_eq!(
            Color {
                rgb: 0x000000,
                alpha: 0.0
            }
            .css(),
            "rgba(0, 0, 0, 0)"
        );
        // 0.9995 rounds to opaque rather than an rgba() with alpha 1.
        assert_eq!(
            Color {
                rgb: 0x112233,
                alpha: 0.9995
            }
            .css(),
            "#112233"
        );
        // A poisoned alpha must never print NaN into the declaration.
        let nan = Color {
            rgb: 0x112233,
            alpha: f64::NAN,
        };
        assert_eq!(nan.css(), "#112233");
        assert_eq!(nan.hex(), "#112233");
    }

    #[test]
    fn color_map_can_swap_tx1_and_bg1() {
        let src = format!(r#"<a:schemeClr {} val="tx1"/>"#, NS);
        let doc = xml::parse(&src).unwrap();
        let map = ClrMap {
            tx1: SchemeSlot::Lt1,
            bg1: SchemeSlot::Dk1,
            tx2: SchemeSlot::Lt2,
            bg2: SchemeSlot::Dk2,
        };
        let c = parse_color_elem_map(doc.root_element(), &test_theme(), &map, None).unwrap();
        assert_eq!(c.rgb, 0xFFFFFF, "tx1 must follow the inverted map");
    }

    #[test]
    fn color_ref_reports_the_unresolved_base() {
        let map = ClrMap::default();
        let cases: [(&str, ColorRef); 4] = [
            (r#"<a:schemeClr val="phClr"/>"#, ColorRef::Placeholder),
            (
                r#"<a:schemeClr val="accent2"/>"#,
                ColorRef::Scheme(SchemeSlot::Accent2),
            ),
            (r#"<a:srgbClr val="010203"/>"#, ColorRef::Srgb(0x010203)),
            (r#"<a:srgbClr val="café"/>"#, ColorRef::None),
        ];
        for (body, want) in cases {
            let src = format!("<a:wrap {}>{}</a:wrap>", NS, body);
            let doc = xml::parse(&src).unwrap();
            let elem = elems(doc.root_element()).next().unwrap();
            assert_eq!(color_ref(elem, &map), want, "{}", body);
        }
    }

    #[test]
    fn mix_blends_towards_the_other_color() {
        let a = Color::from_rgb(0x000000);
        let b = Color::from_rgb(0xFFFFFF);
        assert_eq!(a.mix(&b, 0.0).rgb, 0x000000);
        assert_eq!(a.mix(&b, 1.0).rgb, 0xFFFFFF);
        assert_eq!(a.mix(&b, 0.5).rgb, 0x808080);
        // A non-finite factor must not produce a non-finite channel.
        assert!(a.mix(&b, f64::NAN).rgb <= 0xFF_FFFF);
    }
}
