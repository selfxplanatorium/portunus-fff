//! Font handling for the office renderers: mapping Office font names to CSS
//! stacks of metric-compatible substitutes, and remapping the symbol fonts that
//! store their glyphs in the Private Use Area.

#![allow(dead_code)] // Consumed by the later-stage renderers.

/// Longest family name accepted. Real names are far shorter; the cap keeps a
/// pathological document from stuffing kilobytes into every `style` attribute.
const MAX_NAME: usize = 64;

/// Strips a document-supplied font name down to a safe family name.
///
/// Returns the cleaned name and whether anything was rejected. The whitelist is
/// letters (including non-Latin, so CJK family names survive), digits, space
/// and hyphen — every character an HTML or CSS escape could hinge on (quotes,
/// `;`, `{}`, `<>`, backslash, controls) falls outside it.
///
/// Scanning stops at the *first* offender instead of filtering offenders out:
/// stripping would smuggle an injection payload through as a plausible-looking
/// family name (`Arial";}</style>` → `Arialstyle`), while truncating keeps only
/// the part that was ever a real name.
fn sanitize(name: &str) -> (String, bool) {
    let mut out = String::new();
    let mut n = 0usize;
    let mut dirty = false;
    let mut last_space = false;
    for c in name.trim().chars() {
        if !(c.is_alphanumeric() || c == ' ' || c == '-') {
            dirty = true;
            break;
        }
        if n >= MAX_NAME {
            break;
        }
        if c == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(c);
        n += 1;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    (out, dirty)
}

/// Maps an Office font name to a CSS font stack.
///
/// Linux systems generally lack the Office fonts, so each known name resolves to
/// its metric-compatible clone first (same advance widths, so line breaks land
/// where the authoring app put them), then a near substitute, then a generic.
///
/// **This is a CSS injection sink**: `name` comes from untrusted document XML
/// and the result ends up inside a `style` attribute. The output is either one
/// of the fixed stacks below or a `sanitize`d name in double quotes — and
/// `sanitize` guarantees no quote can appear inside it, so the quoting cannot be
/// escaped from.
pub fn css_font_stack(name: &str) -> String {
    let (clean, dirty) = sanitize(name);
    if clean.is_empty() {
        return "sans-serif".to_string();
    }
    // A name that contained disallowed characters is not a real family name, so
    // it does not get dignified with a metric substitution — just the quoted
    // remnant and a generic fallback.
    if !dirty {
        let lower = clean.to_lowercase();
        if let Some(stack) = alias(&lower) {
            return stack.to_string();
        }
        // An unknown *condensed* family falls back to a narrow one rather than to
        // the default sans. The width difference is not cosmetic: a line authored
        // to fit a fixed slide box in a condensed face wraps in a normal-width
        // substitute, and the extra line is what pushes text out of its box.
        if is_condensed(&lower) {
            return format!(
                "\"{}\", \"Liberation Sans Narrow\", \"Arial Narrow\", \"DejaVu Sans Condensed\", sans-serif",
                clean
            );
        }
    }
    format!("\"{}\", sans-serif", clean)
}

/// Whether a family name says "narrow" in the way font vendors spell it. Matched
/// on word boundaries so a family that merely contains the letters (say
/// "Concorde") is not squeezed.
fn is_condensed(lower: &str) -> bool {
    lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(w, "cond" | "condensed" | "narrow" | "cn")
    })
}

