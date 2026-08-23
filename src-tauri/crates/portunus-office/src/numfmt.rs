//! Excel/ODF number formats: parse a format code once per cell style, then
//! apply it per cell. This is what turns a spreadsheet's raw `45678` back into
//! `2025-01-21` — a date cell stores a serial number and *only* the format code
//! says it is a date, so without this module dates render as integers.
//!
//! Format codes are untrusted document content, so nothing here panics or
//! allocates unboundedly: over-long codes and unparseable grammar degrade to
//! General, and every scan is bounded by a constant.

#![allow(dead_code)] // Consumed by the later-stage renderers.

/// Longest format code accepted. Real codes are tens of characters; past this a
/// code is either generated garbage or an attempt to make the tokenizer the
/// expensive part of a preview.
const MAX_CODE: usize = 512;
/// Token cap per section. Bounds both parse work and the render walk.
const MAX_TOKENS: usize = 256;
/// Sections beyond the fourth are ignored (Excel defines exactly four).
const MAX_SECTIONS: usize = 4;
/// Excel's own "column too narrow / value not representable" marker. Used for
/// values a date section cannot render at all (negative or absurd serials).
const OVERFLOW: &str = "########";

// ── tokens ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Hour,
    Minute,
    Second,
}

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Lit(String),
    /// `_x`: reserve the width of `x`. Rendered as a space, since the preview
    /// has no way to measure the glyph.
    Skip,
    /// `*x`: repeat `x` to fill the column. No column width here, so it emits
    /// nothing.
    Fill,
    Year(u8),
    /// Width 1..=5; 5 is `mmmmm`, the single-letter month. Still ambiguous
    /// between month and minute at this point for widths 1 and 2 — see
    /// `resolve_minutes`.
    Month(u8),
    Day(u8),
    Hour(u8),
    Minute(u8),
    Second(u8),
    /// `[h]` / `[m]` / `[s]`: elapsed time, not clock time.
    Elapsed(Unit, u8),
    /// `AM/PM` (true) or `A/P` (false). Either one switches hours to 12-hour.
    Ampm(bool),
    /// `.0` / `.00` / `.000` after a seconds token.
    SubSec(u8),
    /// `0`, `#` or `?` digit placeholder.
    Digit(u8),
    Dot,
    /// `,` between digit placeholders (grouping) or trailing (scale by 1000).
    Comma,
    Percent,
    /// `E+` (true, always signs the exponent) or `E-` (false).
    Exp(bool),
    Slash,
    /// `@`: the raw text of the cell.
    At,
}

impl Tok {
    fn is_datetime(&self) -> bool {
        matches!(
            self,
            Tok::Year(_)
                | Tok::Month(_)
                | Tok::Day(_)
                | Tok::Hour(_)
                | Tok::Minute(_)
                | Tok::Second(_)
                | Tok::Elapsed(_, _)
                | Tok::SubSec(_)
        )
    }

    // Separators the month/minute disambiguation looks through: they carry no
    // date/number meaning of their own.
    fn is_separator(&self) -> bool {
        matches!(self, Tok::Lit(_) | Tok::Skip | Tok::Fill | Tok::Slash)
    }
}

// ── sections ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    General,
    Date,
    Fraction,
    Number,
}

#[derive(Clone, Debug, Default)]
struct NumSpec {
    /// Every placeholder left of the decimal, `#` included. Digits are dealt
    /// across these right-to-left, so a code like `000-0000` splits a number
    /// across its literals the way Excel does.
    int_all: usize,
    /// `0` placeholders left of the decimal: minimum integer digits.
    int_zeros: usize,
    /// `0` + `?` left of the decimal: width to pad to (with spaces for `?`).
    int_pad: usize,
    /// All placeholders right of the decimal: the rounding precision.
    dec_places: usize,
    dec_zeros: usize,
    dec_pad: usize,
    thousands: bool,
    /// Trailing commas: each divides the value by 1000.
    scale: u32,
    /// `%` count: each multiplies by 100 (and prints, as a literal token).
    percent: u32,
    sci: bool,
    exp_places: usize,
    exp_plus: bool,
    has_digits: bool,
}

#[derive(Clone, Debug, Default)]
struct FracSpec {
    /// A whole-number part precedes the fraction (`# ?/?` has one, `?/?` not).
    has_whole: bool,
    /// The whole-number run holds a `0`, so a zero whole part still prints.
    whole_forced: bool,
    num_places: usize,
    den_places: usize,
    /// Literal denominator, as in `# ?/16`.
    den_fixed: Option<u32>,
}

#[derive(Clone, Debug)]
struct Section {
    toks: Vec<Tok>,
    kind: Kind,
    color: Option<&'static str>,
    num: NumSpec,
    frac: FracSpec,
    /// Any `AM/PM` / `A/P` token: hours render 12-hour.
    twelve_hour: bool,
    elapsed: Option<Unit>,
}

impl Section {
    fn general() -> Section {
        Section {
            toks: Vec::new(),
            kind: Kind::General,
            color: None,
            num: NumSpec::default(),
            frac: FracSpec::default(),
            twelve_hour: false,
            elapsed: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.kind != Kind::General && self.toks.is_empty()
    }
}

/// A parsed Excel/ODF number format. Parse once per style, apply per cell.
#[derive(Clone, Debug)]
pub struct Format {
    sections: Vec<Section>,
    date1904: bool,
}

impl Format {
    /// Parse a code such as `#,##0.00`, `yyyy-mm-dd`, `0.00%`, or a full
    /// 4-section `pos;neg;zero;text` code. Never fails: anything unrecognizable
    /// ends up rendering as General.
    pub fn parse(code: &str) -> Format {
        let mut f = Format {
            sections: parse_sections(code),
            date1904: false,
        };
        if f.sections.is_empty() {
            f.sections.push(Section::general());
        }
        f
    }

    /// Excel's built-in `numFmtId` formats (0-49). `None` for ids that are
    /// locale-reserved (23-36) or out of range.
    pub fn builtin(id: u16) -> Option<Format> {
        builtin_code(id).map(Format::parse)
    }

    /// Selects the 1904 date system (`workbook.xml`'s `date1904` flag), where
    /// serial 0 is 1904-01-01 and the phantom 1900 leap day does not exist.
    pub fn with_date1904(mut self, on: bool) -> Format {
        self.date1904 = on;
        self
    }

    pub fn set_date1904(&mut self, on: bool) {
        self.date1904 = on;
    }

    pub fn date1904(&self) -> bool {
        self.date1904
    }

    /// Whether this renders a date/time. The caller needs it to know that a
    /// bare number is a serial date.
    pub fn is_date(&self) -> bool {
        self.sections
            .first()
            .map(|s| s.kind == Kind::Date)
            .unwrap_or(false)
    }

    /// Excel's `[Red]`/`[Blue]` modifier for the section `value` selects, as a
    /// CSS colour.
    pub fn color(&self, value: f64) -> Option<&'static str> {
        self.select(value).and_then(|s| s.color)
    }

    // Section choice: one section covers everything; two split
    // positive/negative (the negative section formats the absolute value, since
    // it carries its own sign or parentheses); three or more add a zero
    // section.
    fn select(&self, value: f64) -> Option<&Section> {
        let n = self.sections.len();
        if n == 0 {
            return None;
        }
        let idx = if n == 1 {
            0
        } else if n == 2 {
            if value < 0.0 {
                1
            } else {
                0
            }
        } else if value < 0.0 {
            1
        } else if value == 0.0 {
            2
        } else {
            0
        };
        self.sections.get(idx.min(n - 1))
    }
}

// ── parsing ──────────────────────────────────────────────────────────────────

fn parse_sections(code: &str) -> Vec<Section> {
    // An absent code and an over-long one both mean General. An *empty section*
    // inside a real code is different — that one hides its value class.
    if code.is_empty() || code.len() > MAX_CODE {
        return vec![Section::general()];
    }
    let chars: Vec<char> = code.chars().collect();
    let mut out: Vec<Section> = Vec::new();
    let mut toks: Vec<Tok> = Vec::new();
    let mut color: Option<&'static str> = None;
    let mut i = 0usize;
    while i < chars.len() {
        if out.len() >= MAX_SECTIONS {
            break;
        }
        let c = chars[i];
        if c == ';' {
            out.push(finish_section(std::mem::take(&mut toks), color.take()));
            i += 1;
            continue;
        }
        if toks.len() >= MAX_TOKENS {
            // Skip to the next section rather than growing the token list.
            while i < chars.len() && chars[i] != ';' {
                i += 1;
            }
            continue;
        }
        i = token_at(&chars, i, &mut toks, &mut color);
    }
    // Every `;` opened a following section, so a non-empty `out` always owes one
    // more — even when it is empty, since `0.00;` deliberately hides negatives.
    if out.len() < MAX_SECTIONS {
        out.push(finish_section(toks, color));
    }
    out
}

