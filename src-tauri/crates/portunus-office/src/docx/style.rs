//! `word/styles.xml`: the paragraph and run property cascade.
//!
//! A docx paragraph's appearance is the sum of, weakest first:
//! `w:docDefaults` → the `w:basedOn` chain of its `w:pStyle` (root first) → the
//! numbering level's own `w:pPr` (see [`super::numbering`]) → the paragraph's
//! direct `w:pPr`; and for each run, that paragraph's accumulated `w:rPr` → the
//! run's `w:rStyle` chain → its direct `w:rPr`. Every link states only some
//! properties, which is why every field here is an `Option` and every merge
//! overwrites nothing it has no value for — "unstated" and "stated as the
//! default" are different, and only the former inherits.
//!
//! Word's real toggle rule is *not* implemented: for `w:b`/`w:i`/`w:caps` and
//! friends, direct formatting on top of a style that already sets the toggle
//! XORs (bolding an already-bold style un-bolds it). That is surprising to
//! readers, invisible in most documents, and needs the direct/style provenance
//! carried through every merge. Plain later-wins is used instead, so an explicit
//! `w:b w:val="0"` still turns bold off — only the double-toggle case differs.
//!
//! Units stay in the source's own scaling (dxa, half-points, eighths of a point)
//! until the conversion at the bottom, so that a merge never rounds twice.
//!
//! Table styles resolve the same way, over their own three property sets — see
//! [`Styles::resolve_table`]. They are here rather than with the table renderer
//! because a `w:tblPr` states borders and shading in the same attribute grammar
//! as a `w:pBdr` and a `w:shd`, and that grammar stays one copy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::super::cellstyle::{border_css, Rotation};
use super::super::drawingml::color::Color;
use super::super::drawingml::theme::{parse_hex_rgb, SchemeSlot, Theme};
use super::super::fonts;
use super::super::html::{dxa_to_px, eighth_pt_to_px, pct50_to_pct, pt_to_px};
use super::super::model::{self, Align, Border, Borders, Caps, LineHeight, Para, Script, TextRun};
use super::super::xml::{self, attr_local, child, elems, truthy};
use roxmltree::Node;

/// Font size used when no link in the cascade states one. Word's own built-in
/// `docDefaults` say 10pt, but every real document overrides it; this is only for
/// packages that ship no `styles.xml` at all.
pub const DEFAULT_SZ_PT: f32 = 11.0;

/// Styles a document may define. Word's own ceiling is 4079; past this the
/// remainder is dropped rather than letting a generated file allocate without
/// bound.
const MAX_STYLES: usize = 4096;

/// `w:basedOn` links followed before the chain is abandoned. Deep chains are
/// always machine-generated, and the cap is what makes a cycle terminate even if
/// the visited set below were wrong.
const MAX_BASED_ON: usize = 16;

/// Twentieths of a point a measurement may state, i.e. ±22 inches — Word's own
/// limit for an indent. Everything larger is a corrupt or hostile value that
/// would push the text column off the page.
const MAX_DXA: i64 = 31_680;

/// Eighths of a point for a border width: Word's UI caps at 6pt, the file format
/// at 12pt.
const MAX_BORDER_EIGHTHS: i64 = 96;

/// `w:pBdr/*@w:space` is in whole points and Word caps it at 31.
const MAX_BORDER_SPACE_PT: f32 = 31.0;

/// Columns a table grid may have. Word's own ceiling is 63; the grid, a
/// `w:gridSpan` and the emitted `<colgroup>` are all bounded by this one value so
/// that the three cannot disagree about how wide the table is.
pub const MAX_GRID_COLS: i64 = 64;

/// Twentieths of a point a cell margin may state, i.e. one inch. A margin is
/// padding inside a cell that is itself only a few inches wide, so a larger value
/// is corrupt rather than merely generous.
const MAX_CELL_MAR_DXA: i64 = 1440;

// ── properties ───────────────────────────────────────────────────────────────

/// A colour-valued property that a document can explicitly set to *nothing*.
///
/// [`ColorVal::Auto`] is a statement, not the absence of one: `w:color
/// w:val="auto"` overrides an inherited red back to the reader's default, and
/// `w:highlight w:val="none"` removes an inherited marker. Both spell out as the
/// model's `None`, but only after they have won the merge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorVal {
    Auto,
    Set(Color),
}

impl ColorVal {
    /// The colour this states, or `None` for [`ColorVal::Auto`] — which is a
    /// statement that the reader's own default applies, not an absence.
    pub fn color(self) -> Option<Color> {
        match self {
            ColorVal::Auto => None,
            ColorVal::Set(c) => Some(c),
        }
    }
}

/// How `w:spacing@w:line` is to be read. Word's default when the attribute is
/// present without a rule is `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRule {
    Auto,
    Exact,
    AtLeast,
}

/// `w:vertAlign`. `Baseline` is kept rather than folded into `None` so that it
/// can cancel an inherited superscript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertAlign {
    Baseline,
    Super,
    Sub,
}

/// One side of a `w:pBdr`, in the format's own units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderSpec {
    /// `w:val` — an `ST_Border` name, translated by
    /// [`super::super::cellstyle::border_css`].
    pub val: &'static str,
    /// `w:sz`, eighths of a point. `None` takes the width the style name implies.
    pub sz: Option<i64>,
    /// `w:space`, whole points.
    pub space_pt: Option<f32>,
    pub color: Option<ColorVal>,
}

/// The four `w:pBdr` edges. Each side is an `Option` of its own because Word
/// states them independently and a stronger link that mentions only `w:top` must
/// not erase an inherited `w:bottom`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BorderSides {
    pub top: Option<BorderSpec>,
    pub right: Option<BorderSpec>,
    pub bottom: Option<BorderSpec>,
    pub left: Option<BorderSpec>,
}

impl BorderSides {
    pub fn merge(&mut self, src: &BorderSides) {
        for (dst, s) in [
            (&mut self.top, src.top),
            (&mut self.right, src.right),
            (&mut self.bottom, src.bottom),
            (&mut self.left, src.left),
        ] {
            if s.is_some() {
                *dst = s;
            }
        }
    }

    fn is_none(&self) -> bool {
        *self == BorderSides::default()
    }
}

/// Run properties as one `w:rPr` states them. `None` is "not stated here".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunProps {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    /// `w:strike` or `w:dstrike` — CSS has one line-through, so both land here.
    pub strike: Option<bool>,
    /// `w:caps`. Wins over `small_caps` when both are on, as in Word.
    pub caps: Option<bool>,
    pub small_caps: Option<bool>,
    /// `w:sz`, half-points.
    pub sz_half: Option<i64>,
    pub color: Option<ColorVal>,
    /// Already through [`fonts::css_font_stack`], i.e. safe inside a `style`
    /// attribute.
    pub font: Option<String>,
    /// The raw typeface name, kept for the symbol-font remap.
    pub font_raw: Option<String>,
    /// `w:highlight` — a named marker colour, not an RGB one.
    pub highlight: Option<ColorVal>,
    /// Run-level `w:shd@w:fill`. Painted like a highlight; `w:highlight` wins
    /// when a run carries both.
    pub shade: Option<ColorVal>,
    pub vert_align: Option<VertAlign>,
    /// `w:spacing`, twentieths of a point of letter spacing (may be negative).
    pub spacing_dxa: Option<i64>,
    /// `w:vanish`: hidden text. Not applied here — the body walk drops the run,
    /// because a hidden run's text must not reach the search-term highlighter
    /// either.
    pub vanish: Option<bool>,
    pub r_style: Option<String>,
}

/// Paragraph properties as one `w:pPr` states them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParaProps {
    pub align: Option<Align>,
    /// `w:ind@w:left` / `@w:start`, twentieths of a point.
    pub ind_start_dxa: Option<i64>,
    /// `w:ind@w:right` / `@w:end`.
    pub ind_end_dxa: Option<i64>,
    /// `w:ind@w:firstLine` and `@w:hanging` folded into one signed value:
    /// positive indents the first line, negative hangs it left of the rest. See
    /// [`model::Para::first_line_px`] for why the model has only one field.
    pub first_line_dxa: Option<i64>,
    pub before_dxa: Option<i64>,
    pub after_dxa: Option<i64>,
    /// `w:spacing@w:line`, twentieths of a point — or 240ths of a line under
    /// [`LineRule::Auto`].
    pub line: Option<i64>,
    pub line_rule: Option<LineRule>,
    /// `w:contextualSpacing`: drop the before/after space between paragraphs of
    /// the same style. Whether a *neighbour* qualifies is the body walk's
    /// business, so it is only carried here.
    pub contextual_spacing: Option<bool>,
    pub borders: BorderSides,
    /// `w:shd` resolved to the fill it paints, pattern approximated.
    pub shade: Option<ColorVal>,
    pub num_id: Option<i64>,
    pub ilvl: Option<i64>,
    /// `w:outlineLvl`, 0-8, or 9 for "body text".
    pub outline_lvl: Option<i64>,
    pub bidi: Option<bool>,
    pub p_style: Option<String>,
    /// `w:pPr/w:rPr` — the paragraph *mark's* formatting. It sizes an otherwise
    /// empty paragraph, so it is not the same thing as the style's `w:rPr`.
    pub mark: RunProps,
}

/// Later wins, field by field. Nothing `src` leaves unstated is touched.
pub fn merge_run(dst: &mut RunProps, src: &RunProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(
        bold,
        italic,
        underline,
        strike,
        caps,
        small_caps,
        sz_half,
        color,
        highlight,
        shade,
        vert_align,
        spacing_dxa,
        vanish,
        r_style
    );
    // The font and its raw name are one statement in two fields: `w:rFonts`
    // setting the face must not leave the previous link's raw name behind, or the
    // symbol remap keys off a face that is no longer in effect.
    if src.font.is_some() {
        dst.font = src.font.clone();
        dst.font_raw = src.font_raw.clone();
    }
}

/// Later wins, field by field, including the paragraph mark's run properties and
/// each border side separately.
pub fn merge_para(dst: &mut ParaProps, src: &ParaProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(
        align,
        ind_start_dxa,
        ind_end_dxa,
        first_line_dxa,
        before_dxa,
        after_dxa,
        contextual_spacing,
        shade,
        num_id,
        ilvl,
        outline_lvl,
        bidi,
        p_style
    );
    // `w:line` and its rule are one statement: a link that restates the length
    // without a rule means `auto`, not "keep the inherited exact".
    if src.line.is_some() {
        dst.line = src.line;
        dst.line_rule = src.line_rule.or(Some(LineRule::Auto));
    } else if src.line_rule.is_some() {
        dst.line_rule = src.line_rule;
    }
    dst.borders.merge(&src.borders);
    merge_run(&mut dst.mark, &src.mark);
}