fn alias(lower: &str) -> Option<&'static str> {
    Some(match lower {
        // Metric-compatible clones shipped by most distros (Carlito, Caladea,
        // Liberation, Croscore/Chrome OS core fonts, Gelasio, Selawik).
        "calibri" | "calibri light" | "aptos" | "aptos display" | "aptos narrow" | "carlito" => {
            "Carlito, Lato, sans-serif"
        }
        "cambria" | "caladea" => "Caladea, Georgia, serif",
        "cambria math" => "\"Latin Modern Math\", Caladea, serif",
        "arial" | "helvetica" | "helvetica neue" | "arial mt" => {
            "\"Liberation Sans\", Arimo, Helvetica, sans-serif"
        }
        "arial narrow" => "\"Liberation Sans Narrow\", \"Liberation Sans\", sans-serif",
        "arial black" => "\"Archivo Black\", \"Liberation Sans\", sans-serif",
        "times new roman" | "times" | "timesnewromanpsmt" => {
            "\"Liberation Serif\", Tinos, Times, serif"
        }
        "courier new" | "courier" => "\"Liberation Mono\", Cousine, monospace",
        "segoe ui" | "segoe ui light" | "segoe ui semibold" => {
            "Selawik, system-ui, sans-serif"
        }
        "georgia" => "Gelasio, Georgia, serif",
        "verdana" | "tahoma" => "\"DejaVu Sans\", Verdana, sans-serif",
        "consolas" | "cascadia code" | "cascadia mono" | "lucida console"
        | "lucida sans typewriter" => "\"Liberation Mono\", \"DejaVu Sans Mono\", monospace",
        "garamond" | "adobe garamond pro" => "\"EB Garamond\", Garamond, serif",
        "book antiqua" | "palatino" | "palatino linotype" => {
            "P052, \"URW Palladio L\", Palatino, serif"
        }
        "bookman old style" => "\"URW Bookman L\", \"Bookman Old Style\", serif",
        "century schoolbook" | "century" => "C059, \"Century Schoolbook L\", serif",
        "trebuchet ms" => "\"Trebuchet MS\", \"DejaVu Sans\", sans-serif",
        "comic sans ms" => "\"Comic Relief\", \"Comic Neue\", cursive",
        "impact" => "Impact, \"DejaVu Sans Condensed\", sans-serif",
        "candara" | "corbel" | "calibri body" => "\"DejaVu Sans\", sans-serif",
        "constantia" => "\"DejaVu Serif\", Georgia, serif",
        "franklin gothic book" | "franklin gothic medium" => {
            "\"Libre Franklin\", \"Liberation Sans\", sans-serif"
        }
        // CJK: the Noto CJK families are the near-universal Linux stand-ins.
        "ms gothic" | "ms pgothic" | "meiryo" | "yu gothic" | "microsoft yahei" | "simhei"
        | "malgun gothic" => "\"Noto Sans CJK JP\", \"Noto Sans CJK SC\", sans-serif",
        "ms mincho" | "ms pmincho" | "yu mincho" | "simsun" | "nsimsun" | "batang" => {
            "\"Noto Serif CJK JP\", \"Noto Serif CJK SC\", serif"
        }
        // Symbol fonts: the glyphs are remapped to real Unicode by `remap`, so
        // the stack only has to render ordinary text.
        "wingdings" | "wingdings 2" | "wingdings 3" | "webdings" | "symbol"
        | "monotype sorts" | "zapfdingbats" => "sans-serif",
        _ => return None,
    })
}

/// The family as stated, quoted, with a generic behind it — deliberately *not*
/// through [`alias`], which folds every symbol font onto plain `sans-serif`
/// because a substituted symbol face is tofu.
///
/// For the one case where that fold is wrong: a code point the remap tables do
/// not know (a docx `w:sym`). It means its glyph only in the font that stated it,
/// so a reader who has that font installed gets the glyph and everyone else gets
/// one missing character instead of missing text.
pub fn literal_font_stack(name: &str) -> String {
    let (clean, _) = sanitize(name);
    if clean.is_empty() {
        return "sans-serif".to_string();
    }
    format!("\"{}\", sans-serif", clean)
}

pub fn is_symbol_font(name: &str) -> bool {
    let (clean, _) = sanitize(name);
    matches!(
        clean.to_lowercase().as_str(),
        "wingdings"
            | "wingdings 2"
            | "wingdings 3"
            | "webdings"
            | "symbol"
            | "monotype sorts"
            | "zapfdingbats"
    )
}

// ── symbol-font remapping ────────────────────────────────────────────────────

