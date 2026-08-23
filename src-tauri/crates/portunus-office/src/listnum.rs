//! Ordered-list numerals: `(kind, n) → label`.
//!
//! The three generators below are the whole vocabulary every office format
//! numbers a list with; only the *spelling* of the format name differs.
//! DrawingML's `a:buAutoNum@type` is handled here by `auto_num` because it folds
//! the numeral and its punctuation into one token. Elsewhere the two are stated
//! separately and the generator is reached directly: docx `w:numFmt@val`
//! (`lowerLetter`/`upperLetter` → `alpha`, `lowerRoman`/`upperRoman` → `roman`,
//! `decimal` and its enclosed variants → `decimal`) takes its punctuation from
//! `w:lvlText`, and ODF `text:list-level-style-number@style:num-format`
//! (`a`/`A`/`i`/`I`/`1` → the same three) from `style:num-prefix`/`num-suffix`.

/// `a:buAutoNum@type` → the rendered label. Unknown types fall back to the
/// arabic-period form rather than dropping the number.
pub fn auto_num(ty: &str, n: u32) -> String {
    let (body, wrap) = match ty {
        t if t.starts_with("alphaLc") => (alpha(n, false), t),
        t if t.starts_with("alphaUc") => (alpha(n, true), t),
        t if t.starts_with("romanLc") => (roman(n, false), t),
        t if t.starts_with("romanUc") => (roman(n, true), t),
        t => (decimal(n), t),
    };
    if wrap.ends_with("ParenBoth") {
        format!("({body})")
    } else if wrap.ends_with("ParenR") {
        format!("{body})")
    } else if wrap.ends_with("Period") || wrap.ends_with("PeriodOne") {
        format!("{body}.")
    } else if wrap.ends_with("Dash") {
        format!("- {body}")
    } else {
        format!("{body}.")
    }
}

pub fn decimal(n: u32) -> String {
    n.to_string()
}

pub fn alpha(n: u32, upper: bool) -> String {
    let mut n = n.max(1);
    let base = if upper { b'A' } else { b'a' };
    let mut out = Vec::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        out.push(base + rem);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

pub fn roman(n: u32, upper: bool) -> String {
    const T: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n.min(3999);
    let mut out = String::new();
    for (v, s) in T {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    if upper {
        out.to_uppercase()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_num_formats_cover_the_common_types() {
        assert_eq!(auto_num("arabicPeriod", 3), "3.");
        assert_eq!(auto_num("arabicParenR", 3), "3)");
        assert_eq!(auto_num("alphaLcParenBoth", 2), "(b)");
        assert_eq!(auto_num("alphaUcPeriod", 27), "AA.");
        assert_eq!(auto_num("romanUcPeriod", 4), "IV.");
        assert_eq!(auto_num("romanLcPeriod", 9), "ix.");
        // Unknown types still number rather than dropping the marker.
        assert_eq!(auto_num("circleNumDbPlain", 5), "5.");
    }

    #[test]
    fn generators_are_reachable_without_a_format_name() {
        // The bounds a caller cannot state: zero is the first item, and roman
        // saturates rather than looping or emitting a thousand `m`s.
        assert_eq!(alpha(0, false), "a");
        assert_eq!(alpha(26, true), "Z");
        assert_eq!(roman(0, false), "");
        assert_eq!(roman(4000, true), roman(3999, true));
        assert_eq!(decimal(0), "0");
    }
}