// ── ooxml → properties ───────────────────────────────────────────────────────

/// An OOXML on/off element: present means on, and only an explicit `0`/`false`/
/// `off` in `w:val` means off. Absent is not represented — the caller only calls
/// this once it has the element.
fn toggle(n: Node) -> bool {
    attr_local(n, "val").map(truthy).unwrap_or(true)
}

fn dxa(n: Node, name: &str) -> Option<i64> {
    let v: i64 = attr_local(n, name)?.trim().parse().ok()?;
    Some(v.clamp(-MAX_DXA, MAX_DXA))
}

/// Parses one `w:rPr`. `theme` resolves `w:*Theme` font and colour references.
pub fn parse_run_props(rpr: Node, theme: &Theme) -> RunProps {
    let mut p = RunProps::default();
    for e in elems(rpr) {
        match e.tag_name().name() {
            "b" => p.bold = Some(toggle(e)),
            "i" => p.italic = Some(toggle(e)),
            // `w:val="none"` is the off switch here, not a false-y boolean: every
            // other value names an underline style CSS cannot spell anyway.
            "u" => p.underline = Some(!matches!(attr_local(e, "val"), Some("none") | None)),
            "strike" | "dstrike" => {
                let on = toggle(e);
                // Two elements, one CSS decoration: an off `w:dstrike` must not
                // cancel an on `w:strike` in the same `w:rPr`.
                p.strike = Some(on || p.strike == Some(true));
            }
            "caps" => p.caps = Some(toggle(e)),
            "smallCaps" => p.small_caps = Some(toggle(e)),
            "vanish" => p.vanish = Some(toggle(e)),
            "sz" => {
                if let Some(v) = attr_local(e, "val").and_then(|v| v.trim().parse::<i64>().ok()) {
                    // 1pt to 2000pt: below 1 the text is invisible, above it the
                    // line box alone would exceed the preview's byte budget.
                    if (2..=4000).contains(&v) {
                        p.sz_half = Some(v);
                    }
                }
            }
            "spacing" => {
                if let Some(v) = dxa(e, "val") {
                    p.spacing_dxa = Some(v.clamp(-200, 200));
                }
            }
            "color" => p.color = Some(theme_color(e, theme)),
            "highlight" => {
                p.highlight = Some(match attr_local(e, "val").and_then(highlight_color) {
                    Some(rgb) => ColorVal::Set(Color::from_rgb(rgb)),
                    None => ColorVal::Auto,
                })
            }
            "shd" => p.shade = Some(shading(e, theme)),
            "vertAlign" => {
                p.vert_align = Some(match attr_local(e, "val") {
                    Some("superscript") => VertAlign::Super,
                    Some("subscript") => VertAlign::Sub,
                    _ => VertAlign::Baseline,
                })
            }
            "rFonts" => {
                if let Some(raw) = fonts_face(e, theme) {
                    p.font = Some(fonts::css_font_stack(&raw));
                    p.font_raw = Some(raw);
                }
            }
            "rStyle" => p.r_style = style_ref(e),
            _ => {}
        }
    }
    p
}

/// Parses one `w:pPr`, including its `w:rPr` (the paragraph mark).
pub fn parse_para_props(ppr: Node, theme: &Theme) -> ParaProps {
    let mut p = ParaProps::default();
    for e in elems(ppr) {
        match e.tag_name().name() {
            "jc" => p.align = attr_local(e, "val").and_then(jc),
            "ind" => {
                // `w:start`/`w:end` are the 2010+ logical spellings of
                // `w:left`/`w:right`; a document may carry either, and the
                // logical one wins when both are present.
                if let Some(v) = dxa(e, "start").or_else(|| dxa(e, "left")) {
                    p.ind_start_dxa = Some(v);
                }
                if let Some(v) = dxa(e, "end").or_else(|| dxa(e, "right")) {
                    p.ind_end_dxa = Some(v);
                }
                // Mutually exclusive in practice; hanging is the negative
                // direction of the same single model field.
                if let Some(v) = dxa(e, "firstLine") {
                    p.first_line_dxa = Some(v);
                }
                if let Some(v) = dxa(e, "hanging") {
                    p.first_line_dxa = Some(-v);
                }
            }
            "spacing" => {
                if let Some(v) = dxa(e, "before") {
                    p.before_dxa = Some(v.max(0));
                }
                if let Some(v) = dxa(e, "after") {
                    p.after_dxa = Some(v.max(0));
                }
                if let Some(v) = dxa(e, "line") {
                    p.line = Some(v.max(0));
                    p.line_rule = Some(LineRule::Auto);
                }
                if let Some(r) = attr_local(e, "lineRule") {
                    p.line_rule = Some(match r {
                        "exact" => LineRule::Exact,
                        "atLeast" => LineRule::AtLeast,
                        _ => LineRule::Auto,
                    });
                }
            }
            "contextualSpacing" => p.contextual_spacing = Some(toggle(e)),
            "pBdr" => p.borders = parse_borders(e, theme),
            "shd" => p.shade = Some(shading(e, theme)),
            "numPr" => {
                if let Some(v) = child(e, "numId").and_then(|n| xml::attr_i64(n, "val")) {
                    p.num_id = Some(v.clamp(0, i32::MAX as i64));
                }
                if let Some(v) = child(e, "ilvl").and_then(|n| xml::attr_i64(n, "val")) {
                    p.ilvl = Some(v.clamp(0, 8));
                }
            }
            "outlineLvl" => p.outline_lvl = xml::attr_i64(e, "val").map(|v| v.clamp(0, 9)),
            "bidi" => p.bidi = Some(toggle(e)),
            "pStyle" => p.p_style = style_ref(e),
            "rPr" => p.mark = parse_run_props(e, theme),
            _ => {}
        }
    }
    p
}

fn jc(v: &str) -> Option<Align> {
    Some(match v {
        "left" | "start" => Align::Left,
        "center" => Align::Center,
        "right" | "end" => Align::Right,
        // `distribute` stretches every line including the last; CSS
        // `text-align-last` could express that, but justify is the closer of the
        // two single-value answers.
        "both" | "distribute" | "justify" => Align::Justify,
        _ => return None,
    })
}

/// A `w:val` naming another style. Bounded because it is used as a map key and
/// echoed into no output, but an unbounded key from document XML is still a way
/// to make the memo table hold megabytes.
fn style_ref(n: Node) -> Option<String> {
    let v = attr_local(n, "val")?.trim();
    if v.is_empty() {
        return None;
    }
    Some(v.chars().take(128).collect())
}

fn parse_borders(pbdr: Node, theme: &Theme) -> BorderSides {
    let mut b = BorderSides::default();
    for e in elems(pbdr) {
        let spec = border_spec(e, theme);
        match e.tag_name().name() {
            "top" => b.top = Some(spec),
            "right" | "end" => b.right = Some(spec),
            "bottom" => b.bottom = Some(spec),
            "left" | "start" => b.left = Some(spec),
            _ => {}
        }
    }
    b
}

/// One edge element of a `w:pBdr`, `w:tblBorders` or `w:tcBorders`. All three use
/// the same attribute grammar, so the reading of it stays one copy.
fn border_spec(e: Node, theme: &Theme) -> BorderSpec {
    BorderSpec {
        // The name is kept as a `'static` str so the model's `Border` can
        // hold the CSS keyword without allocating; an unknown name still
        // draws, matching `border_css`.
        val: border_val(attr_local(e, "val").unwrap_or("single")),
        sz: attr_local(e, "sz")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|v| v.clamp(0, MAX_BORDER_EIGHTHS)),
        space_pt: attr_local(e, "space")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, MAX_BORDER_SPACE_PT)),
        // A border names its colour in `@w:color`; `@w:val` is the line style.
        color: Some(stated_color(
            e,
            theme,
            "color",
            "themeColor",
            "themeShade",
            "themeTint",
        )),
    }
}

/// `ST_Border` names that [`border_css`] knows, interned so the model can hold
/// them by reference. WordprocessingML has ~180 art borders on top of the shared
/// table-border names; the unknown ones fall through to `border_css`'s default,
/// which is a plain line — the same thing Word draws when it cannot find the art.
fn border_val(v: &str) -> &'static str {
    match v {
        "none" | "nil" => "none",
        "single" | "thinThickSmallGap" | "thickThinSmallGap" => "thin",
        "thick" => "thick",
        "double" | "triple" | "thinThickThinSmallGap" => "double",
        "dotted" | "dotDash" | "dotDotDash" => "dotted",
        "dashed" | "dashSmallGap" | "dashDotStroked" => "dashed",
        "wave" | "doubleWave" => "dashed",
        _ => "single",
    }
}

/// `w:shd`: the fill, with a pattern approximated by blending the pattern's own
/// colour into it. `w:val="clear"` is the flat case and by far the common one;
/// `nil` and a missing fill paint nothing.
fn shading(shd: Node, theme: &Theme) -> ColorVal {
    let val = attr_local(shd, "val").unwrap_or("clear");
    if val == "nil" {
        return ColorVal::Auto;
    }
    let fill = stated_color(shd, theme, "fill", "themeFill", "themeFillShade", "themeFillTint");
    if val == "clear" {
        return fill;
    }
    // Every other `w:val` is a pattern of the foreground colour over the fill at
    // some density. Nothing here rasterises a pattern, so it degrades to a flat
    // half-blend — which reads as "shaded" without inventing a texture.
    let fg = stated_color(shd, theme, "color", "themeColor", "themeShade", "themeTint");
    match (fill.color(), fg.color()) {
        (Some(f), Some(g)) => ColorVal::Set(f.mix(&g, 0.5)),
        (Some(f), None) => ColorVal::Set(f),
        // A pattern over an `auto` fill draws onto the page, so the pattern's own
        // colour is all there is to show.
        (None, Some(g)) => ColorVal::Set(g),
        (None, None) => ColorVal::Auto,
    }
}

/// The `w:color`-shaped attribute group: a literal hex in `hex_attr` (or `auto`),
/// else a theme slot with an optional shade/tint byte.
///
/// The attribute *names* differ per element — `w:color@w:val`, a `w:pBdr` side's
/// `@w:color`, a `w:shd`'s `@w:fill` and `@w:color` — while the resolution is
/// identical, so they are parameters rather than three copies of this.
fn stated_color(
    n: Node,
    theme: &Theme,
    hex_attr: &str,
    slot_attr: &str,
    shade_attr: &str,
    tint_attr: &str,
) -> ColorVal {
    if let Some(rgb) = attr_local(n, hex_attr)
        .filter(|v| *v != "auto")
        .and_then(parse_hex_rgb)
    {
        return ColorVal::Set(Color::from_rgb(rgb));
    }
    match theme_slot_color(n, theme, slot_attr, shade_attr, tint_attr) {
        Some(c) => ColorVal::Set(c),
        None => ColorVal::Auto,
    }
}

