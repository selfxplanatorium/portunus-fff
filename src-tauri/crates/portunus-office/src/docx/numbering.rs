//! `word/numbering.xml`: list definitions and the counter state a walk advances.
//!
//! Two indirections sit between a paragraph and its marker: `w:numPr/w:numId`
//! names a `w:num`, which points at a `w:abstractNum` and may override
//! individual levels of it. Nothing is flattened at parse time because a
//! `w:lvlOverride` patches only the parts it states — a `w:startOverride`
//! without a replacement `w:lvl` yields a level that exists in no single place
//! in the file, which is why [`Numbering::level`] hands back an owned copy.
//!
//! The counters live here too, and that is the load-bearing part: Word numbering
//! is *stateful*. `%1.%2` renders from wherever the walk has got to, so the same
//! level of the same list gives a different label on every paragraph and the only
//! correct order to ask in is document order. One `Numbering` therefore serves
//! exactly one forward pass over the body; [`Numbering::reset`] starts another.
//!
//! Deliberately independent of the style pass: markers come back as raw OOXML
//! values (font name, half-points, hex string) rather than a rendered
//! `ListMarker`, so this module needs no `model` import and can be tested on
//! nothing but XML strings.

use super::super::fonts;
use super::super::listnum::{alpha, decimal, roman};
use super::super::xml::{self, attr_bool, attr_i64, attr_local, attr_u32, child, elems};
use std::collections::HashMap;

/// `w:ilvl` is 0..=8 in the schema, and the `%1`..`%9` placeholders can address
/// no more than this many either.
pub const LEVELS: usize = 9;

// Every table below is document-controlled and unbounded; each gets a cap so a
// hostile part cannot make numbering the expensive part of a preview. Real
// documents are orders of magnitude below all of these.
const MAX_ABSTRACT: usize = 1024;
const MAX_NUMS: usize = 2048;
const MAX_LVL_TEXT: usize = 120;
const MAX_FONT_NAME: usize = 64;
const MAX_COLOR: usize = 8;
/// A counter saturates here rather than wrapping. Roman numerals already
/// saturate at 3999 and `alpha` grows logarithmically, so this only bounds the
/// decimal spellings.
const MAX_COUNTER: u32 = 1_000_000;
/// `w:sz` is in half-points; 1638 is Word's own ceiling.
const MAX_HALF_POINTS: u32 = 1638;

/// `w:numFmt@w:val`. Word defines some three dozen spellings (`cardinalText`,
/// `chicago`, `chineseCounting`, …); everything outside this set falls back to
/// decimal, which keeps the item numbered instead of dropping the marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumFmt {
    Decimal,
    DecimalZero,
    UpperRoman,
    LowerRoman,
    UpperLetter,
    LowerLetter,
    Ordinal,
    Bullet,
    None,
}

/// `w:suff@w:val` — what separates the marker from the paragraph text. Absent
/// means `tab`, which is what Word applies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Suffix {
    Tab,
    Space,
    Nothing,
}

/// `w:lvlJc@w:val`: how the marker sits in the space reserved for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Jc {
    Left,
    Center,
    Right,
}

/// The level's `w:pPr/w:ind`, in raw twentieths of a point. Kept unconverted: the
/// caller merges these with the paragraph's and the style's own indents before
/// anything becomes px, and rounding at three places instead of one drifts.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Indent {
    /// `w:left`, or its newer spelling `w:start`.
    pub left: Option<i64>,
    pub hanging: Option<i64>,
    pub first_line: Option<i64>,
}

/// The level's `w:rPr`, as the marker's own formatting. Raw values only — a font
/// name as authored (so the caller can still ask `fonts::is_symbol_font` about
/// it), a size in half-points, a colour as the six hex digits Word wrote.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct MarkerFmt {
    /// `w:rFonts@w:ascii`, unmapped.
    pub font: Option<String>,
    /// `w:sz@w:val`, half-points.
    pub half_points: Option<u32>,
    /// `w:color@w:val`: `RRGGBB`, or the literal `auto`, which is passed through
    /// rather than guessed at here.
    pub color: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