// Consumes one token starting at `i` and returns the next index. Always
// advances by at least one, so the caller's loop cannot spin.
fn token_at(
    chars: &[char],
    i: usize,
    toks: &mut Vec<Tok>,
    color: &mut Option<&'static str>,
) -> usize {
    let c = chars[i];
    match c {
        '"' => {
            let mut s = String::new();
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                s.push(chars[j]);
                j += 1;
            }
            push_lit(toks, &s);
            // Skip the closing quote when there is one; an unterminated run
            // just ends at the code's end.
            j + usize::from(j < chars.len())
        }
        '\\' => {
            if i + 1 < chars.len() {
                push_lit(toks, &chars[i + 1].to_string());
                i + 2
            } else {
                i + 1
            }
        }
        '_' => {
            toks.push(Tok::Skip);
            // The width character is consumed but never printed.
            i + 2usize.min(chars.len() - i)
        }
        '*' => {
            toks.push(Tok::Fill);
            i + 2usize.min(chars.len() - i)
        }
        '[' => bracket_at(chars, i, toks, color),
        '0' | '#' | '?' => {
            toks.push(Tok::Digit(c as u8));
            i + 1
        }
        '.' => {
            // `.0` right after a seconds token is fractional seconds, not a
            // decimal point.
            let after = run_len(chars, i + 1, '0');
            if after > 0 && toks.iter().rev().any(|t| matches!(t, Tok::Second(_))) {
                toks.push(Tok::SubSec(after.min(9) as u8));
                return i + 1 + after;
            }
            toks.push(Tok::Dot);
            i + 1
        }
        ',' => {
            toks.push(Tok::Comma);
            i + 1
        }
        '%' => {
            toks.push(Tok::Percent);
            i + 1
        }
        '/' => {
            toks.push(Tok::Slash);
            i + 1
        }
        '@' => {
            toks.push(Tok::At);
            i + 1
        }
        'e' | 'E' => {
            match chars.get(i + 1) {
                Some('+') => {
                    toks.push(Tok::Exp(true));
                    i + 2
                }
                Some('-') => {
                    toks.push(Tok::Exp(false));
                    i + 2
                }
                _ => {
                    push_lit(toks, &c.to_string());
                    i + 1
                }
            }
        }
        'a' | 'A' => {
            if let Some(n) = ampm_at(chars, i) {
                toks.push(Tok::Ampm(n == 5));
                i + n
            } else {
                push_lit(toks, &c.to_string());
                i + 1
            }
        }
        'y' | 'Y' => {
            let n = run_len_ci(chars, i, 'y');
            // Excel: 1-2 y's are the 2-digit year, 3+ the 4-digit year.
            toks.push(Tok::Year(if n <= 2 { 2 } else { 4 }));
            i + n
        }
        'm' | 'M' => {
            let n = run_len_ci(chars, i, 'm');
            toks.push(Tok::Month(n.min(5) as u8));
            i + n
        }
        'd' | 'D' => {
            let n = run_len_ci(chars, i, 'd');
            toks.push(Tok::Day(n.min(4) as u8));
            i + n
        }
        'h' | 'H' => {
            let n = run_len_ci(chars, i, 'h');
            toks.push(Tok::Hour(n.min(2) as u8));
            i + n
        }
        's' | 'S' => {
            let n = run_len_ci(chars, i, 's');
            toks.push(Tok::Second(n.min(2) as u8));
            i + n
        }
        // `g`/`b` are era and calendar selectors; there is no era support here,
        // so they are dropped rather than printed as letters.
        'g' | 'G' | 'b' | 'B' => {
            let word = word_at(chars, i);
            if word.eq_ignore_ascii_case("general") {
                push_lit(toks, &word);
                return i + word.chars().count();
            }
            i + run_len_ci(chars, i, c.to_ascii_lowercase())
        }
        _ => {
            push_lit(toks, &c.to_string());
            i + 1
        }
    }
}

// `[...]`: a colour name, an elapsed-time unit, or something to ignore
// (`[$-409]` locale/currency, `[>100]` conditions). Ignored means *not
// printed* — the brackets and their contents vanish.
fn bracket_at(
    chars: &[char],
    i: usize,
    toks: &mut Vec<Tok>,
    color: &mut Option<&'static str>,
) -> usize {
    let mut j = i + 1;
    let mut body = String::new();
    while j < chars.len() && chars[j] != ']' {
        body.push(chars[j]);
        j += 1;
    }
    let next = j + usize::from(j < chars.len());
    let lower = body.trim().to_ascii_lowercase();
    if let Some(css) = color_name(&lower) {
        *color = Some(css);
        return next;
    }
    // Elapsed unit: the body is a run of one time letter and nothing else.
    let mut cs = lower.chars();
    if let Some(first) = cs.next() {
        if matches!(first, 'h' | 'm' | 's') && lower.chars().all(|c| c == first) {
            let unit = match first {
                'h' => Unit::Hour,
                'm' => Unit::Minute,
                _ => Unit::Second,
            };
            toks.push(Tok::Elapsed(unit, lower.chars().count().min(2) as u8));
        }
    }
    next
}

fn color_name(lower: &str) -> Option<&'static str> {
    match lower {
        "black" => Some("#000000"),
        "blue" => Some("#0000ff"),
        "cyan" => Some("#00ffff"),
        "green" => Some("#008000"),
        "magenta" => Some("#ff00ff"),
        "red" => Some("#ff0000"),
        "white" => Some("#ffffff"),
        "yellow" => Some("#ffff00"),
        _ => {
            // `[Color n]` indexes the legacy 56-colour palette; only the first
            // eight are stable enough across Excel versions to be worth
            // mapping, and the rest fall back to no colour rather than to a
            // wrong one.
            let n = lower.strip_prefix("color")?.trim().parse::<u32>().ok()?;
            match n {
                1 => Some("#000000"),
                2 => Some("#ffffff"),
                3 => Some("#ff0000"),
                4 => Some("#00ff00"),
                5 => Some("#0000ff"),
                6 => Some("#ffff00"),
                7 => Some("#ff00ff"),
                8 => Some("#00ffff"),
                _ => None,
            }
        }
    }
}

fn ampm_at(chars: &[char], i: usize) -> Option<usize> {
    let rest: String = chars[i..chars.len().min(i + 5)]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    if rest.starts_with("am/pm") {
        Some(5)
    } else if rest.starts_with("a/p") {
        Some(3)
    } else {
        None
    }
}

fn run_len_ci(chars: &[char], i: usize, lower: char) -> usize {
    let mut n = 0;
    while i + n < chars.len() && chars[i + n].to_ascii_lowercase() == lower {
        n += 1;
    }
    n.max(1)
}

fn run_len(chars: &[char], i: usize, c: char) -> usize {
    let mut n = 0;
    while i + n < chars.len() && chars[i + n] == c {
        n += 1;
    }
    n
}

fn word_at(chars: &[char], i: usize) -> String {
    let mut s = String::new();
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_alphabetic() {
        s.push(chars[j]);
        j += 1;
    }
    s
}

// Literal runs are merged so the render walk emits fewer, larger pushes and the
// token cap counts meaningful tokens rather than individual punctuation.
fn push_lit(toks: &mut Vec<Tok>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Tok::Lit(prev)) = toks.last_mut() {
        prev.push_str(s);
        return;
    }
    toks.push(Tok::Lit(s.to_string()));
}

fn finish_section(mut toks: Vec<Tok>, color: Option<&'static str>) -> Section {
    let mut s = Section::general();
    s.color = color;
    // A section of nothing but the word "General" (any case) is General.
    let only_lits = toks.iter().all(|t| matches!(t, Tok::Lit(_)));
    if only_lits {
        let joined: String = toks
            .iter()
            .map(|t| match t {
                Tok::Lit(l) => l.as_str(),
                _ => "",
            })
            .collect();
        if joined.trim().eq_ignore_ascii_case("general") {
            return s;
        }
        if toks.is_empty() {
            // Genuinely empty section (e.g. the `;;` that hides zeros): renders
            // as nothing, which `is_empty` distinguishes from General.
            s.kind = Kind::Number;
            return s;
        }
    }
    resolve_minutes(&mut toks);
    s.twelve_hour = toks.iter().any(|t| matches!(t, Tok::Ampm(_)));
    s.elapsed = toks.iter().find_map(|t| match t {
        Tok::Elapsed(u, _) => Some(*u),
        _ => None,
    });
    s.kind = if toks.iter().any(|t| t.is_datetime()) {
        Kind::Date
    } else if let Some(f) = frac_spec(&toks) {
        s.frac = f;
        Kind::Fraction
    } else {
        Kind::Number
    };
    if s.kind == Kind::Number {
        s.num = num_spec(&toks);
    }
    s.toks = toks;
    s
}