/// A `w:color`-shaped colour with the `w:val`/`w:themeColor` attribute names,
/// which is what every element outside `w:shd` and `w:pBdr` uses.
fn theme_color(n: Node, theme: &Theme) -> ColorVal {
    stated_color(n, theme, "val", "themeColor", "themeShade", "themeTint")
}

fn theme_slot_color(
    n: Node,
    theme: &Theme,
    slot_attr: &str,
    shade_attr: &str,
    tint_attr: &str,
) -> Option<Color> {
    let slot = theme_slot(attr_local(n, slot_attr)?)?;
    let base = Color::from_rgb(theme.color(slot));
    // Both are a hex byte out of 0xFF, not a percentage, and they are
    // alternatives rather than a pair. Word itself scales HSL *luminance* by the
    // fraction; blending towards black/white in sRGB instead lands on Word's own
    // answer exactly for a tint and within about 4/255 per channel for a shade,
    // which is not worth a second copy of the HSL round trip in this module.
    // Explicitly not DrawingML's `a:tint`, which is defined on linear light and
    // would be visibly wrong here.
    if let Some(f) = hex_byte(n, shade_attr) {
        return Some(base.mix(&Color::from_rgb(0x000000), 1.0 - f));
    }
    if let Some(f) = hex_byte(n, tint_attr) {
        return Some(base.mix(&Color::from_rgb(0xFFFFFF), 1.0 - f));
    }
    Some(base)
}

/// A two-hex-digit attribute as a 0..1 fraction of 255.
fn hex_byte(n: Node, name: &str) -> Option<f64> {
    let v = attr_local(n, name)?.trim();
    if v.len() < 2 || !v.is_char_boundary(2) {
        return None;
    }
    let b = u8::from_str_radix(&v[..2], 16).ok()?;
    Some(b as f64 / 255.0)
}

/// `w:themeColor` names. They are *not* the DrawingML slot names: `text1` and
/// `background1` are Word's spelling of the `dk1`/`lt1` pair, and `dark1` /
/// `light1` are legal aliases of the same two.
fn theme_slot(name: &str) -> Option<SchemeSlot> {
    Some(match name {
        "text1" | "dark1" => SchemeSlot::Dk1,
        "text2" | "dark2" => SchemeSlot::Dk2,
        "background1" | "light1" => SchemeSlot::Lt1,
        "background2" | "light2" => SchemeSlot::Lt2,
        "accent1" => SchemeSlot::Accent1,
        "accent2" => SchemeSlot::Accent2,
        "accent3" => SchemeSlot::Accent3,
        "accent4" => SchemeSlot::Accent4,
        "accent5" => SchemeSlot::Accent5,
        "accent6" => SchemeSlot::Accent6,
        "hyperlink" => SchemeSlot::Hlink,
        "followedHyperlink" => SchemeSlot::FolHlink,
        _ => return None,
    })
}

/// `w:highlight@w:val` — a fixed palette of *named* markers, unrelated to any
/// colour scheme, so it gets its own table. The values are the sixteen Word
/// offers in its highlighter; `none` and anything unknown paint nothing.
fn highlight_color(name: &str) -> Option<u32> {
    Some(match name {
        "yellow" => 0xFFFF00,
        "green" => 0x00FF00,
        "cyan" => 0x00FFFF,
        "magenta" => 0xFF00FF,
        "blue" => 0x0000FF,
        "red" => 0xFF0000,
        "darkBlue" => 0x000080,
        "darkCyan" => 0x008080,
        "darkGreen" => 0x008000,
        "darkMagenta" => 0x800080,
        "darkRed" => 0x800000,
        "darkYellow" => 0x808000,
        "darkGray" => 0x808080,
        "lightGray" => 0xC0C0C0,
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        _ => return None,
    })
}

/// The Latin face a `w:rFonts` states, resolving a `*Theme` attribute against
/// the theme's major/minor font. `w:ascii` covers ASCII and `w:hAnsi` the rest of
/// Latin-1; they differ only in documents that deliberately mix faces, so the
/// first one present wins.
fn fonts_face(rfonts: Node, theme: &Theme) -> Option<String> {
    for name in ["ascii", "hAnsi"] {
        if let Some(v) = attr_local(rfonts, name).map(str::trim).filter(|v| !v.is_empty()) {
            return Some(v.chars().take(128).collect());
        }
    }
    for name in ["asciiTheme", "hAnsiTheme"] {
        if let Some(v) = attr_local(rfonts, name) {
            if v.starts_with("major") {
                return Some(theme.major_font.clone());
            }
            if v.starts_with("minor") {
                return Some(theme.minor_font.clone());
            }
        }
    }
    None
}

// ── table properties ─────────────────────────────────────────────────────────

/// A `w:tblW` / `w:tcW` / `w:tblInd` measurement: `@w:w` read under `@w:type`.
///
/// `Auto` and `Nil` are both "no length here", kept apart because a `nil` is a
/// statement that overrides an inherited width while `auto` asks the layout to
/// decide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Width {
    Auto,
    /// Twentieths of a point.
    Dxa(i64),
    /// Percent of the enclosing box.
    Pct(f32),
    Nil,
}

/// `w:tblBorders` / `w:tcBorders`: the four edges, plus the two interior lines a
/// *table* states on behalf of the cells inside it. A cell's own `w:tcBorders`
/// uses the same edge elements and leaves the interior pair unstated.
///
/// The diagonals (`w:tl2br`, `w:tr2bl`) are not read: CSS has no cell diagonal,
/// and drawing one would need a gradient background per cell.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TableBorders {
    pub sides: BorderSides,
    pub inside_h: Option<BorderSpec>,
    pub inside_v: Option<BorderSpec>,
}

impl TableBorders {
    fn merge(&mut self, src: &TableBorders) {
        self.sides.merge(&src.sides);
        if src.inside_h.is_some() {
            self.inside_h = src.inside_h;
        }
        if src.inside_v.is_some() {
            self.inside_v = src.inside_v;
        }
    }
}

/// `w:tblCellMar` / `w:tcMar`, twentieths of a point per side. `None` inherits;
/// what an unstated side finally falls back to is Word's own default, which the
/// renderer states rather than this parser.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CellMargins {
    pub top: Option<i64>,
    pub right: Option<i64>,
    pub bottom: Option<i64>,
    pub left: Option<i64>,
}

impl CellMargins {
    pub fn merge(&mut self, src: &CellMargins) {
        for (dst, s) in [
            (&mut self.top, src.top),
            (&mut self.right, src.right),
            (&mut self.bottom, src.bottom),
            (&mut self.left, src.left),
        ] {
            if s.is_some() {
                *dst = s;
            }
        }
    }
}

/// `w:trHeight@w:hRule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HRule {
    Auto,
    Exact,
    AtLeast,
}

/// `w:vMerge`. The trap is that the two states are *not* spelled like the on/off
/// elements around them: `w:val="restart"` opens a vertical span and a `w:vMerge`
/// with **no** `w:val` at all means continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VMerge {
    Restart,
    Continue,
}

/// Table properties as one `w:tblPr` states them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableProps {
    /// `w:tblStyle`. Only ever present on a table's own `w:tblPr`; a table style
    /// names its parent with `w:basedOn` instead.
    pub style: Option<String>,
    pub width: Option<Width>,
    /// `w:jc` — how the table sits in the text column, not how its text aligns.
    pub align: Option<Align>,
    pub ind_dxa: Option<i64>,
    pub borders: TableBorders,
    pub shade: Option<ColorVal>,
    pub cell_mar: CellMargins,
}

/// Row properties as one `w:trPr` states them.
///
/// `w:tblHeader` is deliberately absent: it repeats a header row across page
/// breaks, and this renderer does not paginate. `w:cantSplit` is the same.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowProps {
    pub height_dxa: Option<i64>,
    pub height_rule: Option<HRule>,
    /// `w:trPr/w:del` — the row itself is a tracked deletion, not merely its text.
    pub del: bool,
}

/// Cell properties as one `w:tcPr` states them.
///
/// [`CellProps::grid_span`] and [`CellProps::v_merge`] are structure rather than
/// appearance, and a table *style* never states either usefully — the renderer
/// reads them from a cell's own `w:tcPr` only. They are parsed here because that
/// is where the element lives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellProps {
    pub width: Option<Width>,
    pub borders: TableBorders,
    pub shade: Option<ColorVal>,
    /// `w:vAlign`, already in the shared [`super::super::cellstyle::AlignSpec`]
    /// vocabulary (SpreadsheetML's), which spells docx's `both` as `justify`.
    pub v_align: Option<&'static str>,
    pub mar: CellMargins,
    pub no_wrap: Option<bool>,
    pub text_direction: Option<Rotation>,
    pub grid_span: Option<i64>,
    pub v_merge: Option<VMerge>,
}

pub fn merge_table(dst: &mut TableProps, src: &TableProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(style, width, align, ind_dxa, shade);
    dst.borders.merge(&src.borders);
    dst.cell_mar.merge(&src.cell_mar);
}

pub fn merge_row(dst: &mut RowProps, src: &RowProps) {
    // The length and its rule are one statement, as with `w:spacing@w:line`.
    if src.height_dxa.is_some() {
        dst.height_dxa = src.height_dxa;
        dst.height_rule = src.height_rule;
    }
    dst.del |= src.del;
}

pub fn merge_cell(dst: &mut CellProps, src: &CellProps) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(
        width,
        shade,
        v_align,
        no_wrap,
        text_direction,
        grid_span,
        v_merge
    );
    dst.borders.merge(&src.borders);
    dst.mar.merge(&src.mar);
}

/// Parses one `w:tblPr` (or a table style's base `w:tblPr`).
pub fn parse_table_props(tblpr: Node, theme: &Theme) -> TableProps {
    let mut p = TableProps::default();
    for e in elems(tblpr) {
        match e.tag_name().name() {
            "tblStyle" => p.style = style_ref(e),
            "tblW" => p.width = width(e),
            "jc" => p.align = attr_local(e, "val").and_then(jc),
            // An indent is a length or nothing: a percentage table indent has no
            // box to be a percentage of here.
            "tblInd" => {
                if let Some(Width::Dxa(v)) = width(e) {
                    p.ind_dxa = Some(v);
                }
            }
            "tblBorders" => p.borders = parse_table_borders(e, theme),
            "shd" => p.shade = Some(shading(e, theme)),
            "tblCellMar" => p.cell_mar = parse_cell_margins(e),
            _ => {}
        }
    }
    p
}