// Wingdings and Symbol runs carry their glyphs as Private Use Area code points
// (U+F000 + the font's own byte), so rendering them with a substitute font
// yields tofu. The tables below fold the common glyphs onto real Unicode
// characters that any DejaVu/Noto install can draw. Unmapped code points return
// `None` so the caller can fall back (drop the run, keep the original text)
// rather than emit a replacement box.

// Word also emits these runs as plain Latin text with the font applied, rather
// than as PUA code points, so accept both encodings.
fn font_byte(ch: char) -> Option<u8> {
    let c = ch as u32;
    if (0xF000..=0xF0FF).contains(&c) {
        Some((c & 0xFF) as u8)
    } else if (0x20..=0xFF).contains(&c) {
        Some(c as u8)
    } else {
        None
    }
}

/// Wingdings (1) → Unicode. Deliberately partial: the bullets, check marks,
/// boxes and arrowheads that documents actually use for list markers.
pub fn wingdings(ch: char) -> Option<char> {
    Some(match font_byte(ch)? {
        // Bullets and geometric markers.
        0x6C => '●', // 'l'
        0x6D => '❍', // 'm'
        0x6E => '■', // 'n'
        0x6F => '□', // 'o'
        0x71 => '❑', // 'q'
        0x73 => '◆', // 's'
        0x75 => '◆',
        0x77 => '◆',
        0x9F => '•',
        0xA7 => '▪',
        0xA8 => '◻',
        0xB7 => '•',
        // Arrowheads / pointers used as list markers.
        0xD8 => '➢',
        0xD9 => '➢',
        0xF0 => '➔',
        // Plain arrows. Wingdings' own arrow block maps to Supplemental
        // Arrows-C (U+1F8xx), which substitute fonts do not cover, so fold onto
        // the BMP arrows instead.
        0xE0 => '⇦',
        0xE1 => '⇨',
        0xE2 => '⇧',
        0xE3 => '⇩',
        0xE8 => '←',
        0xE9 => '→',
        0xEA => '↑',
        0xEB => '↓',
        // Checks and ballot boxes.
        0xFB => '✗',
        0xFC => '✔',
        0xFD => '☒',
        0xFE => '☑',
        _ => return None,
    })
}

/// Adobe Symbol encoding → Unicode: the Greek alphabet plus the common math
/// operators. Symbol is what Word uses for `Σ`, `∞` and the default `•` bullet.
pub fn symbol_font(ch: char) -> Option<char> {
    Some(match font_byte(ch)? {
        // Uppercase Greek.
        b'A' => 'Α',
        b'B' => 'Β',
        b'C' => 'Χ',
        b'D' => 'Δ',
        b'E' => 'Ε',
        b'F' => 'Φ',
        b'G' => 'Γ',
        b'H' => 'Η',
        b'I' => 'Ι',
        b'J' => 'ϑ',
        b'K' => 'Κ',
        b'L' => 'Λ',
        b'M' => 'Μ',
        b'N' => 'Ν',
        b'O' => 'Ο',
        b'P' => 'Π',
        b'Q' => 'Θ',
        b'R' => 'Ρ',
        b'S' => 'Σ',
        b'T' => 'Τ',
        b'U' => 'Υ',
        b'V' => 'ς',
        b'W' => 'Ω',
        b'X' => 'Ξ',
        b'Y' => 'Ψ',
        b'Z' => 'Ζ',
        // Lowercase Greek.
        b'a' => 'α',
        b'b' => 'β',
        b'c' => 'χ',
        b'd' => 'δ',
        b'e' => 'ε',
        b'f' => 'φ',
        b'g' => 'γ',
        b'h' => 'η',
        b'i' => 'ι',
        b'j' => 'ϕ',
        b'k' => 'κ',
        b'l' => 'λ',
        b'm' => 'μ',
        b'n' => 'ν',
        b'o' => 'ο',
        b'p' => 'π',
        b'q' => 'θ',
        b'r' => 'ρ',
        b's' => 'σ',
        b't' => 'τ',
        b'u' => 'υ',
        b'v' => 'ϖ',
        b'w' => 'ω',
        b'x' => 'ξ',
        b'y' => 'ψ',
        b'z' => 'ζ',
        // Logic and set notation.
        0x22 => '∀',
        0x24 => '∃',
        0x40 => '≅',
        0x5C => '∴',
        0x5E => '⊥',
        0xC7 => '∩',
        0xC8 => '∪',
        0xC6 => '∅',
        0xCC => '⊂',
        0xCD => '⊆',
        0xCE => '∈',
        0xD8 => '¬',
        0xD9 => '∧',
        0xDA => '∨',
        // Arrows.
        0xAB => '↔',
        0xAC => '←',
        0xAD => '↑',
        0xAE => '→',
        0xAF => '↓',
        0xDB => '⇔',
        0xDC => '⇐',
        0xDD => '⇑',
        0xDE => '⇒',
        0xDF => '⇓',
        // Operators and misc.
        0xA2 => '′',
        0xA3 => '≤',
        0xA5 => '∞',
        0xB0 => '°',
        0xB1 => '±',
        0xB2 => '″',
        0xB3 => '≥',
        0xB4 => '×',
        0xB5 => '∝',
        0xB6 => '∂',
        0xB7 => '•',
        0xB8 => '÷',
        0xB9 => '≠',
        0xBA => '≡',
        0xBB => '≈',
        0xBC => '…',
        0xD0 => '∠',
        0xD1 => '∇',
        0xD5 => '∏',
        0xD6 => '√',
        0xD7 => '⋅',
        0xE5 => '∑',
        0xF2 => '∫',
        _ => return None,
    })
}

