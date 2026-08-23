//! `number:*-style` trees resolved into `office::numfmt::Format`.
//!
//! ODF states a number format as a *tree* (`number:year`, `number:text`,
//! `number:currency-symbol`, …) where xlsx states it as a code string
//! (`yyyy-mm-dd`). The two describe the same thing, and `office::numfmt` already
//! implements the code side completely — value classes, date detection, the
//! `[Red]` modifier, the four-section split.
//!
//! So this module does not reimplement any of that: it **synthesizes a format
//! code** from the tree and hands it to [`Format::parse`]. That is also the only
//! seam available (`Format`'s sections are private), and it is the cheaper one to
//! trust — a bug here shows up as a wrong code string, which a test can read.
//!
//! ## What the tree maps onto
//!
//! - Children are walked **in document order**, so `number:currency-symbol`
//!   before or after `number:number` is symbol-before or symbol-after with no
//!   special case.
//! - `number:text` is arbitrary document content and gets escaped ([`Body::text`])
//!   so it cannot open a section, a colour bracket or a placeholder run. This is
//!   load-bearing: an unescaped `;` in a `number:text` would silently turn one
//!   format into two.
//! - `style:map` is ODF's conditional section. `numfmt` already has multi-section
//!   semantics (`pos;neg;zero`), so a map pair composes onto it — including the
//!   shape LibreOffice actually writes, where the style the cell names is the
//!   *negative* one and its `value()>=0` map points at the positive.
//! - `style:text-properties/fo:color` becomes a colour bracket, but only when the
//!   colour is exactly one of the eight Excel names `numfmt` knows. A near miss is
//!   dropped rather than snapped.
//!
//! ## What cannot be said in a format code
//!
//! Reported rather than faked:
//!
//! - `number:era`, `number:quarter`, `number:week-of-year`,
//!   `number:embedded-text`, `number:calendar` (non-Gregorian),
//!   `number:transliteration-*`, `number:display-factor`,
//!   `number:automatic-order` — no token exists, so they contribute nothing.
//! - A `number:minutes` with neither hours nor seconds beside it. The grammar
//!   disambiguates `m` from month by its neighbours (`numfmt::resolve_minutes`),
//!   so a minute in isolation is read as a month. Inherent to the code form;
//!   Excel has the same ambiguity.
//! - A `number:percentage-style` that states no `%` in its text. The ×100 and the
//!   printed sign are one token, so scaling without printing is not expressible;
//!   the value renders unscaled rather than growing a sign the document never
//!   wrote.
//! - A `style:condition` against anything but zero (`value()>100`). Excel's
//!   sections are fixed at positive / negative / zero.
//! - A mapped style's own maps: composition is one level deep, which is also what
//!   makes a cycle impossible.
//!
//! The corpus contains **zero** `number:*` styles — the sample workbook is on
//! locale defaults — so the tests below are the whole of this module's evidence,
//! and they format values through the produced [`Format`] rather than only
//! checking the code string.
//!
//! Nothing in here is reachable yet: the odt renderer needs no cell formats, so
//! the ODF sheet pass is this module's only consumer and the allow below stays
//! until it lands.
#![allow(dead_code)]

use std::collections::HashMap;

use super::super::numfmt::Format;
use super::super::xml::{self, attr_bool, attr_local, attr_u32, child, elems, inner_text};
use roxmltree::Node;

/// Number styles kept. Document-controlled, like every table in
/// [`super::style`].
const MAX_STYLES: usize = 1024;
/// Bytes per section. `numfmt::Format::parse` reads a code over 512 bytes as
/// General, so four sections plus their separators have to fit inside that —
/// hence a quarter of it, less a little.
const MAX_BODY: usize = 120;
/// `style:map`s read per style.
const MAX_MAPS: usize = 8;
/// Placeholders emitted for one `number:*` attribute. Well past any real format.
const MAX_DIGITS: u32 = 20;
const MAX_NAME: usize = 128;

// ── the section body ─────────────────────────────────────────────────────────

/// One `numfmt` section under construction.
///
/// Two ways in, and the distinction is the security boundary: [`Body::code`]
/// appends format-code text this module wrote itself, [`Body::text`] appends
/// document content and escapes it. Both stop at [`MAX_BODY`] on a token
/// boundary, never mid-escape — a truncation that split a `\"` or left a quoted
/// run open would change what every character after it means.
#[derive(Debug, Default)]
struct Body {
    s: String,
}