/// Parses one `w:trPr`. No theme: a row states no colour.
pub fn parse_row_props(trpr: Node) -> RowProps {
    let mut p = RowProps::default();
    for e in elems(trpr) {
        match e.tag_name().name() {
            "trHeight" => {
                p.height_dxa = xml::attr_i64(e, "val").map(|v| v.clamp(0, MAX_DXA));
                p.height_rule = Some(match attr_local(e, "hRule") {
                    Some("exact") => HRule::Exact,
                    Some("auto") => HRule::Auto,
                    // Word writes `atLeast` whenever it writes a height at all,
                    // and reading an absent rule as a *minimum* cannot clip the
                    // row's content the way `exact` would.
                    _ => HRule::AtLeast,
                });
            }
            "del" => p.del = true,
            _ => {}
        }
    }
    p
}

/// Parses one `w:tcPr` (or a table style's base `w:tcPr`).
pub fn parse_cell_props(tcpr: Node, theme: &Theme) -> CellProps {
    let mut p = CellProps::default();
    for e in elems(tcpr) {
        match e.tag_name().name() {
            "tcW" => p.width = width(e),
            "gridSpan" => {
                p.grid_span = xml::attr_i64(e, "val").map(|v| v.clamp(1, MAX_GRID_COLS))
            }
            "vMerge" => {
                p.v_merge = match attr_local(e, "val") {
                    // No `w:val` is the continuation, not the opener.
                    None | Some("continue") => Some(VMerge::Continue),
                    Some("restart") => Some(VMerge::Restart),
                    // `w:val` is `ST_Merge`, which has no other member; anything
                    // else is not a merge at all rather than a guess.
                    Some(_) => None,
                }
            }
            "tcBorders" => p.borders = parse_table_borders(e, theme),
            "shd" => p.shade = Some(shading(e, theme)),
            "vAlign" => p.v_align = attr_local(e, "val").and_then(v_align),
            "tcMar" => p.mar = parse_cell_margins(e),
            "noWrap" => p.no_wrap = Some(toggle(e)),
            "textDirection" => p.text_direction = attr_local(e, "val").and_then(text_direction),
            _ => {}
        }
    }
    p
}

fn parse_table_borders(node: Node, theme: &Theme) -> TableBorders {
    let mut b = TableBorders {
        sides: parse_borders(node, theme),
        ..Default::default()
    };
    for e in elems(node) {
        match e.tag_name().name() {
            "insideH" => b.inside_h = Some(border_spec(e, theme)),
            "insideV" => b.inside_v = Some(border_spec(e, theme)),
            _ => {}
        }
    }
    b
}

fn parse_cell_margins(node: Node) -> CellMargins {
    let mut m = CellMargins::default();
    for e in elems(node) {
        let v = match width(e) {
            Some(Width::Dxa(v)) => v.clamp(0, MAX_CELL_MAR_DXA),
            // A `nil` margin is zero padding, stated; a percentage one is not a
            // length Word ever writes here.
            Some(Width::Nil) => 0,
            _ => continue,
        };
        match e.tag_name().name() {
            "top" => m.top = Some(v),
            "right" | "end" => m.right = Some(v),
            "bottom" => m.bottom = Some(v),
            "left" | "start" => m.left = Some(v),
            _ => {}
        }
    }
    m
}

/// A `w:tblW`-shaped measurement. `@w:type` decides how `@w:w` reads, and its
/// schema default is `dxa`.
fn width(n: Node) -> Option<Width> {
    let raw = attr_local(n, "w").map(str::trim).unwrap_or("");
    match attr_local(n, "type").unwrap_or("dxa") {
        "nil" => Some(Width::Nil),
        "auto" => Some(Width::Auto),
        "pct" => {
            // Fiftieths of a percent, except in the strict schema where the same
            // attribute carries a percentage *string* with its own sign.
            let v = match raw.strip_suffix('%') {
                Some(s) => s.trim().parse::<f32>().ok().filter(|v| v.is_finite())?,
                None => pct50_to_pct(raw.parse::<i64>().ok()?),
            };
            Some(Width::Pct(v))
        }
        // dxa, and whatever a producer misspells it as: a length is the only
        // reading that can be acted on.
        _ => Some(Width::Dxa(raw.parse::<i64>().ok()?.clamp(-MAX_DXA, MAX_DXA))),
    }
}

/// `w:vAlign` in the shared cell vocabulary. docx spells the justified case
/// `both`; every other member is already SpreadsheetML's spelling.
fn v_align(v: &str) -> Option<&'static str> {
    Some(match v {
        "top" => "top",
        "center" => "center",
        "bottom" => "bottom",
        "both" => "justify",
        _ => return None,
    })
}

/// `w:textDirection` as a rotation. Only the two rotated flows differ from the
/// default here; the `V` variants turn the same way and differ only in how Word
/// orients East Asian glyphs inside the turned box, which is not modelled.
fn text_direction(v: &str) -> Option<Rotation> {
    Some(match v {
        // Bottom-to-top, left-to-right: a quarter turn counter-clockwise.
        "btLr" | "btLrV" | "vert270" => Rotation::Ccw(90.0),
        // Top-to-bottom, right-to-left: a quarter turn clockwise.
        "tbRl" | "tbRlV" | "tbLrV" | "vert" => Rotation::Ccw(-90.0),
        _ => return None,
    })
}

// ── styles.xml ───────────────────────────────────────────────────────────────

/// Which of the four `w:style@w:type` families a style belongs to. Only
/// `Paragraph` and `Character` participate in the text cascade; the other two are
/// recorded so a later table renderer can find them by id without reparsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Paragraph,
    Character,
    Table,
    Numbering,
}

#[derive(Debug, Clone)]
struct RawStyle {
    /// `w:name@w:val`, the display name. Load-bearing: `Heading 2` is how a
    /// document says "this is an `h2`" when it states no `w:outlineLvl`.
    name: String,
    based_on: Option<String>,
    para: ParaProps,
    run: RunProps,
    /// Base table/row/cell properties, boxed and present only for a
    /// `w:type="table"` style — a document defines thousands of paragraph styles
    /// and none of them carries any of this.
    table: Option<Box<TableStyle>>,
}

/// One table style's base `w:tblPr`/`w:trPr`/`w:tcPr` with its whole `w:basedOn`
/// chain folded in, root first.
#[derive(Debug, Clone, Default)]
pub struct TableStyle {
    pub table: TableProps,
    pub row: RowProps,
    pub cell: CellProps,
}

/// One style's properties with its whole `w:basedOn` chain already folded in,
/// root first, on top of `w:docDefaults`.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub para: ParaProps,
    pub run: RunProps,
    /// Heading level implied by a `Heading N` style *name* anywhere in the chain,
    /// most specific first. `w:outlineLvl` outranks it — see [`heading_of`].
    pub heading: Option<u8>,
}

/// The parsed `word/styles.xml`.
pub struct Styles {
    defaults: Resolved,
    styles: HashMap<String, RawStyle>,
    /// The style a paragraph with no `w:pStyle` resolves against. There is no
    /// `default_char` counterpart: a run starts from its paragraph's run
    /// properties, and Word's default character style ("Default Paragraph Font")
    /// states nothing of its own to add.
    default_para: Option<String>,
    /// Chains are walked per paragraph and a document reuses a handful of styles
    /// thousands of times, so each resolved chain is computed once. `RefCell`
    /// rather than a `&mut self` API: resolution is logically a read, and the
    /// renderer holds `Styles` immutably while walking the body.
    memo: RefCell<HashMap<String, Rc<Resolved>>>,
    /// The same, for table styles: a separate table because the two resolutions
    /// answer different questions about the same style ids.
    memo_table: RefCell<HashMap<String, Rc<TableStyle>>>,
}

impl Styles {
    /// docDefaults only — what a package with no (or an unparseable)
    /// `styles.xml` gets.
    pub fn empty() -> Styles {
        Styles {
            defaults: Resolved::default(),
            styles: HashMap::new(),
            default_para: None,
            memo: RefCell::new(HashMap::new()),
            memo_table: RefCell::new(HashMap::new()),
        }
    }

    /// Parses `word/styles.xml`. **Never fails**: a malformed part degrades to
    /// [`Styles::empty`], because a document whose styles cannot be read still
    /// renders — with Word's own defaults — and refusing to preview it at all is
    /// the worse answer.
    pub fn parse(xml: &str, theme: &Theme) -> Styles {
        let Ok(doc) = xml::parse(xml) else {
            return Styles::empty();
        };
        let mut s = Styles::empty();
        let root = doc.root_element();
        if let Some(dd) = child(root, "docDefaults") {
            if let Some(rpr) = child(dd, "rPrDefault").and_then(|n| child(n, "rPr")) {
                s.defaults.run = parse_run_props(rpr, theme);
            }
            if let Some(ppr) = child(dd, "pPrDefault").and_then(|n| child(n, "pPr")) {
                s.defaults.para = parse_para_props(ppr, theme);
            }
        }
        for e in elems(root).filter(|e| e.tag_name().name() == "style") {
            if s.styles.len() >= MAX_STYLES {
                break;
            }
            let Some(id) = attr_local(e, "styleId").map(|v| v.chars().take(128).collect::<String>())
            else {
                continue;
            };
            let kind = match attr_local(e, "type") {
                Some("character") => StyleKind::Character,
                Some("table") => StyleKind::Table,
                Some("numbering") => StyleKind::Numbering,
                // The schema's own default for `w:type` is `paragraph`.
                _ => StyleKind::Paragraph,
            };
            // The first `w:default="1"` paragraph style wins: the schema allows
            // only one, and a document with two is not a reason to prefer the
            // later. A default character, table or numbering style is recorded
            // like any other style — nothing resolves against it by default.
            if kind == StyleKind::Paragraph
                && s.default_para.is_none()
                && attr_local(e, "default").map(truthy).unwrap_or(false)
            {
                s.default_para = Some(id.clone());
            }
            let raw = RawStyle {
                name: child(e, "name")
                    .and_then(|n| attr_local(n, "val"))
                    .map(|v| v.chars().take(128).collect())
                    .unwrap_or_default(),
                based_on: child(e, "basedOn").and_then(style_ref),
                para: child(e, "pPr")
                    .map(|n| parse_para_props(n, theme))
                    .unwrap_or_default(),
                run: child(e, "rPr")
                    .map(|n| parse_run_props(n, theme))
                    .unwrap_or_default(),
                table: (kind == StyleKind::Table).then(|| {
                    Box::new(TableStyle {
                        table: child(e, "tblPr")
                            .map(|n| parse_table_props(n, theme))
                            .unwrap_or_default(),
                        row: child(e, "trPr").map(parse_row_props).unwrap_or_default(),
                        cell: child(e, "tcPr")
                            .map(|n| parse_cell_props(n, theme))
                            .unwrap_or_default(),
                    })
                }),
            };
            s.styles.insert(id, raw);
        }
        s
    }