// The classic trap: `m` is the month, except when it is adjacent to an hour
// token before it or a seconds token after it, when it is the minute. Widths of
// 3+ (`mmm`, `mmmm`, `mmmmm`) are always the month name. Adjacency looks
// *through* separators only, so `m/d` stays a month while `h:m` and `mm:ss`
// become minutes.
fn resolve_minutes(toks: &mut [Tok]) {
    let mut minutes: Vec<usize> = Vec::new();
    for idx in 0..toks.len() {
        match toks[idx] {
            Tok::Month(w) if w <= 2 => {}
            _ => continue,
        }
        let before_hour = toks[..idx]
            .iter()
            .rev()
            .find(|t| !t.is_separator())
            .map(|t| matches!(t, Tok::Hour(_) | Tok::Elapsed(Unit::Hour, _)))
            .unwrap_or(false);
        let after_second = toks[idx + 1..]
            .iter()
            .find(|t| !t.is_separator())
            .map(|t| matches!(t, Tok::Second(_)))
            .unwrap_or(false);
        if before_hour || after_second {
            minutes.push(idx);
        }
    }
    for idx in minutes {
        if let Tok::Month(w) = toks[idx] {
            toks[idx] = Tok::Minute(w);
        }
    }
}

fn num_spec(toks: &[Tok]) -> NumSpec {
    let mut n = NumSpec::default();
    let dot = toks.iter().position(|t| *t == Tok::Dot);
    let exp = toks.iter().position(|t| matches!(t, Tok::Exp(_)));
    let int_end = dot.or(exp).unwrap_or(toks.len());
    for t in &toks[..int_end] {
        match t {
            Tok::Digit(c) => {
                n.int_all += 1;
                n.has_digits = true;
                if *c == b'0' {
                    n.int_zeros += 1;
                }
                if matches!(*c, b'0' | b'?') {
                    n.int_pad += 1;
                }
            }
            Tok::Percent => n.percent += 1,
            _ => {}
        }
    }
    if let Some(d) = dot {
        let dec_end = exp.filter(|e| *e > d).unwrap_or(toks.len());
        for t in &toks[d + 1..dec_end] {
            match t {
                Tok::Digit(b'0') => {
                    n.dec_places += 1;
                    n.dec_zeros += 1;
                    n.dec_pad += 1;
                    n.has_digits = true;
                }
                Tok::Digit(b'?') => {
                    n.dec_places += 1;
                    n.dec_pad += 1;
                    n.has_digits = true;
                }
                Tok::Digit(_) => {
                    n.dec_places += 1;
                    n.has_digits = true;
                }
                Tok::Percent => n.percent += 1,
                _ => {}
            }
        }
    }
    if let Some(e) = exp {
        n.sci = true;
        n.exp_plus = matches!(toks[e], Tok::Exp(true));
        for t in &toks[e + 1..] {
            match t {
                Tok::Digit(_) => n.exp_places += 1,
                Tok::Percent => n.percent += 1,
                _ => {}
            }
        }
    }
    // Commas: grouping if a digit placeholder follows, scale-by-1000 if not.
    let last_digit = toks.iter().rposition(|t| matches!(t, Tok::Digit(_)));
    for (idx, t) in toks.iter().enumerate() {
        if *t != Tok::Comma {
            continue;
        }
        match last_digit {
            Some(ld) if idx < ld => n.thousands = true,
            Some(_) => n.scale = n.scale.saturating_add(1),
            None => {}
        }
    }
    // Only commas in an unbroken trailing run scale; `#,##0,` scales once,
    // `#,##0` not at all. Anything after a non-comma token is a literal.
    if n.scale > 0 {
        let mut run = 0u32;
        for t in toks.iter().rev() {
            match t {
                Tok::Comma => run += 1,
                Tok::Digit(_) => break,
                _ => {
                    run = 0;
                    break;
                }
            }
        }
        n.scale = run;
    }
    n
}

fn frac_spec(toks: &[Tok]) -> Option<FracSpec> {
    let slash = toks.iter().position(|t| *t == Tok::Slash)?;
    // Numerator side: the last unbroken run of placeholders before the slash.
    // An earlier run is the whole-number part (`# ?/?`).
    let mut runs: Vec<(usize, bool)> = Vec::new();
    let mut cur = (0usize, false);
    for t in &toks[..slash] {
        match t {
            Tok::Digit(c) => {
                cur.0 += 1;
                cur.1 |= *c == b'0';
            }
            _ => {
                if cur.0 > 0 {
                    runs.push(cur);
                }
                cur = (0, false);
            }
        }
    }
    if cur.0 > 0 {
        runs.push(cur);
    }
    let num_places = runs.last()?.0;
    // Denominator side: placeholders, or a literal integer (`# ?/16`).
    let mut den_places = 0usize;
    let mut den_fixed = None;
    for t in &toks[slash + 1..] {
        match t {
            Tok::Digit(_) => den_places += 1,
            Tok::Lit(l) if den_places == 0 && den_fixed.is_none() => {
                let digits: String = l.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(v) = digits.parse::<u32>() {
                    if v > 0 {
                        den_fixed = Some(v);
                    }
                }
                break;
            }
            _ => break,
        }
    }
    if den_places == 0 && den_fixed.is_none() {
        return None;
    }
    Some(FracSpec {
        has_whole: runs.len() > 1,
        whole_forced: runs.len() > 1 && runs[0].1,
        num_places,
        den_places: den_places.min(4),
        den_fixed,
    })
}

// ── built-ins ────────────────────────────────────────────────────────────────

// Excel's implicit numFmtId table. 14-22 and 45-47 are the date/time entries;
// 23-36 are reserved for locale-specific formats that no writer emits and that
// have no defined rendering here, so they are `None` and the caller falls back
// to General. The date codes are the en-US renderings — the actual glyph order
// is locale-dependent inside Excel, but a preview has no locale to consult.
fn builtin_code(id: u16) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        5 => "$#,##0_);($#,##0)",
        6 => "$#,##0_);[Red]($#,##0)",
        7 => "$#,##0.00_);($#,##0.00)",
        8 => "$#,##0.00_);[Red]($#,##0.00)",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "m/d/yyyy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0_);(#,##0)",
        38 => "#,##0_);[Red](#,##0)",
        39 => "#,##0.00_);(#,##0.00)",
        40 => "#,##0.00_);[Red](#,##0.00)",
        41 => "_(* #,##0_);_(* (#,##0);_(* \"-\"_);_(@_)",
        42 => "_(\"$\"* #,##0_);_(\"$\"* (#,##0);_(\"$\"* \"-\"_);_(@_)",
        43 => "_(* #,##0.00_);_(* (#,##0.00);_(* \"-\"??_);_(@_)",
        44 => "_(\"$\"* #,##0.00_);_(\"$\"* (#,##0.00);_(\"$\"* \"-\"??_);_(@_)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mm:ss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

// ── calendar ─────────────────────────────────────────────────────────────────

// Serial-to-civil offsets, expressed as days from the Unix epoch so one
// `civil_from_days` covers both date systems.
//
// The 1900 system's epoch looks like 1899-12-30 (serial 1 = 1900-01-01, two
// days later) but only for serials >= 61, because Excel inherited Lotus 1-2-3's
// belief that 1900 was a leap year. Serial 60 *is* that fictional 1900-02-29,
// and every serial below it is therefore one day further along than the epoch
// arithmetic suggests. This is load-bearing, not a curiosity: getting it wrong
// shifts every pre-March-1900 date by a day, and shifting it the other way
// breaks every modern date by a day.
const EPOCH_1900: i64 = -25569; // 1899-12-30
const EPOCH_1904: i64 = -24107; // 1904-01-01
/// Serial of the phantom 1900-02-29.
const PHANTOM_LEAP: i64 = 60;
/// Serials past this are absurd (year ~ 275760) and are refused rather than
/// pushed through the calendar arithmetic.
const MAX_SERIAL: f64 = 100_000_000.0;

struct Parts {
    y: i64,
    mo: u32,
    d: u32,
    dow: u32,
    h: u32,
    mi: u32,
    s: u32,
    /// Sub-second remainder in [0, 1).
    sub: f64,
    /// Whole units for the section's `[h]`/`[m]`/`[s]` token, if any.
    elapsed: u64,
}

// Howard Hinnant's civil-from-days: exact integer arithmetic, no lookup tables,
// correct for any proleptic-Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// 1970-01-01 was a Thursday; +11 keeps the modulo non-negative for the negative
// day numbers every 1900-system date produces.
fn weekday_from_days(z: i64) -> u32 {
    (((z % 7) + 11) % 7) as u32
}

