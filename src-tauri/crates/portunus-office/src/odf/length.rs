//! ODF measurements and the small value vocabularies that travel with them: the
//! `fo:`/`svg:`/`style:` length strings, their percentage forms, the `fo:border`
//! shorthand, and colour values.
//!
//! ODF states a measurement as a CSS-ish string with a unit suffix rather than as
//! a scaled integer, so unlike OOXML there is one parser here instead of one
//! converter per scaling (`html::dxa_to_px` and friends). Everything lands in CSS
//! px at 96 dpi, which is the unit the renderers emit.
//!
//! Every ODF renderer measures through here.

use crate::drawingml::color::Color;
use crate::html::fmt_px;

/// CSS reference resolution: 96 px to the inch. Every other unit is defined
/// against the inch, so there is one constant rather than one per unit.
const PX_PER_IN: f32 = 96.0;

/// Widest length accepted, in px — about 10 000 in. Past this the value is not a
/// page, a column or a shape, and letting it through buys a canvas no reader can
/// use plus an f32 that overflows to infinity a multiplication or two later.
const MAX_ABS_PX: f32 = 1_000_000.0;

/// Percentage bound. Generous, because producers really do state 1000% glyph
/// scalings, but finite: a percentage feeds a multiplication against a parent box.
const MAX_ABS_PCT: f32 = 100_000.0;

/// Thinnest border drawn. ODF states hairlines as low as `0.06pt` (0.08 px),
/// which no display can render, and a border the document explicitly states must
/// be visible — the same reasoning as `cellstyle::border_css`'s `hair` arm.
const MIN_BORDER_PX: f32 = 1.0;

/// A measurement as ODF states it.
///
/// A percentage is *not* a length: `fo:font-size`, `style:column-width` and the
/// indent properties each accept either spelling, and the two resolve against
/// different things (a percentage needs a parent box, a length does not), so a
/// caller has to be able to tell them apart before it can use the number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Measure {
    /// CSS px at 96 dpi.
    Px(f32),
    /// The number in front of the `%`, not a 0..1 fraction: `45%` is `45.0`.
    Percent(f32),
}

/// Either spelling of a measurement.
pub fn parse_measure(s: &str) -> Option<Measure> {
    let (num, unit) = split_measure(s)?;
    if unit == "%" {
        return bounded(num, MAX_ABS_PCT).map(Measure::Percent);
    }
    let scale = match unit {
        "cm" => PX_PER_IN / 2.54,
        "mm" => PX_PER_IN / 25.4,
        "in" => PX_PER_IN,
        // Matches `html::pt_to_px`; a test pins the two together.
        "pt" => PX_PER_IN / 72.0,
        // 1pc = 12pt.
        "pc" => PX_PER_IN / 6.0,
        // A bare number is px. ODF requires a unit, but producers omit it on the
        // px-typed `svg:` attributes, and reading those as nothing collapses the
        // box they size.
        "px" | "" => 1.0,
        _ => return None,
    };
    bounded(num * scale, MAX_ABS_PX).map(Measure::Px)
}

/// A length in CSS px. A percentage is rejected rather than silently read as a
/// px count — the caller that can resolve one asks for it by name.
pub fn parse_len(s: &str) -> Option<f32> {
    match parse_measure(s)? {
        Measure::Px(v) => Some(v),
        Measure::Percent(_) => None,
    }
}

/// The number in front of a `%`, and only that: a length is rejected.
pub fn parse_percent(s: &str) -> Option<f32> {
    match parse_measure(s)? {
        Measure::Percent(v) => Some(v),
        Measure::Px(_) => None,
    }
}

/// `v` confined to `[min, max]`, for the geometry a renderer has to keep inside
/// sane bounds (page width, column width, a slide canvas).
///
/// Total on purpose: a non-finite `v` degrades to `min` rather than propagating,
/// because every caller is sizing a box and the smallest legal box is a better
/// answer than a dropped declaration. Infinity takes that exit too, rather than
/// clamping to `max`, so there is one rule to remember instead of two. An
/// inverted range likewise yields `min` instead of the panic `f32::clamp` would
/// raise.
pub fn clamp_px(v: f32, min: f32, max: f32) -> f32 {
    if !v.is_finite() || min > max {
        return min;
    }
    v.clamp(min, max)
}