    /// `w:docDefaults`, before any style.
    pub fn defaults(&self) -> &Resolved {
        &self.defaults
    }

    /// The style table's own view of itself, for the tests that check parsing
    /// rather than rendering. Nothing in the render path asks a style for its name
    /// or for which style is the default — `resolve` already folds both in — so
    /// these exist only under `cfg(test)` rather than as dead public API.
    #[cfg(test)]
    pub fn name(&self, id: &str) -> Option<&str> {
        self.styles.get(id).map(|s| s.name.as_str())
    }

    /// The paragraph style with `w:default="1"`, which a paragraph stating no
    /// `w:pStyle` takes.
    #[cfg(test)]
    pub fn default_para_style(&self) -> Option<&str> {
        self.default_para.as_deref()
    }

    /// docDefaults plus one paragraph style's whole `basedOn` chain, root first.
    /// `None` resolves the document's default paragraph style. The caller merges
    /// the numbering level's `w:pPr` and then the paragraph's own on top.
    pub fn resolve(&self, style_id: Option<&str>) -> Rc<Resolved> {
        let id = style_id.or_else(|| self.default_para.as_deref()).unwrap_or("");
        if let Some(hit) = self.memo.borrow().get(id) {
            return hit.clone();
        }
        let mut out = self.defaults.clone();
        for raw in self.chain(id) {
            merge_para(&mut out.para, &raw.para);
            merge_run(&mut out.run, &raw.run);
            if let Some(h) = heading_from_name(&raw.name) {
                out.heading = Some(h);
            }
        }
        let rc = Rc::new(out);
        self.memo.borrow_mut().insert(id.to_string(), rc.clone());
        rc
    }

    /// A `w:tblStyle` chain's base table, row and cell properties, root first.
    ///
    /// This is where most real tables get their whole appearance: a "Grid Table"
    /// style keeps its borders and shading in the base `w:tblPr`/`w:tcPr`, so
    /// without this every such table renders borderless — which reads as a bug
    /// rather than as a document.
    ///
    /// `w:tblStylePr` **conditional** formatting (first row, first column, row and
    /// column banding) is deliberately not read: it only redecorates a table the
    /// base properties already drew, and honouring it means a second cascade keyed
    /// on a cell's position plus the `w:tblLook` mask that says which conditions
    /// are even active.
    pub fn resolve_table(&self, style_id: &str) -> Rc<TableStyle> {
        if let Some(hit) = self.memo_table.borrow().get(style_id) {
            return hit.clone();
        }
        let mut out = TableStyle::default();
        for raw in self.chain(style_id) {
            if let Some(t) = raw.table.as_deref() {
                merge_table(&mut out.table, &t.table);
                merge_row(&mut out.row, &t.row);
                merge_cell(&mut out.cell, &t.cell);
            }
        }
        let rc = Rc::new(out);
        self.memo_table
            .borrow_mut()
            .insert(style_id.to_string(), rc.clone());
        rc
    }

    /// A `w:rStyle` chain's run properties, root first — *without* docDefaults,
    /// which the paragraph path has already contributed.
    pub fn resolve_char(&self, style_id: &str) -> RunProps {
        let mut out = RunProps::default();
        for raw in self.chain(style_id) {
            merge_run(&mut out, &raw.run);
        }
        out
    }

    /// The `basedOn` chain of `id`, **root first**. Cycle-safe: a style already
    /// on the chain ends the walk, and [`MAX_BASED_ON`] bounds it even for a
    /// pathological graph the visited check somehow misses (it cannot, but the
    /// cost of the belt is one comparison per link).
    fn chain(&self, id: &str) -> Vec<&RawStyle> {
        let mut out: Vec<&RawStyle> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        let mut cur = id;
        while !cur.is_empty() && out.len() < MAX_BASED_ON {
            if seen.contains(&cur) {
                break;
            }
            seen.push(cur);
            let Some(raw) = self.styles.get(cur) else { break };
            out.push(raw);
            cur = raw.based_on.as_deref().unwrap_or("");
        }
        out.reverse();
        out
    }
}

/// `Heading 1` … `Heading 6`, in the built-in English style names Word writes
/// into `w:name` regardless of UI language. Levels past 6 have no HTML element,
/// so they stay body text as far as the heading structure is concerned.
fn heading_from_name(name: &str) -> Option<u8> {
    let rest = name.strip_prefix("Heading ").or_else(|| name.strip_prefix("heading "))?;
    match rest.trim().parse::<u8>() {
        Ok(n) if (1..=6).contains(&n) => Some(n),
        _ => None,
    }
}

/// The heading level of a paragraph: `w:outlineLvl` if it states one, else the
/// level implied by its style's name.
///
/// `w:outlineLvl` outranks the name because it is what Word's own navigation
/// pane keys off, and a document that renames `Heading 1` keeps the level.
/// Level 9 is the schema's "body text" and is therefore *not* a heading — the
/// only value that can un-head an otherwise heading-named style.
pub fn heading_of(pp: &ParaProps, from_style: Option<u8>) -> Option<u8> {
    match pp.outline_lvl {
        Some(9) => None,
        // 6..8 have no element of their own; they collapse onto `h6` rather than
        // silently losing their place in the outline.
        Some(l) => Some((l.clamp(0, 5) + 1) as u8),
        None => from_style,
    }
}

// ── properties → model ───────────────────────────────────────────────────────

/// The size the paragraph and its runs are measured against, in points.
pub fn size_pt(rp: &RunProps) -> f32 {
    rp.sz_half.map(|v| v as f32 / 2.0).unwrap_or(DEFAULT_SZ_PT)
}

/// The paragraph box, with no runs and no marker: the body walk fills those in,
/// and the numbering module owns the marker.
///
/// `base_pt` is the size the paragraph mark resolves to, which is what sizes an
/// empty line and what each run's size is compared against.
pub fn to_para(pp: &ParaProps, base_pt: f32, heading: Option<u8>) -> Para {
    Para {
        runs: Vec::new(),
        size_pt: base_pt,
        align: pp.align,
        indent_px: pp.ind_start_dxa.map(dxa_to_px).unwrap_or(0.0),
        indent_end_px: pp.ind_end_dxa.map(dxa_to_px).unwrap_or(0.0),
        first_line_px: pp.first_line_dxa.map(dxa_to_px).unwrap_or(0.0),
        line: line_height(pp),
        space_before_px: pp.before_dxa.map(dxa_to_px).unwrap_or(0.0),
        space_after_px: pp.after_dxa.map(dxa_to_px).unwrap_or(0.0),
        marker: None,
        rtl: pp.bidi == Some(true),
        shade: pp.shade.and_then(ColorVal::color),
        borders: to_borders(&pp.borders),
        heading,
    }
}

/// `w:spacing@w:line` under its rule. `auto` is in 240ths of a line, the other
/// two in twentieths of a point.
fn line_height(pp: &ParaProps) -> LineHeight {
    let Some(v) = pp.line.filter(|v| *v > 0) else {
        return LineHeight::default();
    };
    match pp.line_rule.unwrap_or(LineRule::Auto) {
        // 240 is single. The model's multiplier is against the font's line, not
        // the em box, so the ratio carries `SINGLE_LINE` with it.
        LineRule::Auto => LineHeight::Multiple((v as f32 / 240.0).clamp(0.1, 20.0) * model::SINGLE_LINE),
        LineRule::Exact => LineHeight::Exact(dxa_to_px(v)),
        LineRule::AtLeast => LineHeight::AtLeast(dxa_to_px(v)),
    }
}

fn to_borders(b: &BorderSides) -> Borders {
    if b.is_none() {
        return Borders::default();
    }
    Borders {
        top: b.top.and_then(to_border),
        right: b.right.and_then(to_border),
        bottom: b.bottom.and_then(to_border),
        left: b.left.and_then(to_border),
    }
}

/// One edge through the shared `ST_BorderStyle` table. `w:sz` overrides the
/// width that table implies, which is why the pair is parsed rather than
/// reimplemented: the CSS keyword must keep coming from one place.
///
/// `None` is an edge that draws nothing (`w:val="none"`/`"nil"`), which is a
/// statement — it cancels an inherited border rather than leaving it alone.
pub fn to_border(spec: BorderSpec) -> Option<Border> {
    let (w, style) = border_css(spec.val)?;
    let width_px = match spec.sz.filter(|v| *v > 0) {
        Some(sz) => eighth_pt_to_px(sz),
        // The table's widths are whole px strings; a parse failure would mean the
        // table gained a unit, so falling back to a hairline is safe either way.
        None => w.trim_end_matches("px").parse::<f32>().unwrap_or(1.0),
    };
    Some(Border {
        width_px,
        style,
        color: spec.color.and_then(ColorVal::color),
        space_px: spec.space_pt.map(pt_to_px).unwrap_or(0.0),
    })
}