impl Body {
    fn new() -> Body {
        Body { s: String::new() }
    }

    fn room(&self, n: usize) -> bool {
        self.s.len() + n <= MAX_BODY
    }

    fn code(&mut self, s: &str) {
        if self.room(s.len()) {
            self.s.push_str(s);
        }
    }

    fn digits(&mut self, ch: char, n: u32) {
        for _ in 0..n.min(MAX_DIGITS) {
            if self.room(1) {
                self.s.push(ch);
            }
        }
    }

    /// Appends document text as literals.
    ///
    /// A quoted run copies verbatim up to the next quote, so the only character
    /// that cannot ride inside one is the quote itself; that one goes out
    /// backslash-escaped, which the tokenizer reads as exactly one literal
    /// character. Everything else — `;`, `[`, `0`, `\` — is inert inside quotes.
    ///
    /// `percent` is set only for a `number:percentage-style`, where a `%` is the
    /// live token that multiplies the value by 100 rather than a literal sign.
    fn text(&mut self, raw: &str, percent: bool) {
        let mut open = false;
        for ch in raw.chars() {
            let piece = if percent && ch == '%' {
                1
            } else if ch == '"' {
                2
            } else {
                ch.len_utf8() + usize::from(!open)
            };
            // Room for the character, plus the quote that would have to close a
            // run this character opens.
            if !self.room(piece + usize::from(open || ch != '"')) {
                break;
            }
            if (percent && ch == '%') || ch == '"' {
                if open {
                    self.s.push('"');
                    open = false;
                }
                if ch == '"' {
                    self.s.push_str("\\\"");
                } else {
                    self.s.push('%');
                }
                continue;
            }
            if !open {
                self.s.push('"');
                open = true;
            }
            self.s.push(ch);
        }
        if open {
            self.s.push('"');
        }
    }

    fn finish(self) -> String {
        self.s
    }
}

// ── styles ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Percentage,
    Currency,
    Date,
    Time,
    Boolean,
    Text,
}

fn kind_of(local: &str) -> Option<Kind> {
    Some(match local {
        "number-style" => Kind::Number,
        "percentage-style" => Kind::Percentage,
        "currency-style" => Kind::Currency,
        "date-style" => Kind::Date,
        "time-style" => Kind::Time,
        "boolean-style" => Kind::Boolean,
        "text-style" => Kind::Text,
        _ => return None,
    })
}

/// Which of Excel's sections a `style:condition` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cond {
    Positive,
    Negative,
    Zero,
}

#[derive(Debug, Clone)]
struct Map {
    cond: Cond,
    target: String,
}

/// One `number:*-style`, already reduced to section bodies. The XML is not kept:
/// a style is a handful of tokens, and holding the tree would tie every lookup to
/// the document's lifetime.
#[derive(Debug, Clone)]
struct Def {
    /// What this style itself renders as, colour bracket folded in.
    section: String,
    /// A negative spelling the style states on its own — only
    /// `number:boolean-style`, which is never negative and so repeats its `TRUE`
    /// to keep the zero section in Excel's third slot.
    neg: Option<String>,
    /// A zero spelling the style states on its own — `number:boolean-style`'s
    /// `FALSE`.
    zero: Option<String>,
    maps: Vec<Map>,
    /// A text style's section belongs in the fourth slot, the only one
    /// [`Format::apply_text`] reads.
    text: bool,
}

/// A document's `number:*-style` definitions.
pub struct Numbers {
    defs: HashMap<String, Def>,
}

impl Numbers {
    /// A document that defines no number styles, or whose parts would not parse.
    /// Every lookup yields `None`, and a caller then formats with whatever
    /// default it already had.
    pub fn empty() -> Numbers {
        Numbers {
            defs: HashMap::new(),
        }
    }

    /// Parses `styles.xml` (absent for a package that ships none) and
    /// `content.xml`, `content.xml` winning a name collision — the layering
    /// [`super::style::Styles::parse`] uses. Never fails.
    pub fn parse(styles_xml: Option<&str>, content_xml: &str) -> Numbers {
        let mut out = Numbers::empty();
        for src in [styles_xml, Some(content_xml)] {
            let Some(Ok(doc)) = src.map(xml::parse) else {
                continue;
            };
            out.load(doc.root_element());
        }
        out
    }