/// Splits a measurement into its numeric part and its unit suffix, accepting
/// exactly `[+-]? digits [. digits]` followed by letters or `%`.
///
/// Scanned by hand rather than handed to `f32::from_str` on a guessed split,
/// because the float parser also accepts `inf`, `NaN` and `1e9` — none of which
/// ODF has, and all of which would reach CSS looking like a plausible number.
/// Locale-C decimal points only, so `1,5cm` is garbage rather than 1 cm.
fn split_measure(s: &str) -> Option<(f32, &str)> {
    let s = s.trim();
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let mut digits = 0usize;
    let mut dot = false;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' => digits += 1,
            b'.' if !dot => dot = true,
            _ => break,
        }
        i += 1;
    }
    if digits == 0 {
        return None;
    }
    // A long enough digit run parses to infinity, so this check is a guard in its
    // own right rather than a restatement of `bounded`.
    let num: f32 = s[..i].parse().ok()?;
    if !num.is_finite() {
        return None;
    }
    Some((num, s[i..].trim()))
}

fn bounded(v: f32, max: f32) -> Option<f32> {
    (v.is_finite() && v.abs() <= max).then_some(v)
}

// ── colour ───────────────────────────────────────────────────────────────────

/// `#rrggbb`, or the `transparent` keyword.
///
/// ODF's `fo:color` / `fo:background-color` type is XSL's, which is these two
/// spellings and nothing else — no `rgb()` functions, no colour names, and no
/// three-digit shorthand.
///
/// `transparent` comes back as an alpha-0 [`Color`] rather than as `None`,
/// because it is a *stated* value that has to override an inherited fill: a
/// caller must be able to tell it from "nothing stated", which is what `None`
/// means here.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("transparent") {
        return Some(Color { rgb: 0, alpha: 0.0 });
    }
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color::from_rgb(u32::from_str_radix(hex, 16).ok()?))
}

// ── border shorthand ─────────────────────────────────────────────────────────

/// A resolved `fo:border*` shorthand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    /// CSS px, never below [`MIN_BORDER_PX`].
    pub width_px: f32,
    /// A CSS `border-style` keyword, ready to emit.
    pub style: &'static str,
    /// `None` when the shorthand named no colour, which CSS spells
    /// `currentColor` and ODF means the same way.
    pub color: Option<Color>,
}

impl Border {
    /// The CSS `border` shorthand. `None` only when the width cannot be
    /// formatted, which `parse_border` already precludes.
    pub fn css(&self) -> Option<String> {
        let w = fmt_px(self.width_px)?;
        Some(match &self.color {
            Some(c) => format!("{w} {} {}", self.style, c.css()),
            None => format!("{w} {}", self.style),
        })
    }
}

/// The `fo:border` / `fo:border-top` … shorthand: `0.5pt solid #000000`.
///
/// `None` for `none`, `hidden`, and for anything with no usable piece at all. A
/// caller that has to tell "no border" from "nothing stated" has the attribute's
/// own presence for that.
///
/// Tokens are matched by what they are rather than by position: CSS lets the
/// three parts appear in any order, and while every producer in the corpus writes
/// width-style-colour, a positional parse would silently drop the border of the
/// one that does not.
pub fn parse_border(s: &str) -> Option<Border> {
    let mut width: Option<f32> = None;
    let mut style: Option<&'static str> = None;
    let mut color: Option<Color> = None;
    let mut usable = false;
    for tok in s.split_whitespace() {
        if let Some(k) = border_style(tok) {
            if k.is_empty() {
                // `none` / `hidden` win outright: there is nothing to draw, and a
                // width or colour beside them is leftover noise.
                return None;
            }
            style = Some(k);
            usable = true;
        } else if let Some(w) = parse_len(tok) {
            width = Some(w);
            usable = true;
        } else if let Some(c) = parse_color(tok) {
            color = Some(c);
            usable = true;
        }
    }
    if !usable {
        return None;
    }
    Some(Border {
        // A stated border is a visible one: a hairline, a negative width and an
        // explicit zero all floor to the thinnest line CSS can draw.
        width_px: width.unwrap_or(MIN_BORDER_PX).max(MIN_BORDER_PX),
        style: style.unwrap_or("solid"),
        color,
    })
}