// Rounding unit in days for the finest field the section actually prints.
// Excel rounds to that field rather than truncating, which is why 23:59:59.6
// with "h:mm" shows 00:00 rather than 23:59.
fn round_unit(toks: &[Tok]) -> f64 {
    let sub = toks.iter().find_map(|t| match t {
        Tok::SubSec(n) => Some(*n),
        _ => None,
    });
    if let Some(n) = sub {
        return 1.0 / (86400.0 * 10f64.powi(n as i32));
    }
    if toks
        .iter()
        .any(|t| matches!(t, Tok::Second(_) | Tok::Elapsed(Unit::Second, _)))
    {
        return 1.0 / 86400.0;
    }
    if toks
        .iter()
        .any(|t| matches!(t, Tok::Minute(_) | Tok::Elapsed(Unit::Minute, _)))
    {
        return 1.0 / 1440.0;
    }
    if toks
        .iter()
        .any(|t| matches!(t, Tok::Hour(_) | Tok::Elapsed(Unit::Hour, _)))
    {
        return 1.0 / 24.0;
    }
    // Date-only: Excel truncates the time of day, so no rounding at all.
    0.0
}

fn decompose(serial: f64, sec: &Section, date1904: bool) -> Option<Parts> {
    if !serial.is_finite() || serial.abs() > MAX_SERIAL {
        return None;
    }
    // An elapsed section is a duration, so a negative value is meaningful only
    // as a magnitude; a calendar section has no rendering for a negative serial.
    let signed = serial;
    let serial = if sec.elapsed.is_some() {
        serial.abs()
    } else {
        if serial < 0.0 {
            return None;
        }
        serial
    };
    let unit = round_unit(&sec.toks);
    let serial = serial + unit / 2.0;

    let days = serial.floor();
    let mut frac = serial - days;
    let mut days = days as i64;
    // Rounding can push the time of day to exactly 1.0 only through the
    // half-unit add above; normalize so the hour field never reads 24.
    if frac >= 1.0 {
        frac -= 1.0;
        days += 1;
    }
    let total_secs = frac * 86400.0;
    let mut isecs = total_secs.floor() as i64;
    let sub = total_secs - isecs as f64;
    if isecs >= 86400 {
        isecs -= 86400;
        days += 1;
    }

    let (h, mi, s) = (
        (isecs / 3600) as u32,
        ((isecs % 3600) / 60) as u32,
        (isecs % 60) as u32,
    );

    // Elapsed sections never touch the calendar: the whole serial is a duration.
    if let Some(u) = sec.elapsed {
        let total = signed.abs() + unit / 2.0;
        let secs_total = (total * 86400.0).floor().max(0.0);
        let elapsed = match u {
            Unit::Hour => secs_total / 3600.0,
            Unit::Minute => secs_total / 60.0,
            Unit::Second => secs_total,
        }
        .floor() as u64;
        // Fields below the elapsed unit are the remainder within it.
        let rem = secs_total as u64;
        let (h, mi, s) = match u {
            Unit::Hour => (0, (rem % 3600) / 60, rem % 60),
            Unit::Minute => (0, 0, rem % 60),
            Unit::Second => (0, 0, 0),
        };
        return Some(Parts {
            y: 0,
            mo: 1,
            d: 1,
            dow: 0,
            h: h as u32,
            mi: mi as u32,
            s: s as u32,
            sub,
            elapsed,
        });
    }

    let unix = if date1904 {
        days + EPOCH_1904
    } else if days == PHANTOM_LEAP {
        // The fictional day: report it verbatim. Its weekday comes from the
        // unadjusted arithmetic, which lands on the real 1900-02-28.
        let z = days + EPOCH_1900;
        return Some(Parts {
            y: 1900,
            mo: 2,
            d: 29,
            dow: weekday_from_days(z),
            h,
            mi,
            s,
            sub,
            elapsed: 0,
        });
    } else if days < PHANTOM_LEAP {
        // Before the phantom day the sequence is one day *ahead* of the epoch
        // that fits serials >= 61.
        days + EPOCH_1900 + 1
    } else {
        days + EPOCH_1900
    };
    let (y, mo, d) = civil_from_days(unix);
    Some(Parts {
        y,
        mo,
        d,
        dow: weekday_from_days(unix),
        h,
        mi,
        s,
        sub,
        elapsed: 0,
    })
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn pad2(out: &mut String, v: u64, width: u8) {
    if width >= 2 && v < 10 {
        out.push('0');
    }
    push_u64(out, v);
}

fn push_u64(out: &mut String, v: u64) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut v = v;
    if v == 0 {
        out.push('0');
        return;
    }
    while v > 0 && n < buf.len() {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in (0..n).rev() {
        out.push(buf[i] as char);
    }
}

fn render_date(sec: &Section, value: f64, date1904: bool) -> String {
    let p = match decompose(value, sec, date1904) {
        Some(p) => p,
        None => return OVERFLOW.to_string(),
    };
    let hour12 = if sec.twelve_hour {
        let h = p.h % 12;
        if h == 0 {
            12
        } else {
            h
        }
    } else {
        p.h
    };
    let mut out = String::with_capacity(24);
    for t in &sec.toks {
        match t {
            Tok::Lit(l) => out.push_str(l),
            Tok::Skip => out.push(' '),
            Tok::Fill => {}
            Tok::Slash => out.push('/'),
            Tok::Year(4) => push_u64(&mut out, p.y.rem_euclid(10000) as u64),
            Tok::Year(_) => pad2(&mut out, p.y.rem_euclid(100) as u64, 2),
            Tok::Month(5) => {
                // Single-letter month: the first char of the name, which is why
                // J/M/A repeat. Excel does the same.
                let name = MONTHS[(p.mo as usize).clamp(1, 12) - 1];
                out.push(name.chars().next().unwrap_or('?'));
            }
            Tok::Month(4) => out.push_str(MONTHS[(p.mo as usize).clamp(1, 12) - 1]),
            Tok::Month(3) => {
                let name = MONTHS[(p.mo as usize).clamp(1, 12) - 1];
                out.push_str(&name[..3]);
            }
            Tok::Month(w) => pad2(&mut out, p.mo as u64, *w),
            Tok::Day(4) => out.push_str(DAYS[(p.dow as usize).min(6)]),
            Tok::Day(3) => out.push_str(&DAYS[(p.dow as usize).min(6)][..3]),
            Tok::Day(w) => pad2(&mut out, p.d as u64, *w),
            Tok::Hour(w) => pad2(&mut out, hour12 as u64, *w),
            Tok::Minute(w) => pad2(&mut out, p.mi as u64, *w),
            Tok::Second(w) => pad2(&mut out, p.s as u64, *w),
            Tok::Elapsed(_, w) => pad2(&mut out, p.elapsed, *w),
            Tok::Ampm(long) => {
                let pm = p.h >= 12;
                out.push_str(match (long, pm) {
                    (true, false) => "AM",
                    (true, true) => "PM",
                    (false, false) => "A",
                    (false, true) => "P",
                });
            }
            Tok::SubSec(n) => {
                let n = (*n).clamp(1, 9) as i32;
                // Floor, not round: `decompose` already added half of this
                // unit, so flooring here rounds correctly *and* cannot carry
                // into a 10th digit that would print as ".10".
                let scaled = (p.sub * 10f64.powi(n)).floor().max(0.0) as u64;
                out.push('.');
                let s = {
                    let mut tmp = String::new();
                    push_u64(&mut tmp, scaled);
                    tmp
                };
                for _ in s.len()..n as usize {
                    out.push('0');
                }
                out.push_str(&s);
            }
            // Numeric-only tokens inside a date section: Excel prints a literal
            // for punctuation-like ones and ignores placeholders.
            Tok::Dot => out.push('.'),
            Tok::Comma => out.push(','),
            Tok::Percent => out.push('%'),
            Tok::Digit(_) | Tok::Exp(_) | Tok::At => {}
        }
    }
    out
}

// ── General ──────────────────────────────────────────────────────────────────

/// Excel's General: the shortest representation that round-trips, integers
/// without a decimal point, and scientific notation outside Excel's 11-digit
/// display window.
fn render_general(value: f64) -> String {
    if !value.is_finite() {
        return OVERFLOW.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let mag = value.abs();
    if mag >= 1e11 || mag < 1e-4 {
        return sci_string(value, 5, true, 2);
    }
    // Rust's `{}` for f64 is the shortest round-tripping form, which is exactly
    // General's intent, and it already omits a trailing `.0` for integers.
    let mut s = format!("{}", value);
    // Round-tripping can need 17 significant digits; General shows at most 11,
    // so re-render anything longer at that precision.
    if s.trim_start_matches(['-', '0', '.']).trim_matches('.').len() > 11 {
        let digits = 10 - mag.log10().floor() as i32;
        s = trim_zeros(&format!("{:.*}", digits.clamp(0, 15) as usize, value));
    }
    s
}

fn trim_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let t = s.trim_end_matches('0');
    t.trim_end_matches('.').to_string()
}

fn sci_string(value: f64, mantissa_decimals: usize, plus: bool, exp_width: usize) -> String {
    let mag = value.abs();
    let mut exp = if mag == 0.0 {
        0
    } else {
        mag.log10().floor() as i32
    };
    let mut mant = if mag == 0.0 { 0.0 } else { mag / 10f64.powi(exp) };
    // log10 of a value that is exactly a power of ten can land just below it,
    // leaving a mantissa of 10.0; normalize instead of printing "10.00E+1".
    let round_up = 10.0 - 0.5 * 10f64.powi(-(mantissa_decimals as i32));
    if mant >= round_up {
        mant /= 10.0;
        exp += 1;
    } else if mant < 1.0 && mag != 0.0 {
        mant *= 10.0;
        exp -= 1;
    }
    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    out.push_str(&trim_zeros(&format!("{:.*}", mantissa_decimals, mant)));
    out.push('E');
    if exp < 0 {
        out.push('-');
    } else if plus {
        out.push('+');
    }
    let mut e = String::new();
    push_u64(&mut e, exp.unsigned_abs() as u64);
    for _ in e.len()..exp_width {
        out.push('0');
    }
    out.push_str(&e);
    out
}

// ── numbers ──────────────────────────────────────────────────────────────────

/// Significant digits Excel keeps before it rounds for display. Rounding the
/// f64 directly gives the wrong answer on the cases users notice: 2.675 is
/// really 2.67499999999999982, so binary-nearest rounding to two places yields
/// 2.67 while Excel shows 2.68. Excel first collapses the value to 15
/// significant decimal digits — where it reads as exactly 2.675 — and rounds
/// that half-up.
const DISPLAY_SIG: i32 = 15;

/// Decimal digits of `value` rounded to `dec` places for display, as
/// `(int_digits, dec_digits)` strings without a sign.
fn split_digits(value: f64, dec: usize) -> (String, String) {
    let v = value.abs();
    if !v.is_finite() {
        return ("0".to_string(), "0".repeat(dec.min(17)));
    }
    let exp = if v == 0.0 {
        0
    } else {
        v.log10().floor() as i32
    };
    // Guard digits past `dec` are what makes half-up rounding meaningful; too
    // many and the binary noise comes back, hence the fixed significand width.
    let guard = (DISPLAY_SIG - 1 - exp).clamp(0, 25) as usize;
    let s = format!("{:.*}", guard, v);
    round_half_up(&s, dec.min(17))
}

/// Rounds a plain decimal string (`"123.4500"`, no sign, no exponent) to `dec`
/// fractional digits, half away from zero, by digit carry rather than by going
/// back through f64.
fn round_half_up(s: &str, dec: usize) -> (String, String) {
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let mut digits: Vec<u8> = int_part.bytes().filter(u8::is_ascii_digit).collect();
    if digits.is_empty() {
        digits.push(b'0');
    }
    let frac: Vec<u8> = frac_part.bytes().filter(u8::is_ascii_digit).collect();
    for i in 0..dec {
        digits.push(*frac.get(i).unwrap_or(&b'0'));
    }
    let round_up = frac.get(dec).map(|d| *d >= b'5').unwrap_or(false);
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    // A carry past the leading digit lengthens the integer side by one, so the
    // split point is derived from the final length rather than the original.
    let split = digits.len() - dec;
    let int_str = String::from_utf8_lossy(&digits[..split]).into_owned();
    let dec_str = String::from_utf8_lossy(&digits[split..]).into_owned();
    // Strip the leading zeros a carry cannot have needed, keeping one digit.
    let trimmed = int_str.trim_start_matches('0');
    let int_str = if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    };
    (int_str, dec_str)
}