/// Dispatches on the run's font name.
///
/// Only Wingdings 1 and Symbol have tables. Wingdings 2/3, Webdings and
/// Monotype Sorts use entirely different layouts, so running them through the
/// Wingdings 1 table would confidently emit the *wrong* glyph — worse than the
/// caller's fallback.
pub fn remap(font: &str, ch: char) -> Option<char> {
    let (clean, _) = sanitize(font);
    match clean.to_lowercase().as_str() {
        "wingdings" => wingdings(ch),
        "symbol" => symbol_font(ch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_fonts_get_metric_clones_first() {
        let calibri = css_font_stack("Calibri");
        assert!(calibri.starts_with("Carlito"), "{}", calibri);
        assert!(
            calibri.find("Carlito").unwrap() < calibri.find("sans-serif").unwrap()
        );
        assert!(css_font_stack("Cambria").contains("Caladea"));
        assert!(css_font_stack("times new roman").contains("Liberation Serif"));
        assert!(css_font_stack("  Courier New  ").contains("Liberation Mono"));
        assert!(css_font_stack("Segoe UI").contains("Selawik"));
    }

    #[test]
    fn unknown_font_is_quoted_with_a_generic_fallback() {
        assert_eq!(
            css_font_stack("Widget Sans"),
            "\"Widget Sans\", sans-serif"
        );
        // Empty / all-rejected names fall back to the generic alone.
        assert_eq!(css_font_stack(""), "sans-serif");
        assert_eq!(css_font_stack("{}"), "sans-serif");
    }

    #[test]
    fn a_literal_stack_keeps_the_symbol_family_the_alias_table_folds_away() {
        // The whole point: `css_font_stack` substitutes a symbol font away, and an
        // unmapped `w:sym` code point needs the family it was written for.
        assert_eq!(css_font_stack("Wingdings"), "sans-serif");
        assert_eq!(literal_font_stack("Wingdings"), "\"Wingdings\", sans-serif");
        // Same sanitizing, so it cannot break out of the attribute either.
        assert_eq!(
            literal_font_stack("Wingdings\";}</style>"),
            "\"Wingdings\", sans-serif"
        );
        assert_eq!(literal_font_stack("{}"), "sans-serif");
        assert_eq!(literal_font_stack(""), "sans-serif");
    }

    #[test]
    fn css_font_stack_cannot_break_out_of_the_style_attribute() {
        let out = css_font_stack("Arial\";}</style><script>alert(1)</script>");
        assert_eq!(out, "\"Arial\", sans-serif");
        assert!(!out.contains(';'));
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(!out.contains('}'));
        // Quote count stays even: the only quotes are the ones we added.
        assert_eq!(out.matches('"').count(), 2);
        // Newlines and backslash escapes are rejected the same way.
        assert_eq!(css_font_stack("Widget\\27 ;color:red"), "\"Widget\", sans-serif");
        assert_eq!(css_font_stack("Widget\n</style>"), "\"Widget\", sans-serif");
    }

    #[test]
    fn unknown_condensed_families_fall_back_to_a_narrow_stack() {
        // A condensed face substituted by a normal-width one wraps lines the
        // author's fixed-size slide box was never sized for.
        let out = css_font_stack("Trade Gothic Next Cond");
        assert!(out.starts_with("\"Trade Gothic Next Cond\""), "{out}");
        assert!(out.contains("Liberation Sans Narrow"), "{out}");
        assert!(css_font_stack("Helvetica Neue Condensed").contains("Narrow"));
        assert!(css_font_stack("Widget Narrow").contains("Narrow"));
        // A known family keeps its metric clone, and a name that merely contains
        // the letters is not squeezed.
        assert_eq!(css_font_stack("Calibri"), "Carlito, Lato, sans-serif");
        assert_eq!(css_font_stack("Concorde"), "\"Concorde\", sans-serif");
    }

    #[test]
    fn long_font_names_are_capped() {
        let out = css_font_stack(&"W".repeat(500));
        assert_eq!(out.len(), MAX_NAME + "\"\", sans-serif".len());
    }

    #[test]
    fn wingdings_maps_the_common_markers() {
        assert_eq!(wingdings('\u{F0B7}'), Some('•'));
        assert_eq!(wingdings('\u{F0A7}'), Some('▪'));
        assert_eq!(wingdings('\u{F0FC}'), Some('✔'));
        assert_eq!(wingdings('\u{F0FE}'), Some('☑'));
        assert_eq!(wingdings('\u{F0D8}'), Some('➢'));
        // Unmapped PUA code points stay unmapped so the caller can fall back
        // instead of emitting tofu.
        assert_eq!(wingdings('\u{F041}'), None);
        assert_eq!(wingdings('\u{E000}'), None);
    }

    #[test]
    fn symbol_maps_greek_and_math() {
        assert_eq!(symbol_font('\u{F053}'), Some('Σ'));
        assert_eq!(symbol_font('\u{F070}'), Some('π'));
        assert_eq!(symbol_font('\u{F0A5}'), Some('∞'));
        assert_eq!(symbol_font('\u{F0AE}'), Some('→'));
        assert_eq!(symbol_font('\u{F0B3}'), Some('≥'));
        // Plain-text runs with the Symbol font applied use the same encoding.
        assert_eq!(symbol_font('a'), Some('α'));
        assert_eq!(symbol_font(' '), None);
    }

    #[test]
    fn remap_only_trusts_the_fonts_it_has_tables_for() {
        assert_eq!(remap("Wingdings", '\u{F0FC}'), Some('✔'));
        assert_eq!(remap("wingdings", '\u{F0FC}'), Some('✔'));
        assert_eq!(remap("Symbol", '\u{F0B7}'), Some('•'));
        // Different layouts: no table, so no guess.
        assert_eq!(remap("Wingdings 2", '\u{F0FC}'), None);
        assert_eq!(remap("Webdings", '\u{F0FC}'), None);
        assert_eq!(remap("Carlito", '\u{F0FC}'), None);
    }

    #[test]
    fn is_symbol_font_recognizes_the_glyph_fonts() {
        assert!(is_symbol_font("Wingdings"));
        assert!(is_symbol_font("SYMBOL"));
        assert!(!is_symbol_font("Calibri"));
    }
}