/// One `w:lvl`, with `w:startOverride` already folded into `start`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Level {
    pub start: u32,
    pub fmt: NumFmt,
    /// `w:lvlText@w:val`: the `%1`..`%9` template for a numbered level, the
    /// glyph itself for a bullet. Empty when the level states none — Word treats
    /// the element as required, so an invented `%N.` would be fabrication.
    pub lvl_text: String,
    pub jc: Option<Jc>,
    /// `w:lvlRestart@w:val`. `Some(0)` means this level never restarts.
    pub restart: Option<u32>,
    /// `w:isLgl`: render every placeholder as decimal whatever the referenced
    /// level's own format says (legal numbering, `1.1.1`).
    pub is_lgl: bool,
    pub suffix: Suffix,
    pub indent: Indent,
    pub marker: MarkerFmt,
}

/// What one list paragraph shows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Marker {
    /// The substituted `lvlText`, the remapped bullet glyph, or empty for
    /// `w:numFmt="none"` — which still carries indents, so it is a `Marker` with
    /// no text rather than `None`.
    pub text: String,
    pub bullet: bool,
    pub suffix: Suffix,
    pub jc: Option<Jc>,
    pub indent: Indent,
    pub fmt: MarkerFmt,
}

// The counter arithmetic and the %n substitution need these three fields and
// nothing else, so they can be snapshotted for all nine levels without cloning
// any level's strings.
#[derive(Clone, Copy)]
struct Spec {
    start: u32,
    fmt: NumFmt,
    restart: Option<u32>,
}

struct Abstract {
    levels: Vec<Option<Level>>,
}

struct Override {
    start: Option<u32>,
    lvl: Option<Level>,
}

struct Num {
    abstract_id: u32,
    overrides: Vec<Option<Override>>,
}

pub struct Numbering {
    abstracts: HashMap<u32, Abstract>,
    nums: HashMap<u32, Num>,
    /// Per `w:numId`, not per abstract: two `w:num` entries sharing one
    /// `w:abstractNum` are two independent lists in Word, and that is how
    /// documents get "restart numbering" without touching the definition.
    /// `None` at a level means "not yet started", which is distinct from zero —
    /// the first use renders `start`, not `start + 1`.
    counters: HashMap<u32, [Option<u32>; LEVELS]>,
}