/// The CSS `border-style` keywords, which is exactly ODF's set — `fo:border`
/// takes XSL's `border-style` vocabulary, not OOXML's `ST_BorderStyle`. That is
/// why `cellstyle::border_css` is deliberately *not* used here: it maps
/// `hair`/`thin`/`mediumDashed`, spellings ODF never emits, and it also picks a
/// width, which in ODF arrives inside the shorthand and must not be overridden.
///
/// The empty string stands for the two no-border keywords, so the caller has one
/// arm to check rather than two spellings to remember.
fn border_style(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "none" | "hidden" => "",
        "solid" => "solid",
        "double" => "double",
        "dotted" => "dotted",
        "dashed" => "dashed",
        "groove" => "groove",
        "ridge" => "ridge",
        "inset" => "inset",
        "outset" => "outset",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(s: &str) -> Option<f32> {
        parse_len(s)
    }

    fn close(a: Option<f32>, want: f32) -> bool {
        matches!(a, Some(v) if (v - want).abs() < 0.001)
    }

    #[test]
    fn every_unit_converts_to_px_at_96dpi() {
        assert!(close(px("1in"), 96.0));
        assert!(close(px("2.54cm"), 96.0));
        assert!(close(px("25.4mm"), 96.0));
        assert!(close(px("72pt"), 96.0));
        assert!(close(px("6pc"), 96.0));
        assert!(close(px("96px"), 96.0));
        // A bare number is px.
        assert!(close(px("96"), 96.0));
        // Corpus values, as a spot check on the two commonest units.
        assert!(close(px("0.5pt"), 0.6667));
        assert!(close(px("0.6cm"), 22.6772));
    }

    #[test]
    fn pt_conversion_matches_the_shared_converter() {
        // One definition of the point, not two.
        assert_eq!(px("12pt"), Some(crate::html::pt_to_px(12.0)));
        assert_eq!(px("0.74pt"), Some(crate::html::pt_to_px(0.74)));
    }

    #[test]
    fn signs_fractions_and_whitespace_are_accepted() {
        assert!(close(px("-0.25in"), -24.0));
        assert!(close(px("-0.1252in"), -12.0192));
        assert!(close(px("+1in"), 96.0));
        assert!(close(px("  1in  "), 96.0));
        assert!(close(px("1 in"), 96.0));
        assert!(close(px(".5in"), 48.0));
        assert!(close(px("-.5in"), -48.0));
        assert!(close(px("0in"), 0.0));
        assert!(close(px("-0.231cm"), -8.7307));
    }

    #[test]
    fn garbage_degrades_to_none() {
        for s in [
            "", "   ", "abc", "in", "%", "-", "+", ".", "-.", "cm12", "NaN", "nan", "inf",
            "infinity", "-inf", "12,5cm", "1e5cm", "1_000in", "#000000", "1in 1in", "1zz",
            "1 cm cm",
        ] {
            assert_eq!(parse_measure(s), None, "{s:?} must not parse");
        }
    }

    #[test]
    fn absurd_magnitudes_degrade_to_none() {
        // Finite but nonsensical: refused rather than clamped, because a value
        // this far out is a corrupt attribute, not a wide page.
        assert_eq!(px("100000in"), None);
        assert_eq!(px("-100000in"), None);
        // Overflows f32 inside the parse itself.
        let huge: String = "9".repeat(60);
        assert_eq!(px(&format!("{huge}in")), None);
        assert_eq!(px(&huge), None);
        // The largest accepted value is still finite px.
        assert!(px("10000in").is_some_and(|v| v.is_finite()));
    }

    #[test]
    fn percentages_are_distinguishable_from_lengths() {
        assert_eq!(parse_measure("45%"), Some(Measure::Percent(45.0)));
        assert_eq!(parse_percent("45%"), Some(45.0));
        assert_eq!(parse_percent("-10%"), Some(-10.0));
        assert_eq!(parse_percent("100%"), Some(100.0));
        // Each accessor rejects the other spelling.
        assert_eq!(parse_len("45%"), None);
        assert_eq!(parse_percent("12pt"), None);
        assert_eq!(parse_percent("12"), None);
        // Bounded the way a length is.
        assert_eq!(parse_percent("1000000%"), None);
        // `fo:font-size` takes both spellings through one call.
        assert!(matches!(parse_measure("20pt"), Some(Measure::Px(_))));
        assert!(matches!(parse_measure("75%"), Some(Measure::Percent(_))));
    }

    #[test]
    fn clamp_keeps_geometry_inside_its_bounds() {
        assert_eq!(clamp_px(500.0, 96.0, 4096.0), 500.0);
        assert_eq!(clamp_px(10.0, 96.0, 4096.0), 96.0);
        assert_eq!(clamp_px(99999.0, 96.0, 4096.0), 4096.0);
        // Every non-finite value takes the one documented exit, and an inverted
        // range does not panic.
        assert_eq!(clamp_px(f32::NAN, 96.0, 4096.0), 96.0);
        assert_eq!(clamp_px(f32::INFINITY, 96.0, 4096.0), 96.0);
        assert_eq!(clamp_px(f32::NEG_INFINITY, 96.0, 4096.0), 96.0);
        assert_eq!(clamp_px(500.0, 4096.0, 96.0), 4096.0);
    }

    #[test]
    fn colours_parse_hex_and_the_transparent_keyword() {
        assert_eq!(parse_color("#191b0e"), Some(Color::from_rgb(0x19_1b_0e)));
        assert_eq!(parse_color("#FFFFFF"), Some(Color::from_rgb(0xff_ff_ff)));
        assert_eq!(parse_color("  #2f5496 "), Some(Color::from_rgb(0x2f_5496)));

        // Transparent is a stated value, not an absent one, so it has to be
        // representable — which is why this returns `Color` and not a bare rgb
        // integer.
        let t = parse_color("transparent").expect("keyword parses");
        assert_eq!(t.alpha, 0.0);
        assert_eq!(t.css(), "rgba(0, 0, 0, 0)");
        assert_eq!(parse_color("TRANSPARENT").map(|c| c.alpha), Some(0.0));

        for s in ["", "#fff", "#12345g", "#1234567", "red", "rgb(0,0,0)", "000000", "#"] {
            assert_eq!(parse_color(s), None, "{s:?} must not parse");
        }
    }

    #[test]
    fn border_shorthand_yields_width_style_and_colour() {
        let b = parse_border("0.5pt solid #000000").expect("corpus border parses");
        assert_eq!(b.width_px, 1.0);
        assert_eq!(b.style, "solid");
        assert_eq!(b.color, Some(Color::from_rgb(0)));
        assert_eq!(b.css().as_deref(), Some("1px solid #000000"));

        let b = parse_border("1.76pt solid #000000").expect("thicker border parses");
        assert!((b.width_px - 2.3467).abs() < 0.001, "{b:?}");
        assert_eq!(b.css().as_deref(), Some("2.35px solid #000000"));

        // A hairline the display cannot draw still draws, as do an explicit zero
        // and a nonsensical negative.
        assert_eq!(parse_border("0.06pt solid #000000").map(|b| b.width_px), Some(1.0));
        assert_eq!(parse_border("0pt solid #000000").map(|b| b.width_px), Some(1.0));
        assert_eq!(parse_border("-2pt solid #000000").map(|b| b.width_px), Some(1.0));

        // Order-independent.
        let b = parse_border("#ff0000 dashed 2pt").expect("reordered border parses");
        assert_eq!(b.style, "dashed");
        assert_eq!(b.color, Some(Color::from_rgb(0xff_00_00)));
        assert!((b.width_px - 2.6667).abs() < 0.001, "{b:?}");

        // Every CSS style keyword ODF can name survives to the output.
        for k in ["double", "dotted", "dashed", "groove", "ridge", "inset", "outset"] {
            assert_eq!(
                parse_border(&format!("1pt {k} #000000")).map(|b| b.style),
                Some(k)
            );
        }

        // Missing pieces take CSS's own defaults, and a token that is none of the
        // three (`#000` is not an ODF colour) contributes nothing.
        let b = parse_border("solid").expect("style alone parses");
        assert_eq!((b.width_px, b.style, b.color), (1.0, "solid", None));
        assert_eq!(b.css().as_deref(), Some("1px solid"));
        assert_eq!(parse_border("2pt #000000").map(|b| b.style), Some("solid"));
        assert_eq!(parse_border("1pt solid #000").map(|b| b.color), Some(None));
    }

    #[test]
    fn border_none_and_garbage_yield_no_border() {
        assert_eq!(parse_border("none"), None);
        assert_eq!(parse_border("hidden"), None);
        // The keyword wins over the leftover width and colour beside it.
        assert_eq!(parse_border("0.5pt none #000000"), None);
        assert_eq!(parse_border(""), None);
        assert_eq!(parse_border("   "), None);
        assert_eq!(parse_border("café naïve"), None);
    }
}