/// One run's text with its resolved properties.
///
/// `w:vanish` is *not* honoured here: a hidden run must not reach the search-term
/// highlighter either, so the body walk drops it before calling this — see
/// [`RunProps::vanish`].
pub fn to_text_run(text: String, rp: &RunProps, base_pt: f32) -> TextRun {
    TextRun {
        text,
        // The model has no "inherit": a run that states no size carries the
        // paragraph's, so the emitter can tell the two apart.
        size_pt: rp.sz_half.map(|v| v as f32 / 2.0).unwrap_or(base_pt),
        bold: rp.bold == Some(true),
        italic: rp.italic == Some(true),
        underline: rp.underline == Some(true),
        strike: rp.strike == Some(true),
        color: rp.color.and_then(ColorVal::color),
        font: rp.font.clone(),
        caps: if rp.caps == Some(true) {
            Some(Caps::All)
        } else if rp.small_caps == Some(true) {
            Some(Caps::Small)
        } else {
            None
        },
        letter_spacing_pt: rp.spacing_dxa.map(|v| v as f32 / 20.0).unwrap_or(0.0),
        script: match rp.vert_align {
            Some(VertAlign::Super) => Some(Script::Super),
            Some(VertAlign::Sub) => Some(Script::Sub),
            _ => None,
        },
        // A run-level `w:shd` paints the same band as a highlight; an explicit
        // highlighter pen wins over it.
        highlight: rp
            .highlight
            .and_then(ColorVal::color)
            .or_else(|| rp.shade.and_then(ColorVal::color)),
        // A link is a property of the `w:hyperlink` *around* the run, not of the
        // run's own `w:rPr`, so the body walk fills it in — see
        // `body::text_run`, which also decides what an unstyled link looks like.
        link: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    /// accent1 is the only slot the theme expectations below depend on.
    fn theme() -> Theme {
        let mut t = Theme::default();
        t.colors.accent1 = 0x4472C4;
        t.major_font = "Cambria".to_string();
        t.minor_font = "Calibri".to_string();
        t
    }

    fn styles(body: &str) -> Styles {
        Styles::parse(&format!("<w:styles {}>{}</w:styles>", NS, body), &theme())
    }

    fn ppr(body: &str) -> ParaProps {
        let src = format!("<w:pPr {}>{}</w:pPr>", NS, body);
        let doc = xml::parse(&src).expect("fixture parses");
        parse_para_props(doc.root_element(), &theme())
    }

    fn rpr(body: &str) -> RunProps {
        let src = format!("<w:rPr {}>{}</w:rPr>", NS, body);
        let doc = xml::parse(&src).expect("fixture parses");
        parse_run_props(doc.root_element(), &theme())
    }

    // ── resolution ───────────────────────────────────────────────────────────

    #[test]
    fn doc_defaults_reach_a_paragraph_that_states_nothing() {
        let s = styles(
            r#"<w:docDefaults>
                 <w:rPrDefault><w:rPr><w:sz w:val="22"/><w:b/></w:rPr></w:rPrDefault>
                 <w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/>
                 </w:pPr></w:pPrDefault>
               </w:docDefaults>"#,
        );
        // No `w:pStyle` and no default style either: docDefaults is the whole
        // cascade, and it must not be lost just because there is no style to hang
        // it on.
        let r = s.resolve(None);
        assert_eq!(r.run.sz_half, Some(22));
        assert_eq!(r.run.bold, Some(true));
        assert_eq!(r.para.after_dxa, Some(160));
        assert_eq!(size_pt(&r.run), 11.0);
        assert_eq!(r.heading, None);
    }

    #[test]
    fn a_three_deep_chain_merges_root_first() {
        let s = styles(
            r#"<w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="20"/>
                 <w:rFonts w:ascii="Widget Sans"/></w:rPr></w:rPrDefault></w:docDefaults>
               <w:style w:type="paragraph" w:styleId="Normal" w:default="1">
                 <w:name w:val="Normal"/>
                 <w:pPr><w:jc w:val="left"/><w:ind w:left="100"/></w:pPr>
                 <w:rPr><w:sz w:val="24"/><w:i/></w:rPr>
               </w:style>
               <w:style w:type="paragraph" w:styleId="Mid">
                 <w:name w:val="Mid"/><w:basedOn w:val="Normal"/>
                 <w:pPr><w:jc w:val="center"/></w:pPr>
                 <w:rPr><w:sz w:val="28"/></w:rPr>
               </w:style>
               <w:style w:type="paragraph" w:styleId="Leaf">
                 <w:name w:val="Leaf"/><w:basedOn w:val="Mid"/>
                 <w:pPr><w:jc w:val="right"/></w:pPr>
               </w:style>"#,
        );
        let r = s.resolve(Some("Leaf"));
        // Leaf wins the alignment, Mid the size, Normal keeps the indent it alone
        // states, and docDefaults keeps the font nobody overrode.
        assert_eq!(r.para.align, Some(Align::Right));
        assert_eq!(r.run.sz_half, Some(28));
        assert_eq!(r.para.ind_start_dxa, Some(100));
        assert_eq!(r.run.italic, Some(true));
        assert_eq!(r.run.font_raw.as_deref(), Some("Widget Sans"));
        // The memo must hand back the same resolution, not a fresh one.
        assert!(Rc::ptr_eq(&r, &s.resolve(Some("Leaf"))));
        // `None` picks the `w:default="1"` paragraph style.
        assert_eq!(s.default_para_style(), Some("Normal"));
        assert_eq!(s.resolve(None).para.align, Some(Align::Left));
        // An id no style defines is not an error: docDefaults still apply.
        assert_eq!(s.resolve(Some("café")).run.sz_half, Some(20));
    }

    #[test]
    fn a_based_on_cycle_terminates() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="A"><w:name w:val="A"/>
                 <w:basedOn w:val="C"/><w:pPr><w:jc w:val="left"/></w:pPr></w:style>
               <w:style w:type="paragraph" w:styleId="B"><w:name w:val="B"/>
                 <w:basedOn w:val="A"/><w:rPr><w:b/></w:rPr></w:style>
               <w:style w:type="paragraph" w:styleId="C"><w:name w:val="C"/>
                 <w:basedOn w:val="B"/><w:pPr><w:jc w:val="center"/></w:pPr></w:style>
               <w:style w:type="paragraph" w:styleId="Self"><w:name w:val="Self"/>
                 <w:basedOn w:val="Self"/><w:rPr><w:i/></w:rPr></w:style>"#,
        );
        // Every entry point of the cycle resolves, and each style in it still
        // contributes exactly once.
        for id in ["A", "B", "C"] {
            let r = s.resolve(Some(id));
            assert_eq!(r.run.bold, Some(true), "{id} lost B's rPr");
            assert!(r.para.align.is_some(), "{id} lost the jc");
        }
        // A self-based style is the degenerate cycle.
        assert_eq!(s.resolve(Some("Self")).run.italic, Some(true));
    }

    #[test]
    fn direct_formatting_beats_an_inherited_toggle() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="Strong"><w:name w:val="Strong"/>
                 <w:rPr><w:b/><w:i/></w:rPr></w:style>"#,
        );
        let mut run = s.resolve(Some("Strong")).run.clone();
        assert_eq!(run.bold, Some(true));
        // Word would XOR this pair into "bold"; plain later-wins turns it off,
        // which is what the module doc promises.
        merge_run(&mut run, &rpr(r#"<w:b w:val="false"/>"#));
        assert_eq!(run.bold, Some(false));
        assert_eq!(run.italic, Some(true), "the untouched toggle must survive");
        assert!(!to_text_run("café".into(), &run, 11.0).bold);
        // The three off spellings, and the bare element that means on.
        for off in ["0", "false", "off"] {
            let r = rpr(&format!(r#"<w:b w:val="{off}"/>"#));
            assert_eq!(r.bold, Some(false), "{off}");
        }
        assert_eq!(rpr("<w:b/>").bold, Some(true));
        // Absent is inherit, not off.
        assert_eq!(rpr("<w:i/>").bold, None);
    }

    #[test]
    fn a_character_style_chain_carries_only_run_properties() {
        let s = styles(
            r#"<w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="20"/></w:rPr>
               </w:rPrDefault></w:docDefaults>
               <w:style w:type="character" w:styleId="Base"><w:name w:val="Base"/>
                 <w:rPr><w:i/></w:rPr></w:style>
               <w:style w:type="character" w:styleId="Emph"><w:name w:val="Emph"/>
                 <w:basedOn w:val="Base"/><w:rPr><w:b/></w:rPr></w:style>"#,
        );
        let r = s.resolve_char("Emph");
        assert_eq!((r.bold, r.italic), (Some(true), Some(true)));
        // docDefaults belong to the paragraph path; adding them again here would
        // let a character style's size beat the paragraph's.
        assert_eq!(r.sz_half, None);
        assert_eq!(s.name("Base"), Some("Base"));
    }

    #[test]
    fn a_malformed_or_missing_part_degrades_to_doc_defaults() {
        for src in ["not xml at all", "<w:styles", ""] {
            let s = Styles::parse(src, &theme());
            assert!(s.resolve(None).para == ParaProps::default());
            assert_eq!(s.default_para_style(), None);
        }
        // A style with no id, no name and an unknown type is skipped rather than
        // poisoning the table.
        let s = styles(r#"<w:style w:type="café"><w:rPr><w:b/></w:rPr></w:style>"#);
        assert_eq!(s.resolve(None).run.bold, None);
        assert_eq!(Styles::empty().resolve(None).heading, None);
    }

    // ── properties ───────────────────────────────────────────────────────────

    #[test]
    fn first_line_and_hanging_fold_into_one_signed_value() {
        assert_eq!(ppr(r#"<w:ind w:firstLine="720"/>"#).first_line_dxa, Some(720));
        assert_eq!(ppr(r#"<w:ind w:hanging="360"/>"#).first_line_dxa, Some(-360));
        // Both spellings of the leading/trailing edge, logical winning over
        // physical when a producer writes both.
        let p = ppr(r#"<w:ind w:left="720" w:right="360"/>"#);
        assert_eq!((p.ind_start_dxa, p.ind_end_dxa), (Some(720), Some(360)));
        let p = ppr(r#"<w:ind w:left="100" w:start="720" w:right="100" w:end="360"/>"#);
        assert_eq!((p.ind_start_dxa, p.ind_end_dxa), (Some(720), Some(360)));
        // 720 dxa = 36pt = 48px, negated for the hang.
        let para = to_para(&ppr(r#"<w:ind w:left="720" w:hanging="720"/>"#), 11.0, None);
        assert_eq!(para.indent_px, 48.0);
        assert_eq!(para.first_line_px, -48.0);
        // Absurd values are clamped rather than pushing the column off the page.
        assert_eq!(ppr(r#"<w:ind w:left="99999999"/>"#).ind_start_dxa, Some(MAX_DXA));
        assert_eq!(ppr(r#"<w:ind w:left="naïve"/>"#).ind_start_dxa, None);
    }

    #[test]
    fn all_three_line_rules_reach_the_model() {
        // 480/240 = double spacing, as a multiple of the font's line.
        let m = to_para(&ppr(r#"<w:spacing w:line="480" w:lineRule="auto"/>"#), 11.0, None);
        assert_eq!(m.line, LineHeight::Multiple(2.0 * model::SINGLE_LINE));
        // exact/atLeast are twentieths of a point: 360 dxa = 18pt = 24px.
        let e = to_para(&ppr(r#"<w:spacing w:line="360" w:lineRule="exact"/>"#), 11.0, None);
        assert_eq!(e.line, LineHeight::Exact(24.0));
        let a = to_para(&ppr(r#"<w:spacing w:line="360" w:lineRule="atLeast"/>"#), 11.0, None);
        assert_eq!(a.line, LineHeight::AtLeast(24.0));
        // A length with no rule is `auto`, and no length at all is the default.
        let d = to_para(&ppr(r#"<w:spacing w:line="240"/>"#), 11.0, None);
        assert_eq!(d.line, LineHeight::default());
        assert_eq!(to_para(&ppr(""), 11.0, None).line, LineHeight::default());
        // A stronger link restating only the length must not keep an inherited
        // `exact`, which would clip the taller line it asked for.
        let mut p = ppr(r#"<w:spacing w:line="360" w:lineRule="exact"/>"#);
        merge_para(&mut p, &ppr(r#"<w:spacing w:line="480"/>"#));
        assert_eq!(p.line_rule, Some(LineRule::Auto));
        // Spacing before/after, 240 dxa = 12pt = 16px.
        let sp = to_para(&ppr(r#"<w:spacing w:before="240" w:after="120"/>"#), 11.0, None);
        assert_eq!((sp.space_before_px, sp.space_after_px), (16.0, 8.0));
    }

    #[test]
    fn half_points_reach_the_size_in_points() {
        // 24 half-points is 12pt, on the paragraph and on the run alike.
        assert_eq!(size_pt(&rpr(r#"<w:sz w:val="24"/>"#)), 12.0);
        assert_eq!(size_pt(&rpr("")), DEFAULT_SZ_PT);
        let t = to_text_run("café".into(), &rpr(r#"<w:sz w:val="19"/>"#), 11.0);
        assert_eq!(t.size_pt, 9.5);
        // A run that states nothing carries the paragraph's size, not a default.
        assert_eq!(to_text_run("café".into(), &rpr(""), 18.0).size_pt, 18.0);
        // Out-of-range sizes are ignored rather than clamped to something the
        // document did not ask for.
        assert_eq!(rpr(r#"<w:sz w:val="0"/>"#).sz_half, None);
        assert_eq!(rpr(r#"<w:sz w:val="99999"/>"#).sz_half, None);
        assert_eq!(rpr(r#"<w:sz w:val="café"/>"#).sz_half, None);
    }

    #[test]
    fn run_formatting_lands_on_the_model_run() {
        let r = rpr(
            r#"<w:b/><w:i/><w:u w:val="single"/><w:strike/><w:smallCaps/>
               <w:color w:val="00FF00"/><w:spacing w:val="20"/>
               <w:vertAlign w:val="superscript"/><w:rFonts w:ascii="Widget Sans"/>"#,
        );
        let t = to_text_run("naïve".into(), &r, 11.0);
        assert!(t.bold && t.italic && t.underline && t.strike);
        assert_eq!(t.caps, Some(Caps::Small));
        assert_eq!(t.color, Some(Color::from_rgb(0x00FF00)));
        assert_eq!(t.script, Some(Script::Super));
        assert_eq!(t.letter_spacing_pt, 1.0);
        assert!(t.font.as_deref().unwrap().contains("Widget Sans"));
        // `w:caps` outranks `w:smallCaps`, and an underline `none` is the off
        // switch rather than a false-y boolean.
        let r = rpr(r#"<w:caps/><w:smallCaps/><w:u w:val="none"/>"#);
        assert_eq!(to_text_run("café".into(), &r, 11.0).caps, Some(Caps::All));
        assert_eq!(r.underline, Some(false));
        // `w:color w:val="auto"` is a statement that overrides an inherited
        // colour back to the reader's default.
        let mut c = rpr(r#"<w:color w:val="FF0000"/>"#);
        merge_run(&mut c, &rpr(r#"<w:color w:val="auto"/>"#));
        assert_eq!(c.color, Some(ColorVal::Auto));
        assert_eq!(to_text_run("café".into(), &c, 11.0).color, None);
        // Hidden text is reported, never silently dropped here.
        assert_eq!(rpr("<w:vanish/>").vanish, Some(true));
        assert_eq!(rpr(r#"<w:vanish w:val="0"/>"#).vanish, Some(false));
        // A double strike is still one CSS line-through, and an off `w:dstrike`
        // beside an on `w:strike` must not cancel it.
        assert_eq!(rpr("<w:dstrike/>").strike, Some(true));
        assert_eq!(rpr(r#"<w:strike/><w:dstrike w:val="0"/>"#).strike, Some(true));
    }

    #[test]
    fn a_named_highlight_resolves_to_a_colour() {
        let y = rpr(r#"<w:highlight w:val="yellow"/>"#);
        assert_eq!(y.highlight, Some(ColorVal::Set(Color::from_rgb(0xFFFF00))));
        assert_eq!(
            to_text_run("Widget".into(), &y, 11.0).highlight,
            Some(Color::from_rgb(0xFFFF00))
        );
        assert_eq!(
            rpr(r#"<w:highlight w:val="darkCyan"/>"#).highlight,
            Some(ColorVal::Set(Color::from_rgb(0x008080)))
        );
        // `none` is the off switch and beats an inherited marker.
        let mut h = y.clone();
        merge_run(&mut h, &rpr(r#"<w:highlight w:val="none"/>"#));
        assert_eq!(h.highlight, Some(ColorVal::Auto));
        assert_eq!(to_text_run("Widget".into(), &h, 11.0).highlight, None);
        // An unknown keyword is not an RGB value: it paints nothing.
        assert_eq!(
            rpr(r#"<w:highlight w:val="FFFF00"/>"#).highlight,
            Some(ColorVal::Auto)
        );
        // A run-level shading paints the same band, and the pen wins over it.
        let s = rpr(r#"<w:shd w:val="clear" w:fill="EEEEEE"/>"#);
        assert_eq!(
            to_text_run("café".into(), &s, 11.0).highlight,
            Some(Color::from_rgb(0xEEEEEE))
        );
        let both = rpr(r#"<w:highlight w:val="cyan"/><w:shd w:val="clear" w:fill="EEEEEE"/>"#);
        assert_eq!(
            to_text_run("café".into(), &both, 11.0).highlight,
            Some(Color::from_rgb(0x00FFFF))
        );
    }

    #[test]
    fn theme_colours_resolve_through_the_scheme_with_a_shade_or_tint() {
        // Plain slot.
        assert_eq!(
            rpr(r#"<w:color w:themeColor="accent1"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0x4472C4)))
        );
        // themeShade="BF" is 0xBF/0xFF of the slot towards black. Word's own
        // answer for "darker 25%" is #2F5597; it scales HSL luminance instead, so
        // the two differ by a few units per channel by design.
        assert_eq!(
            rpr(r#"<w:color w:themeColor="accent1" w:themeShade="BF"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0x335593)))
        );
        // themeTint="66" is Word's "lighter 60%", which this does hit exactly.
        assert_eq!(
            rpr(r#"<w:color w:themeColor="accent1" w:themeTint="66"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0xB4C7E7)))
        );
        // Word's own slot names, not DrawingML's: text1 is dk1, background1 lt1.
        assert_eq!(
            rpr(r#"<w:color w:themeColor="text1"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0x000000)))
        );
        assert_eq!(
            rpr(r#"<w:color w:themeColor="background1"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0xFFFFFF)))
        );
        // A literal `w:val` outranks the theme reference beside it.
        assert_eq!(
            rpr(r#"<w:color w:val="FF0000" w:themeColor="accent1"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0xFF0000)))
        );
        // Unknown slot, and a junk shade byte.
        assert_eq!(rpr(r#"<w:color w:themeColor="café"/>"#).color, Some(ColorVal::Auto));
        assert_eq!(
            rpr(r#"<w:color w:themeColor="accent1" w:themeShade="ï"/>"#).color,
            Some(ColorVal::Set(Color::from_rgb(0x4472C4)))
        );
    }

    #[test]
    fn theme_fonts_resolve_to_the_major_and_minor_faces() {
        assert_eq!(
            rpr(r#"<w:rFonts w:asciiTheme="majorHAnsi"/>"#).font_raw.as_deref(),
            Some("Cambria")
        );
        assert_eq!(
            rpr(r#"<w:rFonts w:hAnsiTheme="minorHAnsi"/>"#).font_raw.as_deref(),
            Some("Calibri")
        );
        // An explicit face outranks the theme reference.
        assert_eq!(
            rpr(r#"<w:rFonts w:ascii="Widget Sans" w:asciiTheme="majorHAnsi"/>"#)
                .font_raw
                .as_deref(),
            Some("Widget Sans")
        );
        // Restating the face must replace the raw name too, or the symbol remap
        // keys off a face that is no longer in effect.
        let mut f = rpr(r#"<w:rFonts w:ascii="Wingdings"/>"#);
        merge_run(&mut f, &rpr(r#"<w:rFonts w:ascii="Widget Sans"/>"#));
        assert_eq!(f.font_raw.as_deref(), Some("Widget Sans"));
        assert_eq!(rpr(r#"<w:rFonts w:ascii=""/>"#).font_raw, None);
    }

    #[test]
    fn paragraph_borders_go_through_the_shared_border_table() {
        let p = ppr(
            r#"<w:pBdr>
                 <w:top w:val="single" w:sz="24" w:space="4" w:color="0000FF"/>
                 <w:bottom w:val="double" w:color="auto"/>
                 <w:left w:val="none"/>
               </w:pBdr>"#,
        );
        let b = to_para(&p, 11.0, None).borders;
        // 24 eighths = 3pt = 4px; the width comes from `w:sz`, the CSS keyword
        // from `border_css`.
        let top = b.top.expect("top edge");
        assert_eq!((top.width_px, top.style), (4.0, "solid"));
        assert_eq!(top.color, Some(Color::from_rgb(0x0000FF)));
        assert_eq!(top.space_px, pt_to_px(4.0));
        // No `w:sz`: the width the shared table implies, and `auto` leaves the
        // edge at the text colour.
        let bottom = b.bottom.expect("bottom edge");
        assert_eq!((bottom.width_px, bottom.style, bottom.color), (3.0, "double", None));
        // `w:val="none"` is not a zero-width border, it is no border.
        assert!(b.left.is_none() && b.right.is_none());
        // Each side merges on its own: a link that mentions only `w:top` must not
        // erase an inherited bottom.
        let mut merged = p.clone();
        merge_para(
            &mut merged,
            &ppr(r#"<w:pBdr><w:top w:val="dotted"/></w:pBdr>"#),
        );
        assert_eq!(merged.borders.top.unwrap().val, "dotted");
        assert!(merged.borders.bottom.is_some());
        // A paragraph with no `w:pBdr` at all costs no borders.
        assert_eq!(to_para(&ppr(""), 11.0, None).borders, Borders::default());
    }

    #[test]
    fn paragraph_shading_paints_the_box_and_a_pattern_blends() {
        let p = ppr(r#"<w:shd w:val="clear" w:fill="EEEEEE"/>"#);
        assert_eq!(to_para(&p, 11.0, None).shade, Some(Color::from_rgb(0xEEEEEE)));
        // `nil` and an `auto` fill paint nothing, and both are statements that
        // override an inherited fill.
        let mut off = p.clone();
        merge_para(&mut off, &ppr(r#"<w:shd w:val="nil"/>"#));
        assert_eq!(to_para(&off, 11.0, None).shade, None);
        assert_eq!(to_para(&ppr(r#"<w:shd w:val="clear" w:fill="auto"/>"#), 11.0, None).shade, None);
        // A pattern degrades to a flat half-blend of fill and pattern colour
        // rather than inventing a texture.
        let pat = ppr(r#"<w:shd w:val="pct50" w:fill="FFFFFF" w:color="000000"/>"#);
        assert_eq!(to_para(&pat, 11.0, None).shade, Some(Color::from_rgb(0x808080)));
        // A themed fill resolves through the scheme like any other colour.
        assert_eq!(
            to_para(&ppr(r#"<w:shd w:val="clear" w:themeFill="accent1"/>"#), 11.0, None).shade,
            Some(Color::from_rgb(0x4472C4))
        );
    }

    #[test]
    fn heading_comes_from_the_outline_level_or_the_style_name() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="H2"><w:name w:val="Heading 2"/></w:style>
               <w:style w:type="paragraph" w:styleId="Sub"><w:name w:val="Widget Caption"/>
                 <w:basedOn w:val="H2"/></w:style>
               <w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/>
                 <w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style>
               <w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Heading 3"/>
                 <w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>"#,
        );
        // From the name, including through a rename that keeps the chain.
        let h2 = s.resolve(Some("H2"));
        assert_eq!(heading_of(&h2.para, h2.heading), Some(2));
        let sub = s.resolve(Some("Sub"));
        assert_eq!(heading_of(&sub.para, sub.heading), Some(2));
        // From `w:outlineLvl`, which is 0-based.
        let t = s.resolve(Some("Title"));
        assert_eq!(heading_of(&t.para, t.heading), Some(1));
        // Level 9 is the schema's "body text" and un-heads even a heading name.
        let b = s.resolve(Some("Body"));
        assert_eq!(b.heading, Some(3));
        assert_eq!(heading_of(&b.para, b.heading), None);
        // A direct `w:outlineLvl` outranks the style's name.
        let mut direct = h2.para.clone();
        merge_para(&mut direct, &ppr(r#"<w:outlineLvl w:val="4"/>"#));
        assert_eq!(heading_of(&direct, h2.heading), Some(5));
        // Levels past 6 have no element of their own and collapse onto h6.
        assert_eq!(heading_of(&ppr(r#"<w:outlineLvl w:val="8"/>"#), None), Some(6));
        // Names that only look like a heading, and levels out of range.
        assert_eq!(heading_from_name("Heading 7"), None);
        assert_eq!(heading_from_name("Heading Widget"), None);
        assert_eq!(heading_from_name("Subheading 1"), None);
        assert_eq!(heading_of(&ppr(""), None), None);
    }

    // ── table properties ─────────────────────────────────────────────────────

    fn tcpr(body: &str) -> CellProps {
        let src = format!("<w:tcPr {}>{}</w:tcPr>", NS, body);
        let doc = xml::parse(&src).expect("fixture parses");
        parse_cell_props(doc.root_element(), &theme())
    }

    #[test]
    fn a_width_reads_under_its_own_type() {
        let w = |body: &str| {
            let src = format!("<w:tblW {} {}/>", NS, body);
            let doc = xml::parse(&src).expect("fixture parses");
            width(doc.root_element())
        };
        assert_eq!(w(r#"w:w="1440" w:type="dxa""#), Some(Width::Dxa(1440)));
        // The schema's default type is dxa.
        assert_eq!(w(r#"w:w="720""#), Some(Width::Dxa(720)));
        // Fiftieths of a percent, and the strict schema's percentage string.
        assert_eq!(w(r#"w:w="2500" w:type="pct""#), Some(Width::Pct(50.0)));
        assert_eq!(w(r#"w:w="50%" w:type="pct""#), Some(Width::Pct(50.0)));
        assert_eq!(w(r#"w:type="auto""#), Some(Width::Auto));
        assert_eq!(w(r#"w:w="1440" w:type="nil""#), Some(Width::Nil));
        assert_eq!(w(r#"w:w="café""#), None);
        // Absurd lengths are clamped rather than pushing the page open.
        assert_eq!(w(r#"w:w="99999999""#), Some(Width::Dxa(MAX_DXA)));
    }

    #[test]
    fn a_vmerge_without_a_val_is_the_continuation() {
        // The trap: every other on/off element here means "on" when bare.
        assert_eq!(tcpr("<w:vMerge/>").v_merge, Some(VMerge::Continue));
        assert_eq!(
            tcpr(r#"<w:vMerge w:val="continue"/>"#).v_merge,
            Some(VMerge::Continue)
        );
        assert_eq!(
            tcpr(r#"<w:vMerge w:val="restart"/>"#).v_merge,
            Some(VMerge::Restart)
        );
        // `ST_Merge` has no other member, so anything else is not a merge at all.
        assert_eq!(tcpr(r#"<w:vMerge w:val="café"/>"#).v_merge, None);
        assert_eq!(tcpr("").v_merge, None);
        // A span is clamped to the grid's own ceiling.
        assert_eq!(tcpr(r#"<w:gridSpan w:val="3"/>"#).grid_span, Some(3));
        assert_eq!(
            tcpr(r#"<w:gridSpan w:val="9999"/>"#).grid_span,
            Some(MAX_GRID_COLS)
        );
    }

    #[test]
    fn cell_properties_normalise_onto_the_shared_vocabulary() {
        assert_eq!(tcpr(r#"<w:vAlign w:val="center"/>"#).v_align, Some("center"));
        // docx spells the justified case `both`.
        assert_eq!(tcpr(r#"<w:vAlign w:val="both"/>"#).v_align, Some("justify"));
        assert_eq!(tcpr(r#"<w:vAlign w:val="café"/>"#).v_align, None);
        assert_eq!(
            tcpr(r#"<w:textDirection w:val="btLr"/>"#).text_direction,
            Some(Rotation::Ccw(90.0))
        );
        assert_eq!(
            tcpr(r#"<w:textDirection w:val="tbRl"/>"#).text_direction,
            Some(Rotation::Ccw(-90.0))
        );
        // The default flow is not a rotation.
        assert_eq!(tcpr(r#"<w:textDirection w:val="lrTb"/>"#).text_direction, None);
        assert_eq!(tcpr("<w:noWrap/>").no_wrap, Some(true));
        // A margin of a stated zero is a zero, not an inherit; a percentage one is
        // not a length Word writes here.
        let m = tcpr(
            r#"<w:tcMar><w:top w:w="120" w:type="dxa"/><w:left w:w="0" w:type="nil"/>
               <w:right w:w="50" w:type="pct"/></w:tcMar>"#,
        )
        .mar;
        assert_eq!((m.top, m.left, m.right, m.bottom), (Some(120), Some(0), None, None));
    }

    #[test]
    fn a_table_style_chain_carries_borders_row_and_cell_properties() {
        let s = styles(
            r#"<w:style w:type="table" w:styleId="Base"><w:name w:val="Base"/>
                 <w:tblPr><w:tblBorders>
                   <w:top w:val="single" w:sz="8"/><w:insideH w:val="dotted" w:sz="4"/>
                 </w:tblBorders>
                 <w:tblCellMar><w:left w:w="80" w:type="dxa"/></w:tblCellMar></w:tblPr>
                 <w:trPr><w:trHeight w:val="360" w:hRule="atLeast"/></w:trPr>
                 <w:tcPr><w:vAlign w:val="center"/></w:tcPr></w:style>
               <w:style w:type="table" w:styleId="Leaf"><w:name w:val="Leaf"/>
                 <w:basedOn w:val="Base"/>
                 <w:tblPr><w:tblBorders><w:top w:val="double" w:sz="24"/></w:tblBorders></w:tblPr>
                 <w:tcPr><w:shd w:val="clear" w:fill="F2F2F2"/></w:tcPr></w:style>
               <w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/>
                 <w:pPr><w:jc w:val="center"/></w:pPr></w:style>"#,
        );
        let t = s.resolve_table("Leaf");
        // The leaf restates only the outer top edge; the parent keeps the interior
        // line, the cell margin, the row height and the cell alignment.
        assert_eq!(t.table.borders.sides.top.unwrap().val, "double");
        assert_eq!(t.table.borders.inside_h.unwrap().val, "dotted");
        assert_eq!(t.table.cell_mar.left, Some(80));
        assert_eq!(t.row.height_dxa, Some(360));
        assert_eq!(t.row.height_rule, Some(HRule::AtLeast));
        assert_eq!(t.cell.v_align, Some("center"));
        assert_eq!(t.cell.shade, Some(ColorVal::Set(Color::from_rgb(0xF2F2F2))));
        // The memo hands back the same resolution, and a paragraph style resolved
        // as a table one contributes nothing rather than being an error.
        assert!(Rc::ptr_eq(&t, &s.resolve_table("Leaf")));
        assert_eq!(s.resolve_table("Normal").table, TableProps::default());
        assert_eq!(s.resolve_table("café").cell, CellProps::default());
    }

    #[test]
    fn the_paragraph_mark_and_the_remaining_flags_survive_a_merge() {
        let p = ppr(
            r#"<w:pStyle w:val="Quote"/><w:jc w:val="both"/><w:bidi/>
               <w:contextualSpacing/><w:numPr><w:numId w:val="3"/><w:ilvl w:val="2"/></w:numPr>
               <w:rPr><w:sz w:val="36"/></w:rPr>"#,
        );
        assert_eq!(p.p_style.as_deref(), Some("Quote"));
        assert_eq!(p.align, Some(Align::Justify));
        assert_eq!(p.contextual_spacing, Some(true));
        assert_eq!((p.num_id, p.ilvl), (Some(3), Some(2)));
        // The mark's own size is what an empty paragraph is measured at.
        assert_eq!(size_pt(&p.mark), 18.0);
        assert!(to_para(&p, size_pt(&p.mark), None).rtl);
        // A deeper level than the format defines is clamped, not dropped.
        assert_eq!(ppr(r#"<w:numPr><w:ilvl w:val="99"/></w:numPr>"#).ilvl, Some(8));
        // The mark merges as run properties do, field by field.
        let mut m = p.clone();
        merge_para(&mut m, &ppr(r#"<w:rPr><w:b/></w:rPr>"#));
        assert_eq!((m.mark.sz_half, m.mark.bold), (Some(36), Some(true)));
        // An unstated field in the stronger link changes nothing.
        merge_para(&mut m, &ppr(""));
        assert_eq!(m.align, Some(Align::Justify));
        assert_eq!(m.p_style.as_deref(), Some("Quote"));
    }
}