impl Numbering {
    /// A document with no numbering part, or one that would not parse: every
    /// lookup returns `None`. Numbering is decoration on top of text, so a
    /// broken part must cost the document its list markers and nothing else.
    pub fn empty() -> Numbering {
        Numbering {
            abstracts: HashMap::new(),
            nums: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    pub fn parse(numbering_xml: &str) -> Numbering {
        let Ok(doc) = xml::parse(numbering_xml) else {
            return Numbering::empty();
        };
        let root = doc.root_element();
        let mut out = Numbering::empty();

        for n in elems(root)
            .filter(|c| c.tag_name().name() == "abstractNum")
            .take(MAX_ABSTRACT)
        {
            let Some(id) = attr_u32(n, "abstractNumId") else {
                continue;
            };
            let mut levels: Vec<Option<Level>> = (0..LEVELS).map(|_| None).collect();
            for l in elems(n).filter(|c| c.tag_name().name() == "lvl") {
                if let Some((ilvl, lvl)) = parse_level(l) {
                    levels[ilvl] = Some(lvl);
                }
            }
            out.abstracts.insert(id, Abstract { levels });
        }

        for n in elems(root)
            .filter(|c| c.tag_name().name() == "num")
            .take(MAX_NUMS)
        {
            let Some(num_id) = attr_u32(n, "numId") else {
                continue;
            };
            // A `w:num` with no `w:abstractNumId` names no list; skipping it
            // makes the lookup return `None` rather than an empty marker.
            let Some(abstract_id) = child(n, "abstractNumId").and_then(|a| attr_u32(a, "val"))
            else {
                continue;
            };
            let mut overrides: Vec<Option<Override>> = (0..LEVELS).map(|_| None).collect();
            for o in elems(n).filter(|c| c.tag_name().name() == "lvlOverride") {
                let ilvl = attr_u32(o, "ilvl").unwrap_or(0) as usize;
                if ilvl >= LEVELS {
                    continue;
                }
                let start = child(o, "startOverride")
                    .and_then(|s| attr_u32(s, "val"))
                    .map(|v| v.min(MAX_COUNTER));
                let lvl = child(o, "lvl").and_then(parse_level).map(|(_, l)| l);
                overrides[ilvl] = Some(Override { start, lvl });
            }
            out.nums.insert(
                num_id,
                Num {
                    abstract_id,
                    overrides,
                },
            );
        }

        out
    }

    /// The definition in force for `(num_id, ilvl)`. Owned because
    /// `w:startOverride` patches the abstract level's `start`, so the answer is
    /// not any one node of the file.
    pub fn level(&self, num_id: u32, ilvl: usize) -> Option<Level> {
        let (lvl, start) = self.parts(num_id, ilvl)?;
        let mut out = lvl.clone();
        if let Some(s) = start {
            out.start = s;
        }
        Some(out)
    }

    /// Advance `ilvl` of `num_id` by one and render its marker. This mutates:
    /// call it once per list paragraph, in document order, and never for a
    /// paragraph whose marker is not actually drawn — a skipped call is a
    /// skipped number.
    pub fn label(&mut self, num_id: u32, ilvl: usize) -> Option<Marker> {
        let ilvl = ilvl.min(LEVELS - 1);
        let lvl = self.level(num_id, ilvl)?;

        let mut specs: [Option<Spec>; LEVELS] = [None; LEVELS];
        for (i, s) in specs.iter_mut().enumerate() {
            *s = self.spec(num_id, i);
        }

        let counters = self.counters.entry(num_id).or_insert([None; LEVELS]);
        let start = specs[ilvl].map(|s| s.start).unwrap_or(1);
        counters[ilvl] = Some(match counters[ilvl] {
            Some(n) => n.saturating_add(1).min(MAX_COUNTER),
            None => start,
        });
        for (d, c) in counters.iter_mut().enumerate().skip(ilvl + 1) {
            // `w:lvlRestart` is stated by the level being reset, not by the one
            // being advanced: absent means "restart whenever anything shallower
            // is used", 0 means never, and `k` restricts it to levels at or
            // above the k-th (1-based).
            let restarts = match specs[d].and_then(|s| s.restart) {
                Some(0) => false,
                Some(k) => ilvl as u32 + 1 <= k,
                None => true,
            };
            if restarts {
                *c = None;
            }
        }
        // A parent level the walk never visited still renders in `%1.%2` — Word
        // shows its start value there rather than dropping the segment.
        let mut values = [0u32; LEVELS];
        for (i, v) in values.iter_mut().enumerate() {
            *v = counters[i].unwrap_or_else(|| specs[i].map(|s| s.start).unwrap_or(1));
        }

        let text = match lvl.fmt {
            NumFmt::Bullet => bullet_glyph(&lvl.lvl_text, lvl.marker.font.as_deref()),
            NumFmt::None => String::new(),
            _ => substitute(&lvl.lvl_text, ilvl, &values, &specs, lvl.is_lgl),
        };
        Some(Marker {
            text,
            bullet: lvl.fmt == NumFmt::Bullet,
            suffix: lvl.suffix,
            jc: lvl.jc,
            indent: lvl.indent,
            fmt: lvl.marker,
        })
    }

    /// Forget every counter, so a second walk over the same body numbers as the
    /// first did. A render builds its own `Numbering` and walks once, so nothing
    /// outside the tests needs this — it stays `cfg(test)` rather than becoming
    /// dead public API that implies re-walking is supported.
    #[cfg(test)]
    pub fn reset(&mut self) {
        self.counters.clear();
    }

    // The level node in force plus any `w:startOverride`, borrowed. Split out
    // from `level` so the hot path can read `start`/`fmt`/`restart` for all nine
    // levels without cloning nine `lvlText`s per paragraph.
    fn parts(&self, num_id: u32, ilvl: usize) -> Option<(&Level, Option<u32>)> {
        if ilvl >= LEVELS {
            return None;
        }
        let num = self.nums.get(&num_id)?;
        let ov = num.overrides.get(ilvl).and_then(|o| o.as_ref());
        let start = ov.and_then(|o| o.start);
        // A full replacement `w:lvl` shadows the abstract level entirely, and
        // still yields to `w:startOverride` on the same `w:lvlOverride`.
        if let Some(l) = ov.and_then(|o| o.lvl.as_ref()) {
            return Some((l, start));
        }
        let abs = self.abstracts.get(&num.abstract_id)?;
        let l = abs.levels.get(ilvl).and_then(|l| l.as_ref())?;
        Some((l, start))
    }

    fn spec(&self, num_id: u32, ilvl: usize) -> Option<Spec> {
        let (lvl, start) = self.parts(num_id, ilvl)?;
        Some(Spec {
            start: start.unwrap_or(lvl.start),
            fmt: lvl.fmt,
            restart: lvl.restart,
        })
    }
}

/// `(ilvl, level)`; `None` when the level is outside the nine the schema allows.
fn parse_level(n: roxmltree::Node) -> Option<(usize, Level)> {
    let ilvl = attr_u32(n, "ilvl").unwrap_or(0) as usize;
    if ilvl >= LEVELS {
        return None;
    }
    let fmt = child(n, "numFmt")
        .and_then(|f| attr_local(f, "val"))
        .map(num_fmt)
        .unwrap_or(NumFmt::Decimal);
    let lvl_text: String = child(n, "lvlText")
        .and_then(|t| attr_local(t, "val"))
        .unwrap_or("")
        .chars()
        .take(MAX_LVL_TEXT)
        .collect();
    let ind = child(n, "pPr").and_then(|p| child(p, "ind"));
    Some((
        ilvl,
        Level {
            start: child(n, "start")
                .and_then(|s| attr_u32(s, "val"))
                .unwrap_or(1)
                .min(MAX_COUNTER),
            fmt,
            lvl_text,
            jc: child(n, "lvlJc").and_then(|j| attr_local(j, "val")).map(jc),
            restart: child(n, "lvlRestart").and_then(|r| attr_u32(r, "val")),
            is_lgl: on_off(n, "isLgl"),
            suffix: child(n, "suff")
                .and_then(|s| attr_local(s, "val"))
                .map(suffix)
                .unwrap_or(Suffix::Tab),
            indent: Indent {
                // `w:start` is the newer spelling of `w:left`; producers emit
                // one or the other, never both.
                left: ind
                    .and_then(|i| attr_i64(i, "left").or_else(|| attr_i64(i, "start"))),
                hanging: ind.and_then(|i| attr_i64(i, "hanging")),
                first_line: ind.and_then(|i| attr_i64(i, "firstLine")),
            },
            marker: child(n, "rPr").map(marker_fmt).unwrap_or_default(),
        },
    ))
}

fn marker_fmt(rpr: roxmltree::Node) -> MarkerFmt {
    MarkerFmt {
        font: child(rpr, "rFonts")
            .and_then(|f| attr_local(f, "ascii"))
            .map(|v| v.chars().take(MAX_FONT_NAME).collect()),
        half_points: child(rpr, "sz")
            .and_then(|s| attr_u32(s, "val"))
            .map(|v| v.min(MAX_HALF_POINTS)),
        color: child(rpr, "color")
            .and_then(|c| attr_local(c, "val"))
            .map(|v| v.trim_start_matches('#').chars().take(MAX_COLOR).collect()),
        bold: on_off_opt(rpr, "b"),
        italic: on_off_opt(rpr, "i"),
    }
}

/// An OOXML on/off element: present with no `w:val` means on.
fn on_off_opt(parent: roxmltree::Node, local: &str) -> Option<bool> {
    child(parent, local).map(|e| attr_bool(e, "val").unwrap_or(true))
}

fn on_off(parent: roxmltree::Node, local: &str) -> bool {
    on_off_opt(parent, local).unwrap_or(false)
}

fn num_fmt(v: &str) -> NumFmt {
    match v.trim() {
        "decimal" => NumFmt::Decimal,
        "decimalZero" => NumFmt::DecimalZero,
        "upperRoman" => NumFmt::UpperRoman,
        "lowerRoman" => NumFmt::LowerRoman,
        "upperLetter" => NumFmt::UpperLetter,
        "lowerLetter" => NumFmt::LowerLetter,
        "ordinal" => NumFmt::Ordinal,
        "bullet" => NumFmt::Bullet,
        "none" => NumFmt::None,
        _ => NumFmt::Decimal,
    }
}

fn suffix(v: &str) -> Suffix {
    match v.trim() {
        "space" => Suffix::Space,
        "nothing" => Suffix::Nothing,
        _ => Suffix::Tab,
    }
}

fn jc(v: &str) -> Jc {
    match v.trim() {
        "center" => Jc::Center,
        // `end` is the newer spelling of `right`, `start` of `left`.
        "right" | "end" => Jc::Right,
        _ => Jc::Left,
    }
}

/// The bullet glyph. Word authors these as Symbol/Wingdings code points, which
/// draw as tofu in any substitute font, so they are folded onto real Unicode
/// when the level names such a font; anything the tables do not cover keeps its
/// original character.
fn bullet_glyph(lvl_text: &str, font: Option<&str>) -> String {
    let raw = font.unwrap_or("");
    if !fonts::is_symbol_font(raw) {
        return lvl_text.to_string();
    }
    lvl_text
        .chars()
        .map(|c| fonts::remap(raw, c).unwrap_or(c))
        .collect()
}

fn substitute(
    lvl_text: &str,
    ilvl: usize,
    values: &[u32; LEVELS],
    specs: &[Option<Spec>; LEVELS],
    is_lgl: bool,
) -> String {
    let mut out = String::with_capacity(lvl_text.len() + 8);
    let mut it = lvl_text.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.peek().and_then(|d| d.to_digit(10)).filter(|d| *d >= 1) {
            Some(d) => {
                it.next();
                let i = d as usize - 1;
                // A placeholder deeper than the level being rendered has no
                // value yet; Word drops it rather than showing the start.
                if i > ilvl {
                    continue;
                }
                let fmt = if is_lgl {
                    NumFmt::Decimal
                } else {
                    specs[i].map(|s| s.fmt).unwrap_or(NumFmt::Decimal)
                };
                out.push_str(&render_num(fmt, values[i]));
            }
            // `%0` and a trailing `%` are literal text.
            None => out.push('%'),
        }
    }
    out
}

fn render_num(fmt: NumFmt, n: u32) -> String {
    match fmt {
        NumFmt::Decimal => decimal(n),
        NumFmt::DecimalZero => format!("{n:02}"),
        NumFmt::UpperRoman => roman(n, true),
        NumFmt::LowerRoman => roman(n, false),
        NumFmt::UpperLetter => alpha(n, true),
        NumFmt::LowerLetter => alpha(n, false),
        NumFmt::Ordinal => ordinal(n),
        // A bullet or `none` level referenced through a `%n` contributes no
        // numeral — there is nothing to spell.
        NumFmt::Bullet | NumFmt::None => String::new(),
    }
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    fn wrap(body: &str) -> String {
        format!("<w:numbering xmlns:w=\"{W}\">{body}</w:numbering>")
    }

    /// Decimal / decimal / lowerLetter, the shape Word's "1.1.1" gallery entry
    /// produces, plus one `w:num` pointing at it.
    fn three_levels() -> String {
        wrap(
            r#"<w:abstractNum w:abstractNumId="3">
                 <w:lvl w:ilvl="0">
                   <w:start w:val="1"/><w:numFmt w:val="decimal"/>
                   <w:lvlText w:val="%1."/><w:lvlJc w:val="left"/>
                   <w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr>
                 </w:lvl>
                 <w:lvl w:ilvl="1">
                   <w:start w:val="1"/><w:numFmt w:val="decimal"/>
                   <w:lvlText w:val="%1.%2."/>
                   <w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr>
                 </w:lvl>
                 <w:lvl w:ilvl="2">
                   <w:start w:val="1"/><w:numFmt w:val="lowerLetter"/>
                   <w:lvlText w:val="%1.%2.%3"/><w:suff w:val="space"/>
                 </w:lvl>
               </w:abstractNum>
               <w:num w:numId="7"><w:abstractNumId w:val="3"/></w:num>"#,
        )
    }

    fn text(n: &mut Numbering, num_id: u32, ilvl: usize) -> String {
        n.label(num_id, ilvl).expect("level is defined").text
    }

    #[test]
    fn three_level_definition_numbers_nested_labels() {
        let mut n = Numbering::parse(&three_levels());
        assert_eq!(text(&mut n, 7, 0), "1.");
        assert_eq!(text(&mut n, 7, 1), "1.1.");
        assert_eq!(text(&mut n, 7, 2), "1.1.a");
        // The level's own indents and suffix ride along with the marker.
        let m = n.label(7, 0).expect("level 0");
        assert_eq!(m.text, "2.");
        assert_eq!(m.indent.left, Some(720));
        assert_eq!(m.indent.hanging, Some(360));
        assert_eq!(m.suffix, Suffix::Tab);
        assert_eq!(m.jc, Some(Jc::Left));
        assert!(!m.bullet);
        assert_eq!(n.level(7, 2).expect("level 2").suffix, Suffix::Space);
    }

    #[test]
    fn deeper_levels_restart_when_a_shallower_one_advances() {
        let mut n = Numbering::parse(&three_levels());
        assert_eq!(text(&mut n, 7, 0), "1.");
        assert_eq!(text(&mut n, 7, 1), "1.1.");
        assert_eq!(text(&mut n, 7, 1), "1.2.");
        assert_eq!(text(&mut n, 7, 2), "1.2.a");
        assert_eq!(text(&mut n, 7, 2), "1.2.b");
        assert_eq!(text(&mut n, 7, 0), "2.");
        // Both deeper counters went back to "not yet started".
        assert_eq!(text(&mut n, 7, 1), "2.1.");
        assert_eq!(text(&mut n, 7, 2), "2.1.a");
        // A fresh walk starts over.
        n.reset();
        assert_eq!(text(&mut n, 7, 0), "1.");
    }

    #[test]
    fn lvl_restart_zero_keeps_the_deeper_counter_running() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
                 <w:lvl w:ilvl="1">
                   <w:numFmt w:val="decimal"/><w:lvlText w:val="%1.%2."/>
                   <w:lvlRestart w:val="0"/>
                 </w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(text(&mut n, 1, 0), "1.");
        assert_eq!(text(&mut n, 1, 1), "1.1.");
        assert_eq!(text(&mut n, 1, 1), "1.2.");
        assert_eq!(text(&mut n, 1, 0), "2.");
        assert_eq!(text(&mut n, 1, 1), "2.3.");
    }