    fn load(&mut self, root: Node) {
        for container in elems(root)
            .filter(|e| matches!(e.tag_name().name(), "styles" | "automatic-styles"))
        {
            for e in elems(container) {
                if self.defs.len() >= MAX_STYLES {
                    return;
                }
                let Some(kind) = kind_of(e.tag_name().name()) else {
                    continue;
                };
                let Some(name) = name_of(e, "name") else {
                    continue;
                };
                self.defs.insert(name, build(e, kind));
            }
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    /// The format code `name` composes to, sections and all.
    ///
    /// Exposed beside [`Numbers::format`] because it is what a test can read and
    /// what a degradation note can quote; a renderer wants the parsed form.
    pub fn code(&self, name: &str) -> Option<String> {
        let d = self.defs.get(name)?;
        if d.text {
            // Only the fourth section applies to text, so the three value
            // sections ahead of it are General: a text style handed a number
            // renders it the way an unformatted cell does.
            return Some(format!("General;General;General;{}", d.section));
        }
        // LibreOffice writes the negative form as the style the cell names and
        // maps `value()>=0` to the positive one, so the base is whichever side
        // the maps did not claim.
        let (pos, base_is_neg) = match self.mapped(d, Cond::Positive) {
            Some(s) => (s, true),
            None => (d.section.clone(), false),
        };
        let neg = self
            .mapped(d, Cond::Negative)
            .or_else(|| base_is_neg.then(|| d.section.clone()))
            .or_else(|| d.neg.clone());
        let zero = self.mapped(d, Cond::Zero).or_else(|| d.zero.clone());
        Some(match (neg, zero) {
            (None, None) => pos,
            (Some(n), None) => format!("{pos};{n}"),
            // Excel reaches a zero section only through the third slot, so a
            // style that states one but no negative form gets the positive body
            // behind a minus — which is what a one-section code does implicitly.
            (None, Some(z)) => format!("{pos};-{pos};{z}"),
            (Some(n), Some(z)) => format!("{pos};{n};{z}"),
        })
    }

    /// `code`, parsed. `None` only when the document defines no such style, so a
    /// caller can tell "no style" from "a style that renders as General".
    pub fn format(&self, name: &str) -> Option<Format> {
        Some(Format::parse(&self.code(name)?))
    }

    // The body of the style a `style:map` points at. One level deep: a target's
    // own maps are not followed, which also makes a cycle impossible. A target
    // the document does not define, or one that is a text style, contributes
    // nothing and leaves the base in that slot.
    fn mapped(&self, d: &Def, cond: Cond) -> Option<String> {
        let name = &d.maps.iter().find(|m| m.cond == cond)?.target;
        let t = self.defs.get(name)?;
        (!t.text).then(|| t.section.clone())
    }
}

fn name_of(n: Node, attr: &str) -> Option<String> {
    let v = attr_local(n, attr)?.trim();
    (!v.is_empty()).then(|| v.chars().take(MAX_NAME).collect())
}

fn build(e: Node, kind: Kind) -> Def {
    let color = child(e, "text-properties")
        .and_then(|t| attr_local(t, "color"))
        .and_then(bracket_color);
    let boolean = kind == Kind::Boolean;
    Def {
        section: body(e, kind, color, "TRUE"),
        neg: boolean.then(|| body(e, kind, color, "TRUE")),
        zero: boolean.then(|| body(e, kind, color, "FALSE")),
        maps: elems(e)
            .filter(|c| c.tag_name().name() == "map")
            .take(MAX_MAPS)
            .filter_map(|m| {
                Some(Map {
                    cond: condition(attr_local(m, "condition")?)?,
                    target: name_of(m, "apply-style-name")?,
                })
            })
            .collect(),
        text: kind == Kind::Text,
    }
}

fn body(e: Node, kind: Kind, color: Option<&'static str>, boolean: &str) -> String {
    let mut b = Body::new();
    if let Some(c) = color {
        b.code(c);
    }
    let percent = kind == Kind::Percentage;
    for c in elems(e) {
        match c.tag_name().name() {
            "text" => b.text(&text_of(c), percent),
            // A `number:currency-symbol` with no content states a locale symbol
            // this module cannot know; it contributes nothing rather than a
            // guessed glyph.
            "currency-symbol" => b.text(&text_of(c), false),
            "number" => number(&mut b, c),
            "scientific-number" => scientific(&mut b, c),
            "fraction" => fraction(&mut b, c),
            "year" => b.code(if long(c) { "yyyy" } else { "yy" }),
            "month" => b.code(match (long(c), attr_bool(c, "textual").unwrap_or(false)) {
                (true, true) => "mmmm",
                (false, true) => "mmm",
                (true, false) => "mm",
                (false, false) => "m",
            }),
            "day" => b.code(if long(c) { "dd" } else { "d" }),
            // The weekday is the *four* and three wide `d`, which is why it does
            // not collide with the day of the month above.
            "day-of-week" => b.code(if long(c) { "dddd" } else { "ddd" }),
            "hours" => b.code(if long(c) { "hh" } else { "h" }),
            "minutes" => b.code(if long(c) { "mm" } else { "m" }),
            "seconds" => seconds(&mut b, c),
            "am-pm" => b.code("AM/PM"),
            "boolean" => b.text(boolean, false),
            "text-content" => b.code("@"),
            // `number:era`, `number:quarter`, `number:week-of-year`,
            // `number:embedded-text`: no token exists, and inventing a literal
            // would put text in the output the document never wrote.
            _ => {}
        }
    }
    b.finish()
}

fn number(b: &mut Body, n: Node) {
    let ints = digits(n, "min-integer-digits").unwrap_or(1);
    let grouping = attr_bool(n, "grouping").unwrap_or(false);
    // `#,##` is Excel's own spelling of the grouping trigger: a comma between
    // placeholders. The zeros after it are the minimum integer width.
    if grouping {
        b.code("#,##");
    }
    if ints == 0 {
        // No integer digit is forced, so a value below one shows none (`.5`).
        if !grouping {
            b.code("#");
        }
    } else {
        b.digits('0', ints);
    }
    decimals(b, n);
}

fn scientific(b: &mut Body, n: Node) {
    b.digits('0', digits(n, "min-integer-digits").unwrap_or(1).max(1));
    decimals(b, n);
    b.code(if attr_bool(n, "forced-exponent-sign").unwrap_or(true) {
        "E+"
    } else {
        "E-"
    });
    b.digits('0', digits(n, "min-exponent-digits").unwrap_or(2).max(1));
}

fn fraction(b: &mut Body, n: Node) {
    let ints = digits(n, "min-integer-digits").unwrap_or(0);
    if ints > 0 {
        b.digits('0', ints);
        // A space, which is both the separator Excel writes and the break that
        // makes the whole part its own placeholder run.
        b.code(" ");
    }
    b.digits('?', digits(n, "min-numerator-digits").unwrap_or(1).max(1));
    b.code("/");
    match attr_u32(n, "denominator-value").filter(|v| *v > 0) {
        // Quoted: a fixed denominator ending in a zero would otherwise be read
        // as a literal digit followed by a placeholder.
        Some(v) => {
            b.code("\"");
            b.code(&v.min(9999).to_string());
            b.code("\"");
        }
        None => b.digits('?', digits(n, "min-denominator-digits").unwrap_or(1).max(1)),
    }
}

fn seconds(b: &mut Body, n: Node) {
    b.code(if long(n) { "ss" } else { "s" });
    // Fractional seconds, which the grammar spells as a decimal run immediately
    // after the seconds token.
    decimals(b, n);
}

fn decimals(b: &mut Body, n: Node) {
    let dec = digits(n, "decimal-places").unwrap_or(0);
    if dec > 0 {
        b.code(".");
        b.digits('0', dec);
    }
}

fn digits(n: Node, attr: &str) -> Option<u32> {
    attr_u32(n, attr).map(|v| v.min(MAX_DIGITS))
}

fn long(n: Node) -> bool {
    attr_local(n, "style").map(|v| v.trim()) == Some("long")
}

fn text_of(n: Node) -> String {
    let mut s = String::new();
    inner_text(n, &mut s);
    s
}

/// `style:condition`. Excel's sections are fixed at positive / negative / zero,
/// so a threshold other than zero selects nothing and the map is dropped.
fn condition(v: &str) -> Option<Cond> {
    let s: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    match s.strip_prefix("value()")? {
        ">=0" | ">0" => Some(Cond::Positive),
        "<0" | "<=0" => Some(Cond::Negative),
        "=0" | "==0" => Some(Cond::Zero),
        _ => None,
    }
}

/// A `fo:color` as one of `numfmt`'s colour brackets. Excel names eight colours
/// and `numfmt` knows exactly those, so a colour is expressible only when it is
/// one of them on the nose. Anything else is dropped rather than snapped to a
/// name it is merely near.
fn bracket_color(hex: &str) -> Option<&'static str> {
    Some(match hex.trim().to_ascii_lowercase().as_str() {
        "#000000" => "[Black]",
        "#0000ff" => "[Blue]",
        "#00ffff" => "[Cyan]",
        "#008000" => "[Green]",
        "#ff00ff" => "[Magenta]",
        "#ff0000" => "[Red]",
        "#ffffff" => "[White]",
        "#ffff00" => "[Yellow]",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = concat!(
        r#" xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""#,
        r#" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0""#,
        r#" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0""#,
        r#" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0""#,
    );

    fn numbers(body: &str) -> Numbers {
        Numbers::parse(
            None,
            &format!(
                "<office:document-content{NS}><office:automatic-styles>{body}\
                 </office:automatic-styles></office:document-content>"
            ),
        )
    }

    /// `(code, format)` for the one style a fixture defines.
    fn one(body: &str) -> (String, Format) {
        let n = numbers(body);
        let code = n.code("N1").expect("style N1 is defined");
        (code, n.format("N1").expect("style N1 is defined"))
    }

    /// 2025-01-21, which is `numfmt`'s own worked example.
    const DAY: f64 = 45678.0;

    #[test]
    fn an_integer_states_its_minimum_width_and_its_grouping() {
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:number number:decimal-places="0" number:min-integer-digits="3"
                    number:grouping="true"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "#,##000");
        assert_eq!(f.apply(42.0), "042");
        assert_eq!(f.apply(1234567.0), "1,234,567");
        assert!(!f.is_date());

        // `min-integer-digits="0"` forces no leading digit at all.
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:number number:decimal-places="2" number:min-integer-digits="0"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "#.00");
        assert_eq!(f.apply(0.5), ".50");
    }

    #[test]
    fn decimals_round_and_pad() {
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:number number:decimal-places="2" number:min-integer-digits="1"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "0.00");
        assert_eq!(f.apply(3.14159), "3.14");
        assert_eq!(f.apply(0.5), "0.50");
        assert_eq!(f.apply(-7.0), "-7.00");
    }