fn group_thousands(digits: &str) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / 3);
    for (idx, c) in digits.chars().enumerate() {
        if idx > 0 && (n - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn render_number(sec: &Section, value: f64, force_sign: bool) -> String {
    let spec = &sec.num;
    if !value.is_finite() {
        return OVERFLOW.to_string();
    }
    let mut v = value.abs();
    for _ in 0..spec.percent.min(8) {
        v *= 100.0;
    }
    for _ in 0..spec.scale.min(8) {
        v /= 1000.0;
    }

    // Scientific: normalize first, then the mantissa runs through the same
    // placeholder machinery as a plain number.
    let (int_digits, dec_digits, exp_part) = if spec.sci {
        let mut exp = if v == 0.0 {
            0i32
        } else {
            v.abs().log10().floor() as i32
        };
        // `##0.0E+0` asks for up to 3 mantissa integer digits (engineering
        // notation): step the exponent down to a multiple of that width.
        let width = spec.int_all.clamp(1, 9) as i32;
        if width > 1 {
            exp -= exp.rem_euclid(width);
        }
        let mant = if v == 0.0 { 0.0 } else { v / 10f64.powi(exp) };
        let (i, d) = split_digits(mant, spec.dec_places);
        let mut e = String::new();
        if exp < 0 {
            e.push('-');
        } else if spec.exp_plus {
            e.push('+');
        }
        let mut digits = String::new();
        push_u64(&mut digits, exp.unsigned_abs() as u64);
        for _ in digits.len()..spec.exp_places {
            e.push('0');
        }
        e.push_str(&digits);
        (i, d, e)
    } else {
        let (i, d) = split_digits(v, spec.dec_places);
        (i, d, String::new())
    };

    // Integer side: a zero integer part vanishes unless a `0` placeholder
    // demands it — `.00` on 0.5 shows `.50`, and `#` on 0 shows nothing at all.
    let mut int_str = int_digits;
    if int_str == "0" && spec.int_zeros == 0 {
        int_str.clear();
    }
    while int_str.len() < spec.int_zeros {
        int_str.insert(0, '0');
    }
    if spec.thousands && !int_str.is_empty() {
        int_str = group_thousands(&int_str);
    }
    while int_str.len() < spec.int_pad {
        int_str.insert(0, ' ');
    }

    // Decimal side: `#` placeholders release trailing zeros, `?` replaces them
    // with alignment spaces.
    let mut dec_str = dec_digits;
    while dec_str.len() > spec.dec_zeros && dec_str.ends_with('0') {
        dec_str.pop();
    }
    while dec_str.len() < spec.dec_pad {
        dec_str.push(' ');
    }

    let mut out = String::with_capacity(sec.toks.len() + int_str.len() + dec_str.len() + 4);
    if force_sign {
        out.push('-');
    }
    // Digits are dealt across the placeholders rather than dumped at the first
    // one, because literals may sit *between* placeholders: `000-0000` on
    // 5551234 is `555-1234`, and Excel's rule is that the leftmost placeholder
    // absorbs every digit that does not fit the ones to its right. Both sides
    // are ASCII by construction (digits, grouping commas, pad spaces), so byte
    // slicing is safe.
    let mut int_slot = 0usize;
    let mut int_pos = 0usize;
    let mut dec_slot = 0usize;
    let mut dec_pos = 0usize;
    let mut past_dot = false;
    let mut exp_done = false;
    for t in &sec.toks {
        match t {
            Tok::Lit(l) => out.push_str(l),
            Tok::Skip => out.push(' '),
            Tok::Fill => {}
            Tok::Digit(_) => {
                if past_dot {
                    // Fractional digits fill left to right; the last
                    // placeholder takes whatever is left.
                    let take = if dec_slot + 1 >= spec.dec_places {
                        dec_str.len() - dec_pos
                    } else {
                        1
                    };
                    let end = (dec_pos + take).min(dec_str.len());
                    out.push_str(&dec_str[dec_pos..end]);
                    dec_pos = end;
                    dec_slot += 1;
                } else {
                    let take = if int_slot == 0 {
                        int_str.len().saturating_sub(spec.int_all.saturating_sub(1))
                    } else {
                        1
                    };
                    let end = (int_pos + take).min(int_str.len());
                    out.push_str(&int_str[int_pos..end]);
                    int_pos = end;
                    int_slot += 1;
                }
            }
            Tok::Dot => {
                // Integer digits are never dropped, even by a code with no
                // integer placeholders at all (`.00` on 42.5 is `42.50`).
                if int_slot == 0 {
                    out.push_str(&int_str);
                    int_pos = int_str.len();
                }
                past_dot = true;
                // Suppress a dangling separator when every decimal placeholder
                // was optional and released.
                if !dec_str.is_empty() {
                    out.push('.');
                }
            }
            Tok::Comma => {
                // Grouping and scaling commas are both consumed by the specs;
                // only a comma with no digits at all is a literal.
                if !spec.has_digits {
                    out.push(',');
                }
            }
            Tok::Percent => out.push('%'),
            Tok::Exp(_) => {
                if int_slot == 0 {
                    out.push_str(&int_str);
                    int_pos = int_str.len();
                }
                if !exp_done {
                    exp_done = true;
                    out.push('E');
                    out.push_str(&exp_part);
                }
            }
            Tok::Slash => out.push('/'),
            Tok::At => out.push_str(&render_general(value)),
            // Date tokens cannot appear: their presence makes the section a
            // date section.
            _ => {}
        }
    }
    if int_pos == 0 && dec_pos == 0 && spec.has_digits {
        out.push_str(&int_str);
    }
    out
}

// ── fractions ────────────────────────────────────────────────────────────────

/// Best `n/d` approximation of `v` in [0, 1) with `d <= max_den`, by direct
/// search. Bounded by `max_den`, which the parser caps at 4 placeholder digits.
fn best_fraction(v: f64, max_den: u32) -> (u64, u32) {
    let mut best = (0u64, 1u32);
    let mut best_err = f64::INFINITY;
    for d in 1..=max_den.max(1) {
        let n = (v * d as f64).round();
        let err = (v - n / d as f64).abs();
        if err < best_err - 1e-12 {
            best_err = err;
            best = (n.max(0.0) as u64, d);
            if err == 0.0 {
                break;
            }
        }
    }
    best
}

fn render_fraction(sec: &Section, value: f64, force_sign: bool) -> String {
    let spec = &sec.frac;
    if !value.is_finite() {
        return OVERFLOW.to_string();
    }
    let av = value.abs();
    let (whole, rem) = if spec.has_whole {
        (av.floor(), av - av.floor())
    } else {
        (0.0, av)
    };
    let max_den = spec
        .den_fixed
        .unwrap_or_else(|| 10u32.saturating_pow(spec.den_places.clamp(1, 4) as u32) - 1);
    let (mut num, mut den) = match spec.den_fixed {
        Some(d) => ((rem * d as f64).round().max(0.0) as u64, d),
        None => best_fraction(rem, max_den),
    };
    let mut whole = whole;
    // A remainder that rounds to 1 belongs in the whole part, not as `4/4`.
    if den > 0 && num >= den as u64 {
        if spec.has_whole {
            whole += (num / den as u64) as f64;
            num %= den as u64;
        } else {
            num = den as u64;
        }
    }
    if num == 0 {
        den = 1;
    }

    let mut out = String::new();
    if force_sign {
        out.push('-');
    }
    // A zero whole part is dropped entirely (separator included) unless a `0`
    // placeholder forces it: `# ?/?` on 0.75 is `3/4`, `0 ?/?` is `0 3/4`.
    let show_whole = spec.has_whole && (whole != 0.0 || spec.whole_forced);
    if show_whole {
        push_u64(&mut out, whole as u64);
    }
    // With no remainder Excel blanks the fraction rather than printing `0/1`,
    // keeping the column aligned.
    if num == 0 && spec.has_whole {
        if !show_whole {
            out.push('0');
        }
        for _ in 0..spec.num_places + 1 + spec.den_places.max(1) + 1 {
            out.push(' ');
        }
        return out;
    }
    if show_whole {
        out.push(' ');
    }
    let mut n = String::new();
    push_u64(&mut n, num);
    for _ in n.len()..spec.num_places {
        out.push(' ');
    }
    out.push_str(&n);
    out.push('/');
    let mut d = String::new();
    push_u64(&mut d, den as u64);
    out.push_str(&d);
    for _ in d.len()..spec.den_places {
        out.push(' ');
    }
    out
}

// ── apply ────────────────────────────────────────────────────────────────────

impl Format {
    pub fn apply(&self, value: f64) -> String {
        self.apply_with(value, self.date1904)
    }

    /// `apply` with the date system overridden, for callers that hold the
    /// workbook flag separately from the parsed format.
    pub fn apply_with(&self, value: f64, date1904: bool) -> String {
        let sec = match self.select(value) {
            Some(s) => s,
            None => return render_general(value),
        };
        if sec.is_empty() {
            // An empty section deliberately hides the value (`0;;` and friends).
            return String::new();
        }
        // Only a single-section format renders the sign itself; with two or more
        // sections the negative section carries its own minus or parentheses,
        // and Excel prints no sign when it carries neither.
        let force_sign = self.sections.len() == 1 && value < 0.0;
        match sec.kind {
            Kind::General => {
                let mut s = render_general(value.abs());
                if value < 0.0 {
                    s.insert(0, '-');
                }
                s
            }
            Kind::Date => render_date(sec, value, date1904),
            Kind::Fraction => render_fraction(sec, value, force_sign),
            Kind::Number => render_number(sec, value, force_sign),
        }
    }

    /// Renders a text cell. Only the fourth section applies to text; without
    /// one, text passes through unchanged.
    pub fn apply_text(&self, s: &str) -> String {
        let sec = match self.sections.get(3) {
            Some(sec) => sec,
            None => return s.to_string(),
        };
        if sec.kind == Kind::General {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len() + 8);
        let mut placed = false;
        for t in &sec.toks {
            match t {
                Tok::Lit(l) => out.push_str(l),
                Tok::Skip => out.push(' '),
                Tok::At => {
                    out.push_str(s);
                    placed = true;
                }
                _ => {}
            }
        }
        // A text section with no `@` still shows the cell's text in Excel only
        // when it is otherwise empty; a literal-only section replaces it.
        if !placed && out.is_empty() {
            out.push_str(s);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(code: &str, v: f64) -> String {
        Format::parse(code).apply(v)
    }

    fn f1904(code: &str, v: f64) -> String {
        Format::parse(code).with_date1904(true).apply(v)
    }

    // ── serial dates ─────────────────────────────────────────────────────────

    #[test]
    fn serial_converts_to_the_calendar_date() {
        // 2025-01-01 is serial 45658 (a widely published anchor), so 45678 is 20
        // days later. This is the headline bug: without the format code a date
        // cell renders as this integer.
        assert_eq!(f("yyyy-mm-dd", 45678.0), "2025-01-21");
        assert_eq!(f("yyyy-mm-dd", 45658.0), "2025-01-01");
        assert_eq!(f("yyyy-mm-dd", 1.0), "1900-01-01");
        assert_eq!(f("yyyy-mm-dd", 2.0), "1900-01-02");
        // Time of day is truncated, not rounded, by a date-only format.
        assert_eq!(f("yyyy-mm-dd", 45678.99), "2025-01-21");
    }

    #[test]
    fn nineteen_hundred_leap_year_bug_is_reproduced() {
        // Excel believes 1900-02-29 existed. Serials at or below 59 therefore
        // need a different epoch offset than those from 61 up, and serial 60 is
        // the fictional day itself.
        assert_eq!(f("yyyy-mm-dd", 59.0), "1900-02-28");
        assert_eq!(f("yyyy-mm-dd", 60.0), "1900-02-29");
        assert_eq!(f("yyyy-mm-dd", 61.0), "1900-03-01");
        // Both sides of the discontinuity must still agree with real calendars:
        // 1900-03-01 through 1900-03-31 is 30 days later.
        assert_eq!(f("yyyy-mm-dd", 91.0), "1900-03-31");
    }

    #[test]
    fn date1904_shifts_the_epoch() {
        assert_eq!(f1904("yyyy-mm-dd", 0.0), "1904-01-01");
        // 1904 is a leap year and 1905-07 are not: 366 + 3*365 = 1461 days.
        assert_eq!(f1904("yyyy-mm-dd", 1461.0), "1908-01-01");
        // The 1904 system has no phantom day, so serial 60 is a real date.
        assert_eq!(f1904("yyyy-mm-dd", 60.0), "1904-03-01");
        // The same serial is 1462 days apart between the two systems.
        assert_eq!(f1904("yyyy-mm-dd", 45678.0 - 1462.0), "2025-01-21");
    }

    #[test]
    fn negative_and_absurd_serials_degrade_instead_of_wrapping() {
        assert_eq!(f("yyyy-mm-dd", -1.0), OVERFLOW);
        assert_eq!(f("yyyy-mm-dd", 1e300), OVERFLOW);
        assert_eq!(f("yyyy-mm-dd", f64::NAN), OVERFLOW);
        assert_eq!(f("0.00", f64::INFINITY), OVERFLOW);
    }

    // ── date/time tokens ─────────────────────────────────────────────────────

    #[test]
    fn time_of_day_comes_from_the_fraction() {
        assert_eq!(f("h:mm:ss", 0.5), "12:00:00");
        assert_eq!(f("h:mm:ss", 45678.75), "18:00:00");
        assert_eq!(f("hh:mm:ss", 0.25), "06:00:00");
        // 13:45:30 = 13/24 + 45/1440 + 30/86400
        let t = 13.0 / 24.0 + 45.0 / 1440.0 + 30.0 / 86400.0;
        assert_eq!(f("h:mm:ss", t), "13:45:30");
        // Excel rounds to the finest field it prints, so a displayed minute
        // absorbs the 30 seconds rather than truncating them.
        assert_eq!(f("hh:mm", t), "13:46");
    }

    #[test]
    fn twelve_hour_tokens_switch_the_hour_field() {
        let afternoon = 13.0 / 24.0 + 45.0 / 1440.0;
        assert_eq!(f("h:mm AM/PM", afternoon), "1:45 PM");
        assert_eq!(f("h:mm A/P", afternoon), "1:45 P");
        assert_eq!(f("h:mm AM/PM", 1.0 / 24.0), "1:00 AM");
        // Midnight is 12 AM, not 0 AM.
        assert_eq!(f("h:mm AM/PM", 0.0), "12:00 AM");
        assert_eq!(f("h:mm AM/PM", 0.5), "12:00 PM");
        // Without AM/PM the same value stays on the 24-hour clock.
        assert_eq!(f("h:mm", afternoon), "13:45");
    }

    #[test]
    fn month_and_day_name_widths() {
        // 2025-01-21 was a Tuesday.
        assert_eq!(f("dddd", 45678.0), "Tuesday");
        assert_eq!(f("ddd", 45678.0), "Tue");
        assert_eq!(f("mmmm d, yyyy", 45678.0), "January 21, 2025");
        assert_eq!(f("d-mmm-yy", 45678.0), "21-Jan-25");
        assert_eq!(f("mmmmm", 45678.0), "J");
        assert_eq!(f("m/d/yy", 45678.0), "1/21/25");
        assert_eq!(f("dd", 45678.0), "21");
    }

    #[test]
    fn fractional_seconds_and_case_insensitivity() {
        let t = 13.0 / 24.0 + 45.0 / 1440.0 + 30.25 / 86400.0;
        assert_eq!(f("h:mm:ss.0", t), "13:45:30.2");
        assert_eq!(f("h:mm:ss.00", t), "13:45:30.25");
        assert_eq!(f("HH:MM:SS", t), "13:45:30");
        assert_eq!(f("YYYY-MM-DD", 45678.0), "2025-01-21");
    }

    #[test]
    fn seconds_round_and_carry_into_the_date() {
        // 23:59:59.6 with second-precision rounds to the next day's 00:00:00.
        let t = 45678.0 + (23.0 * 3600.0 + 59.0 * 60.0 + 59.6) / 86400.0;
        assert_eq!(f("yyyy-mm-dd h:mm:ss", t), "2025-01-22 0:00:00");
        // Minute precision rounds at the half minute.
        let t2 = 1.0 / 24.0 + 30.6 / 86400.0 * 60.0;
        assert_eq!(f("h:mm", t2), "1:31");
    }

    #[test]
    fn elapsed_tokens_pass_twenty_four_hours() {
        assert_eq!(f("[h]:mm", 1.5), "36:00");
        assert_eq!(f("[h]:mm", 0.5), "12:00");
        assert_eq!(f("[m]", 1.5), "2160");
        assert_eq!(f("[m]:ss", 1.0 / 24.0), "60:00");
        assert_eq!(f("[s]", 1.0 / 24.0), "3600");
        // Elapsed time reads a negative duration as a magnitude rather than
        // refusing it the way a calendar date does.
        assert_eq!(f("[h]:mm", -1.5), "36:00");
    }

    // ── the m ambiguity ──────────────────────────────────────────────────────

    #[test]
    fn m_is_minutes_next_to_an_hour_or_second_token() {
        let t = 13.0 / 24.0 + 45.0 / 1440.0 + 30.0 / 86400.0;
        // No seconds field, so the minute rounds; the point of the assertion is
        // that `m` became a minute at all (a month would print 12 or 1).
        assert_eq!(f("h:m", t), "13:46");
        assert_eq!(f("h:mm:ss", t), "13:45:30");
        // Builtin 45 is mm:ss, where the leading mm has no hour before it and
        // must still be minutes because seconds follow.
        assert_eq!(f("mm:ss", t), "45:30");
        // Elapsed hours count as an hour token for the adjacency rule. A single
        // `m` is unpadded, so the zero minute is one digit.
        assert_eq!(f("[h]:m", 1.5), "36:0");
        assert_eq!(f("[h]:mm", 1.5), "36:00");
    }

    #[test]
    fn m_is_months_next_to_date_tokens() {
        assert_eq!(f("m/d", 45678.0), "1/21");
        assert_eq!(f("yyyy-m-d", 45678.0), "2025-1-21");
        assert_eq!(f("mm-dd-yy", 45678.0), "01-21-25");
        // Both meanings in one code: month before the time, minute after it.
        assert_eq!(f("m/d/yy h:mm", 45678.5), "1/21/25 12:00");
        // Three or more m's are always the month name, adjacency regardless.
        assert_eq!(f("h:mmm", 45678.0 + 13.0 / 24.0), "13:Jan");
    }

    // ── numeric tokens ───────────────────────────────────────────────────────

    #[test]
    fn thousands_and_decimals() {
        assert_eq!(f("#,##0.00", 1234.5), "1,234.50");
        assert_eq!(f("#,##0.00", 0.0), "0.00");
        assert_eq!(f("#,##0", 1234567.0), "1,234,567");
        assert_eq!(f("0", 42.4), "42");
        assert_eq!(f("0", 42.6), "43");
        assert_eq!(f("0.000", 1.5), "1.500");
        // `#` releases trailing zeros, `0` forces them.
        assert_eq!(f("0.0#", 1.5), "1.5");
        assert_eq!(f("0.0#", 1.56), "1.56");
        assert_eq!(f("#", 0.0), "");
        assert_eq!(f(".00", 0.5), ".50");
        // `?` holds the width with a space.
        assert_eq!(f("0.0?", 1.5), "1.5 ");
        assert_eq!(f("???0", 5.0), "   5");
    }

    #[test]
    fn percent_multiplies_by_a_hundred() {
        assert_eq!(f("0.00%", 0.1234), "12.34%");
        assert_eq!(f("0%", 0.5), "50%");
        assert_eq!(f("0.0%", 1.0), "100.0%");
    }

    #[test]
    fn trailing_comma_scales_by_a_thousand() {
        assert_eq!(f("#,##0,", 1234567.0), "1,235");
        assert_eq!(f("0.0,,", 2_500_000.0), "2.5");
        // A comma between placeholders groups instead of scaling.
        assert_eq!(f("#,##0", 1234.0), "1,234");
    }

    #[test]
    fn scientific_notation() {
        assert_eq!(f("0.00E+00", 12345.0), "1.23E+04");
        assert_eq!(f("0.00E+00", 0.00012), "1.20E-04");
        assert_eq!(f("0.00E-00", 12345.0), "1.23E04");
        // Engineering notation: `##0` asks for up to three mantissa digits, so
        // the exponent steps in threes.
        assert_eq!(f("##0.0E+0", 12345.0), "12.3E+3");
    }

    #[test]
    fn rounding_at_the_decimal_boundary() {
        // Rounds on the decimal representation, the direction Excel displays.
        assert_eq!(f("0.00", 0.005), "0.01");
        assert_eq!(f("0.00", 2.675), "2.68");
        assert_eq!(f("0", 0.5), "1");
        assert_eq!(f("0", 1.5), "2");
        // Rounding that carries into a new integer digit must still group.
        assert_eq!(f("#,##0", 999.5), "1,000");
        assert_eq!(f("#,##0.0", 999.99), "1,000.0");
    }

    // ── sections ─────────────────────────────────────────────────────────────

    #[test]
    fn one_section_signs_negatives_itself() {
        assert_eq!(f("0.00", -5.0), "-5.00");
        assert_eq!(f("#,##0", -1234.0), "-1,234");
    }

    #[test]
    fn two_sections_give_the_negative_its_own_rendering() {
        // The negative section formats the absolute value and supplies its own
        // sign, so it can use parentheses instead.
        assert_eq!(f("0.00;(0.00)", -5.0), "(5.00)");
        assert_eq!(f("0.00;(0.00)", 5.0), "5.00");
        // A negative section without a sign shows an unsigned number, which is
        // Excel's behaviour and not a bug here.
        assert_eq!(f("0.00;0.00", -5.0), "5.00");
        assert_eq!(f("0.00;-0.00", -5.0), "-5.00");
    }

    #[test]
    fn four_sections_split_positive_negative_zero_and_text() {
        let fm = Format::parse("#,##0.00;(#,##0.00);\"nil\";\"café \"@");
        assert_eq!(fm.apply(1234.5), "1,234.50");
        assert_eq!(fm.apply(-1234.5), "(1,234.50)");
        assert_eq!(fm.apply(0.0), "nil");
        assert_eq!(fm.apply_text("naïve"), "café naïve");
        // Three sections: zero gets its own, text falls through untouched.
        let fm3 = Format::parse("0.00;-0.00;\"zero\"");
        assert_eq!(fm3.apply(0.0), "zero");
        assert_eq!(fm3.apply_text("example.org"), "example.org");
    }

    #[test]
    fn an_empty_section_hides_the_value() {
        assert_eq!(f("0.00;", -5.0), "");
        assert_eq!(f("0.00;", 5.0), "5.00");
        assert_eq!(f("0.00;-0.00;", 0.0), "");
        assert_eq!(f("0.00;-0.00;", 1.0), "1.00");
    }

    // ── literals, escapes, colours ───────────────────────────────────────────

    #[test]
    fn literals_escapes_and_bracket_prefixes() {
        assert_eq!(f("\"café \"0", 3.0), "café 3");
        // Digits are dealt across placeholders separated by a literal, which
        // is what makes `000-0000` work as a phone-number format.
        assert_eq!(f("0\\-0", 42.0), "4-2");
        assert_eq!(f("000-0000", 5551234.0), "555-1234");
        assert_eq!(f("000-0000", 12.0), "000-0012");
        // Locale and currency brackets are skipped, never printed.
        assert_eq!(f("[$-409]yyyy-mm-dd", 45678.0), "2025-01-21");
        assert_eq!(f("[$€-407]#,##0.00", 1234.5), "1,234.50");
        // `_x` reserves a width (rendered as a space); `*x` fills nothing.
        assert_eq!(f("0_)", 5.0), "5 ");
        assert_eq!(f("*-0", 5.0), "5");
        // An unterminated quote run ends at the code's end instead of panicking.
        assert_eq!(f("\"café", 5.0), "café");
    }

    #[test]
    fn colors_come_from_the_selected_section() {
        let fm = Format::parse("#,##0;[Red]-#,##0");
        assert_eq!(fm.color(5.0), None);
        assert_eq!(fm.color(-5.0), Some("#ff0000"));
        assert_eq!(fm.apply(-5.0), "-5");
        assert_eq!(Format::parse("[Blue]0").color(1.0), Some("#0000ff"));
        assert_eq!(Format::parse("[Color 3]0").color(1.0), Some("#ff0000"));
        // An unmapped palette index falls back to no colour rather than a wrong
        // one, and the format still renders.
        let fm = Format::parse("[Color 42]0.0");
        assert_eq!(fm.color(1.0), None);
        assert_eq!(fm.apply(1.25), "1.3");
    }

    // ── fractions ────────────────────────────────────────────────────────────

    #[test]
    fn fractions_render_as_fractions() {
        assert_eq!(f("# ?/?", 2.5), "2 1/2");
        assert_eq!(f("# ?/?", 2.25), "2 1/4");
        // `??` right-aligns the numerator, so a one-digit numerator keeps its
        // alignment space.
        assert_eq!(f("# ??/??", 0.3), " 3/10");
        // A zero whole part is dropped when only `#` asks for it.
        assert_eq!(f("# ?/?", 0.75), "3/4");
        assert_eq!(f("0 ?/?", 0.75), "0 3/4");
        // Fixed denominator.
        assert_eq!(f("# ?/16", 2.25), "2 4/16");
        // Negative values keep the sign in a single-section format.
        assert_eq!(f("# ?/?", -2.5), "-2 1/2");
    }

    // ── General ──────────────────────────────────────────────────────────────

    #[test]
    fn general_keeps_integers_integral() {
        assert_eq!(f("General", 42.0), "42");
        assert_eq!(f("General", 42.5), "42.5");
        assert_eq!(f("General", 0.0), "0");
        assert_eq!(f("General", -3.25), "-3.25");
        assert_eq!(f("general", 7.0), "7");
        // An unparseable code degrades to General, which must not add a `.0`.
        assert_eq!(f("", 42.0), "42");
        // Outside Excel's display window General switches to scientific.
        assert!(f("General", 1e20).starts_with("1E+"));
        assert!(f("General", 1e-9).starts_with("1E-"));
    }

    // ── robustness ───────────────────────────────────────────────────────────

    #[test]
    fn garbage_codes_do_not_panic() {
        let codes = [
            "[[[[",
            "\"\"\"",
            "\\",
            "_",
            "*",
            "///",
            ";;;;;;;;",
            "[Red",
            "[h",
            ".",
            "?/",
            "E+",
            "[$-",
            "0.00;;;;;;0",
            "mmmmmmmmmmmmmmmmmm",
            "yyyyyyyyyy",
            "[hhhhhh]",
            "########################",
            "0,,,,,,,,,,",
            "@@@@",
            "café naïve",
        ];
        for c in codes {
            let fm = Format::parse(c);
            for v in [0.0, 1.0, -1.0, 0.5, 45678.0, 1e15, -1e15, f64::NAN] {
                let _ = fm.apply(v);
                let _ = fm.color(v);
            }
            let _ = fm.apply_text("example.org");
            let _ = fm.is_date();
        }
    }

    #[test]
    fn oversized_codes_are_bounded() {
        // Past MAX_CODE the code is not parsed at all.
        let long = "0".repeat(MAX_CODE + 1);
        assert_eq!(Format::parse(&long).apply(42.0), "42");
        // Under MAX_CODE but past MAX_TOKENS: parsing stops, rendering still
        // produces something.
        let many = "#".repeat(MAX_TOKENS + 100);
        assert!(many.len() < MAX_CODE);
        assert_eq!(Format::parse(&many).apply(42.0), "42");
        // More sections than Excel defines: the extras are ignored.
        let fm = Format::parse("\"a\";\"b\";\"c\";\"d\";\"e\";\"f\"");
        assert_eq!(fm.apply(1.0), "a");
        assert_eq!(fm.apply(-1.0), "b");
        assert_eq!(fm.apply(0.0), "c");
        assert_eq!(fm.apply_text("café"), "d");
    }

    // ── built-ins ────────────────────────────────────────────────────────────

    #[test]
    fn builtin_ids_cover_the_implicit_table() {
        let general = Format::builtin(0).expect("id 0 exists");
        assert!(!general.is_date());
        assert_eq!(general.apply(42.0), "42");
        assert_eq!(general.apply(42.5), "42.5");

        let date = Format::builtin(14).expect("id 14 exists");
        assert!(date.is_date());
        assert_eq!(date.apply(45678.0), "1/21/2025");

        assert_eq!(Format::builtin(2).expect("id 2").apply(1.5), "1.50");
        assert_eq!(Format::builtin(4).expect("id 4").apply(1234.5), "1,234.50");
        assert_eq!(Format::builtin(9).expect("id 9").apply(0.5), "50%");
        assert_eq!(Format::builtin(10).expect("id 10").apply(0.1234), "12.34%");

        // Every date/time builtin must report itself as one, or the caller
        // renders serial numbers.
        for id in [14u16, 15, 16, 17, 18, 19, 20, 21, 22, 45, 46, 47] {
            let fm = Format::builtin(id).unwrap_or_else(|| panic!("id {} exists", id));
            assert!(fm.is_date(), "builtin {} should be a date/time", id);
        }
        // And no non-date builtin may claim to be one.
        for id in [0u16, 1, 2, 3, 4, 9, 10, 11, 12, 13, 37, 40, 43, 48, 49] {
            let fm = Format::builtin(id).unwrap_or_else(|| panic!("id {} exists", id));
            assert!(!fm.is_date(), "builtin {} should not be a date/time", id);
        }
        // 23-36 are locale-reserved and undefined here; so is anything above 49.
        for id in [23u16, 30, 36, 50, 164, 999] {
            assert!(Format::builtin(id).is_none(), "id {} must be None", id);
        }
    }

    #[test]
    fn builtin_negative_sections_and_colors() {
        // Id 38 is `#,##0_);[Red](#,##0)`: red parentheses for negatives.
        let fm = Format::builtin(38).expect("id 38");
        assert_eq!(fm.color(-5.0), Some("#ff0000"));
        assert_eq!(fm.color(5.0), None);
        assert_eq!(fm.apply(-1234.0), "(1,234)");
        assert_eq!(fm.apply(1234.0), "1,234 ");
        // Id 49 is `@`: text passes through, numbers render as General.
        let text = Format::builtin(49).expect("id 49");
        assert_eq!(text.apply_text("café"), "café");
        assert_eq!(text.apply(42.0), "42");
    }

    #[test]
    fn date_system_flag_round_trips() {
        let mut fm = Format::parse("yyyy-mm-dd");
        assert!(!fm.date1904());
        fm.set_date1904(true);
        assert!(fm.date1904());
        assert_eq!(fm.apply(0.0), "1904-01-01");
        // The explicit override wins over the stored flag.
        assert_eq!(fm.apply_with(1.0, false), "1900-01-01");
    }
}