    #[test]
    fn start_override_replaces_the_abstract_start() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/>
                   <w:lvlText w:val="%1."/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
               <w:num w:numId="2">
                 <w:abstractNumId w:val="0"/>
                 <w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride>
               </w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(n.level(2, 0).expect("level").start, 5);
        assert_eq!(text(&mut n, 2, 0), "5.");
        assert_eq!(text(&mut n, 2, 0), "6.");
        // The two numIds share an abstract definition but count separately.
        assert_eq!(text(&mut n, 1, 0), "1.");
    }

    #[test]
    fn lvl_override_replaces_the_whole_level() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1">
                 <w:abstractNumId w:val="0"/>
                 <w:lvlOverride w:ilvl="0">
                   <w:lvl w:ilvl="0">
                     <w:start w:val="2"/><w:numFmt w:val="upperRoman"/>
                     <w:lvlText w:val="(%1)"/><w:suff w:val="nothing"/>
                   </w:lvl>
                 </w:lvlOverride>
               </w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(text(&mut n, 1, 0), "(II)");
        assert_eq!(n.level(1, 0).expect("level").suffix, Suffix::Nothing);
    }

    #[test]
    fn is_lgl_renders_every_placeholder_as_decimal() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:numFmt w:val="upperRoman"/><w:lvlText w:val="%1."/></w:lvl>
                 <w:lvl w:ilvl="1">
                   <w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1.%2."/>
                   <w:isLgl/>
                 </w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(text(&mut n, 1, 0), "I.");
        assert_eq!(text(&mut n, 1, 0), "II.");
        // Level 0 stays roman for its own label; under isLgl both segments of
        // the deeper label go decimal.
        assert_eq!(text(&mut n, 1, 1), "2.1.");
    }

    #[test]
    fn symbol_font_bullet_is_remapped_to_unicode() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0">
                   <w:numFmt w:val="bullet"/><w:lvlText w:val="&#xF0B7;"/>
                   <w:rPr>
                     <w:rFonts w:ascii="Symbol"/><w:sz w:val="24"/>
                     <w:color w:val="FF0000"/><w:b/>
                   </w:rPr>
                 </w:lvl>
                 <w:lvl w:ilvl="1">
                   <w:numFmt w:val="bullet"/><w:lvlText w:val="&#x25CB;"/>
                   <w:rPr><w:rFonts w:ascii="Courier New"/></w:rPr>
                 </w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        let m = n.label(1, 0).expect("bullet level");
        assert_eq!(m.text, "•");
        assert!(m.bullet);
        assert_eq!(m.fmt.font.as_deref(), Some("Symbol"));
        assert_eq!(m.fmt.half_points, Some(24));
        assert_eq!(m.fmt.color.as_deref(), Some("FF0000"));
        assert_eq!(m.fmt.bold, Some(true));
        assert_eq!(m.fmt.italic, None);
        // A non-symbol font leaves the glyph alone.
        assert_eq!(n.label(1, 1).expect("bullet level").text, "○");
    }

    #[test]
    fn unknown_formats_fall_back_to_decimal_and_none_yields_no_text() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:numFmt w:val="chineseCounting"/>
                   <w:lvlText w:val="%1."/></w:lvl>
                 <w:lvl w:ilvl="1"><w:numFmt w:val="none"/><w:lvlText w:val="%1.%2."/></w:lvl>
                 <w:lvl w:ilvl="2"><w:numFmt w:val="decimalZero"/><w:lvlText w:val="%3"/></w:lvl>
                 <w:lvl w:ilvl="3"><w:numFmt w:val="ordinal"/><w:lvlText w:val="%4 Widget"/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(text(&mut n, 1, 0), "1.");
        let m = n.label(1, 1).expect("none level");
        assert!(m.text.is_empty());
        assert!(!m.bullet);
        assert_eq!(text(&mut n, 1, 2), "01");
        assert_eq!(text(&mut n, 1, 3), "1st Widget");
        assert_eq!(text(&mut n, 1, 3), "2nd Widget");
    }

    #[test]
    fn placeholders_are_bounded_by_the_level_and_by_the_digits() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/>
                   <w:lvlText w:val="%1.%2.%0 100%"/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        // `%2` is deeper than level 0 and disappears; `%0` and a bare `%` are
        // literal text.
        assert_eq!(text(&mut n, 1, 0), "1..%0 100%");
    }

    #[test]
    fn a_level_entered_before_its_parent_shows_the_parent_start() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:start w:val="4"/><w:numFmt w:val="decimal"/>
                   <w:lvlText w:val="%1."/></w:lvl>
                 <w:lvl w:ilvl="1"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1.%2."/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert_eq!(text(&mut n, 1, 1), "4.1.");
    }

    #[test]
    fn malformed_and_missing_definitions_degrade_to_none() {
        // Unclosed element.
        let mut broken = Numbering::parse("<w:numbering><w:num></w:numbering>");
        assert!(broken.label(1, 0).is_none());
        assert!(broken.level(1, 0).is_none());
        // A DTD is rejected outright by the shared parser (XXE), which must not
        // become an error the document render propagates.
        let mut dtd = Numbering::parse(
            r#"<!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]><numbering>&e;</numbering>"#,
        );
        assert!(dtd.label(1, 0).is_none());
        // Well-formed but empty, and lookups outside what exists.
        let mut n = Numbering::parse(&three_levels());
        assert!(n.label(99, 0).is_none());
        assert!(n.level(7, 8).is_none());
        // Beyond the nine levels the label clamps rather than panicking.
        assert!(n.label(7, 40).is_none());
        assert!(Numbering::empty().level(0, 0).is_none());
    }

    #[test]
    fn a_num_without_an_abstract_reference_is_not_a_list() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:lvlText w:val="%1."/></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"/>
               <w:num w:numId="2"><w:abstractNumId w:val="77"/></w:num>"#,
        );
        let mut n = Numbering::parse(&xml);
        assert!(n.label(1, 0).is_none());
        assert!(n.label(2, 0).is_none());
    }

    #[test]
    fn indent_accepts_the_newer_start_spelling() {
        let xml = wrap(
            r#"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:lvlText w:val="%1."/>
                   <w:pPr><w:ind w:start="425" w:firstLine="-283"/></w:pPr></w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        );
        let n = Numbering::parse(&xml);
        let lvl = n.level(1, 0).expect("level");
        assert_eq!(lvl.indent.left, Some(425));
        assert_eq!(lvl.indent.first_line, Some(-283));
        assert_eq!(lvl.indent.hanging, None);
    }

    #[test]
    fn document_controlled_sizes_are_capped() {
        let long = "%1".to_string() + &"café ".repeat(200);
        let xml = wrap(&format!(
            r##"<w:abstractNum w:abstractNumId="0">
                 <w:lvl w:ilvl="0"><w:start w:val="4294967295"/>
                   <w:numFmt w:val="decimal"/><w:lvlText w:val="{long}"/>
                   <w:rPr><w:rFonts w:ascii="{fontname}"/><w:sz w:val="99999"/>
                     <w:color w:val="#0000FF00000"/></w:rPr>
                 </w:lvl>
               </w:abstractNum>
               <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"##,
            long = long,
            fontname = "naïve".repeat(40),
        ));
        let n = Numbering::parse(&xml);
        let lvl = n.level(1, 0).expect("level");
        assert_eq!(lvl.lvl_text.chars().count(), MAX_LVL_TEXT);
        assert_eq!(lvl.start, MAX_COUNTER);
        assert_eq!(lvl.marker.half_points, Some(MAX_HALF_POINTS));
        assert_eq!(
            lvl.marker.font.as_ref().map(|f| f.chars().count()),
            Some(MAX_FONT_NAME)
        );
        // The leading `#` some producers write is stripped, the rest clamped.
        assert_eq!(lvl.marker.color.as_deref(), Some("0000FF00"));
    }

    #[test]
    fn numerals_saturate_rather_than_wrap() {
        assert_eq!(render_num(NumFmt::DecimalZero, 7), "07");
        assert_eq!(render_num(NumFmt::DecimalZero, 123), "123");
        assert_eq!(render_num(NumFmt::Bullet, 3), "");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(21), "21st");
        assert_eq!(ordinal(112), "112th");
        assert_eq!(ordinal(103), "103rd");
    }
}