    #[test]
    fn a_percentage_scales_by_a_hundred() {
        let (code, f) = one(
            r#"<number:percentage-style style:name="N1">
                 <number:number number:decimal-places="1" number:min-integer-digits="1"/>
                 <number:text>%</number:text>
               </number:percentage-style>"#,
        );
        // The `%` is the live token, not a quoted literal: ODF stores the value
        // as a fraction and the ×100 rides the sign.
        assert_eq!(code, "0.0%");
        assert_eq!(f.apply(0.156), "15.6%");
        assert_eq!(f.apply(1.0), "100.0%");

        // A `%` inside a *number* style is a literal and scales nothing.
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/>
                 <number:text> %</number:text>
               </number:number-style>"#,
        );
        assert_eq!(code, "0\" %\"");
        assert_eq!(f.apply(15.0), "15 %");
    }

    #[test]
    fn a_currency_symbol_goes_where_the_document_puts_it() {
        let (code, f) = one(
            r#"<number:currency-style style:name="N1">
                 <number:currency-symbol>€</number:currency-symbol>
                 <number:number number:decimal-places="2" number:min-integer-digits="1"
                    number:grouping="true"/>
               </number:currency-style>"#,
        );
        assert_eq!(code, "\"€\"#,##0.00");
        assert_eq!(f.apply(1234.5), "€1,234.50");

        let (code, f) = one(
            r#"<number:currency-style style:name="N1">
                 <number:number number:decimal-places="2" number:min-integer-digits="1"
                    number:grouping="true"/>
                 <number:text> </number:text>
                 <number:currency-symbol>kr</number:currency-symbol>
               </number:currency-style>"#,
        );
        assert_eq!(code, "#,##0.00\" \"\"kr\"");
        assert_eq!(f.apply(1234.5), "1,234.50 kr");

        // An empty symbol names a locale glyph this module cannot know.
        let (code, f) = one(
            r#"<number:currency-style style:name="N1">
                 <number:currency-symbol number:language="en" number:country="US"/>
                 <number:number number:decimal-places="2" number:min-integer-digits="1"/>
               </number:currency-style>"#,
        );
        assert_eq!(code, "0.00");
        assert_eq!(f.apply(2.5), "2.50");
    }

    #[test]
    fn long_and_short_dates_render_their_serial() {
        let (code, f) = one(
            r#"<number:date-style style:name="N1">
                 <number:year number:style="long"/><number:text>-</number:text>
                 <number:month number:style="long"/><number:text>-</number:text>
                 <number:day number:style="long"/>
               </number:date-style>"#,
        );
        assert_eq!(code, "yyyy\"-\"mm\"-\"dd");
        assert!(f.is_date());
        assert_eq!(f.apply(DAY), "2025-01-21");

        let (code, f) = one(
            r#"<number:date-style style:name="N1">
                 <number:day number:style="short"/><number:text>.</number:text>
                 <number:month number:style="short"/><number:text>.</number:text>
                 <number:year number:style="short"/>
               </number:date-style>"#,
        );
        assert_eq!(code, "d\".\"m\".\"yy");
        assert_eq!(f.apply(DAY), "21.1.25");
    }

    #[test]
    fn a_textual_month_and_a_weekday_name_keep_their_widths() {
        let (code, f) = one(
            r#"<number:date-style style:name="N1">
                 <number:day-of-week number:style="long"/><number:text>, </number:text>
                 <number:month number:textual="true" number:style="long"/>
                 <number:text> </number:text>
                 <number:day number:style="short"/>
               </number:date-style>"#,
        );
        assert_eq!(code, "dddd\", \"mmmm\" \"d");
        assert_eq!(f.apply(DAY), "Tuesday, January 21");

        let (code, f) = one(
            r#"<number:date-style style:name="N1">
                 <number:day-of-week number:style="short"/><number:text> </number:text>
                 <number:month number:textual="true" number:style="short"/>
               </number:date-style>"#,
        );
        assert_eq!(code, "ddd\" \"mmm");
        assert_eq!(f.apply(DAY), "Tue Jan");
    }

    #[test]
    fn a_time_with_am_pm_reads_its_minutes_as_minutes() {
        let (code, f) = one(
            r#"<number:time-style style:name="N1">
                 <number:hours number:style="short"/><number:text>:</number:text>
                 <number:minutes number:style="long"/><number:text> </number:text>
                 <number:am-pm/>
               </number:time-style>"#,
        );
        assert_eq!(code, "h\":\"mm\" \"AM/PM");
        // 18:00 as a fraction of a day, and `mm` beside an hour is minutes and
        // not the month of the serial.
        assert_eq!(f.apply(0.75), "6:00 PM");
        assert_eq!(f.apply(0.0), "12:00 AM");

        // Seconds carry their own fractional run.
        let (code, f) = one(
            r#"<number:time-style style:name="N1">
                 <number:minutes number:style="long"/><number:text>:</number:text>
                 <number:seconds number:style="long" number:decimal-places="2"/>
               </number:time-style>"#,
        );
        assert_eq!(code, "mm\":\"ss.00");
        assert_eq!(f.apply(0.5 + 1.0 / 24.0 / 60.0), "01:00.00");
    }

    #[test]
    fn literal_text_cannot_open_a_section_or_a_bracket() {
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/>
                 <number:text> [naïve;"café"] </number:text>
               </number:number-style>"#,
        );
        // The quote is backslash-escaped out of the quoted run and back into it;
        // the `;` and the `[` ride inside it untouched.
        assert_eq!(code, "0\" [naïve;\"\\\"\"café\"\\\"\"] \"");
        // The round trip is the real assertion: one section, and every character
        // of the document's text still in it.
        assert_eq!(f.apply(7.0), "7 [naïve;\"café\"] ");
        assert_eq!(f.apply(-7.0), "-7 [naïve;\"café\"] ");
    }

    #[test]
    fn a_style_map_pair_becomes_the_positive_and_negative_sections() {
        // LibreOffice's own shape: the style a cell names is the negative one,
        // and its `value()>=0` map points at the positive.
        let n = numbers(
            r##"<number:number-style style:name="N1P0" style:volatile="true">
                  <number:number number:decimal-places="2" number:min-integer-digits="1"
                     number:grouping="true"/>
                </number:number-style>
                <number:number-style style:name="N1">
                  <style:text-properties fo:color="#ff0000"/>
                  <number:text>-</number:text>
                  <number:number number:decimal-places="2" number:min-integer-digits="1"
                     number:grouping="true"/>
                  <style:map style:condition="value()&gt;=0"
                     style:apply-style-name="N1P0"/>
                </number:number-style>"##,
        );
        assert_eq!(
            n.code("N1").as_deref(),
            Some("#,##0.00;[Red]\"-\"#,##0.00")
        );
        let f = n.format("N1").expect("N1");
        assert_eq!(f.apply(1234.5), "1,234.50");
        assert_eq!(f.apply(-1234.5), "-1,234.50");
        assert_eq!(f.color(-1.0), Some("#ff0000"));
        assert_eq!(f.color(1.0), None);

        // A zero map with no negative one still needs Excel's third slot, so the
        // negative section is the positive body behind a minus.
        let n = numbers(
            r#"<number:text-style style:name="N1Z"><number:text>–</number:text>
               </number:text-style>
               <number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/>
                 <number:text> kg</number:text>
                 <style:map style:condition="value()=0" style:apply-style-name="N1Z"/>
               </number:number-style>"#,
        );
        // The map's target is a *text* style, which is not a value section: the
        // slot stays unfilled rather than borrowing a section from the wrong
        // family.
        assert_eq!(n.code("N1").as_deref(), Some("0\" kg\""));

        let n = numbers(
            r#"<number:number-style style:name="N1Z"><number:text>–</number:text>
               </number:number-style>
               <number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/>
                 <number:text> kg</number:text>
                 <style:map style:condition="value()=0" style:apply-style-name="N1Z"/>
               </number:number-style>"#,
        );
        assert_eq!(n.code("N1").as_deref(), Some("0\" kg\";-0\" kg\";\"–\""));
        let f = n.format("N1").expect("N1");
        assert_eq!(f.apply(5.0), "5 kg");
        assert_eq!(f.apply(-5.0), "-5 kg");
        assert_eq!(f.apply(0.0), "–");

        // A condition Excel has no section for is dropped, leaving the base.
        let n = numbers(
            r#"<number:number-style style:name="N1P0">
                 <number:number number:min-integer-digits="1"/>
               </number:number-style>
               <number:number-style style:name="N1">
                 <number:number number:decimal-places="1" number:min-integer-digits="1"/>
                 <style:map style:condition="value()&gt;100" style:apply-style-name="N1P0"/>
               </number:number-style>"#,
        );
        assert_eq!(n.code("N1").as_deref(), Some("0.0"));

        // A map to a style the document never defines does the same.
        let n = numbers(
            r#"<number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/>
                 <style:map style:condition="value()&lt;0" style:apply-style-name="café"/>
               </number:number-style>"#,
        );
        assert_eq!(n.code("N1").as_deref(), Some("0"));
    }

    #[test]
    fn a_boolean_style_spells_true_and_false() {
        let (code, f) = one(
            r#"<number:boolean-style style:name="N1"><number:boolean/>
               </number:boolean-style>"#,
        );
        // Excel has no boolean token, but it does have a zero section, and an ODF
        // boolean is exactly 0 or 1.
        assert_eq!(code, "\"TRUE\";\"TRUE\";\"FALSE\"");
        assert_eq!(f.apply(1.0), "TRUE");
        assert_eq!(f.apply(0.0), "FALSE");
    }

    #[test]
    fn a_text_style_lands_in_the_only_section_text_reads() {
        let (code, f) = one(
            r#"<number:text-style style:name="N1">
                 <number:text>« </number:text><number:text-content/>
                 <number:text> »</number:text>
               </number:text-style>"#,
        );
        assert_eq!(code, "General;General;General;\"« \"@\" »\"");
        assert_eq!(f.apply_text("café"), "« café »");
        // Handed a number instead, it renders as an unformatted cell would.
        assert_eq!(f.apply(5.0), "5");
    }

    #[test]
    fn scientific_and_fraction_forms_round_trip() {
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:scientific-number number:decimal-places="2"
                    number:min-integer-digits="1" number:min-exponent-digits="2"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "0.00E+00");
        assert_eq!(f.apply(12345.0), "1.23E+04");

        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:fraction number:min-integer-digits="1"
                    number:min-numerator-digits="1" number:min-denominator-digits="1"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "0 ?/?");
        assert_eq!(f.apply(2.5), "2 1/2");

        // A fixed denominator is quoted so a trailing zero stays a literal.
        let (code, f) = one(
            r#"<number:number-style style:name="N1">
                 <number:fraction number:min-integer-digits="1"
                    number:denominator-value="10"/>
               </number:number-style>"#,
        );
        assert_eq!(code, "0 ?/\"10\"");
        assert_eq!(f.apply(2.3), "2 3/10");
    }

    #[test]
    fn constructs_with_no_format_code_contribute_nothing() {
        let (code, f) = one(
            r#"<number:date-style style:name="N1">
                 <number:era number:style="long"/><number:quarter/>
                 <number:week-of-year/>
                 <number:year number:style="long"/>
               </number:date-style>"#,
        );
        assert_eq!(code, "yyyy");
        assert_eq!(f.apply(DAY), "2025");

        // A colour the eight Excel names do not cover is dropped, not snapped.
        let n = numbers(
            r##"<number:number-style style:name="N1">
                  <style:text-properties fo:color="#2f5496"/>
                  <number:number number:min-integer-digits="1"/>
                </number:number-style>"##,
        );
        assert_eq!(n.code("N1").as_deref(), Some("0"));
        assert_eq!(n.format("N1").expect("N1").color(1.0), None);
    }

    #[test]
    fn missing_and_empty_styles_degrade_to_none() {
        assert!(numbers("").format("N1").is_none());
        assert!(Numbers::empty().code("N1").is_none());
        assert!(!Numbers::empty().has("N1"));
        // Unparseable parts cost the document its formats and nothing else.
        assert!(Numbers::parse(Some("<office:document-styles>"), "<not xml")
            .code("N1")
            .is_none());
        // An unnamed style is not addressable and is skipped.
        assert!(numbers(r#"<number:number-style><number:number/></number:number-style>"#)
            .code("N1")
            .is_none());
        // An element that is not a number style at all.
        assert!(numbers(r#"<style:style style:name="N1" style:family="text"/>"#)
            .code("N1")
            .is_none());
        // A style with no children is an empty section, which is a defined style
        // that happens to render as General.
        let (code, f) = one(r#"<number:number-style style:name="N1"/>"#);
        assert_eq!(code, "");
        assert_eq!(f.apply(5.0), "5");
    }

    #[test]
    fn document_controlled_sizes_are_capped() {
        let long = "café ".repeat(200);
        let (code, f) = one(&format!(
            r#"<number:number-style style:name="N1">
                 <number:number number:decimal-places="999" number:min-integer-digits="999"/>
                 <number:text>{long}</number:text>
               </number:number-style>"#
        ));
        assert!(code.len() <= MAX_BODY, "{} bytes", code.len());
        // Truncation lands on a token boundary: the quoted run is still closed,
        // so the code parses as one section rather than swallowing the rest.
        assert_eq!(code.matches('"').count() % 2, 0, "{code}");
        assert!(f.apply(1.0).starts_with(&"0".repeat(MAX_DIGITS as usize - 1)));

        // Four capped sections still fit inside what `numfmt` will parse, which
        // is what keeps a long format from silently becoming General.
        let n = numbers(&format!(
            r#"<number:number-style style:name="N1P0">
                 <number:number number:min-integer-digits="1"/><number:text>{long}</number:text>
               </number:number-style>
               <number:number-style style:name="N1Z">
                 <number:number number:min-integer-digits="1"/><number:text>{long}</number:text>
               </number:number-style>
               <number:number-style style:name="N1">
                 <number:number number:min-integer-digits="1"/><number:text>{long}</number:text>
                 <style:map style:condition="value()&gt;=0" style:apply-style-name="N1P0"/>
                 <style:map style:condition="value()=0" style:apply-style-name="N1Z"/>
               </number:number-style>"#
        ));
        let code = n.code("N1").expect("N1");
        assert!(code.len() <= 512, "{} bytes", code.len());
        assert!(n.format("N1").expect("N1").apply(1.0).starts_with('1'));
    }
}
