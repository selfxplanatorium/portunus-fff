//! The single source of truth for content-match keying.
//!
//! The content index tokenizes with FTS5 `tokenize='porter unicode61'`
//! (`content_index.rs`). Every *secondary* match - preview highlight, best-page
//! jump, best-section scroll, OCR/PDF box selection - must agree with what the
//! index actually matched, otherwise highlights and jumps land on the wrong
//! words. This module mirrors `porter unicode61`:
//!
//! - [`normalize`] = unicode61's casefold + diacritic fold (the tokenizer's
//!   default `remove_diacritics=1`). Best-effort Latin fold; ASCII (the common
//!   case) is exact.
//! - [`tokenize`] = unicode61's token boundaries: maximal runs of alphanumerics,
//!   so an apostrophe is a separator (`don't` -> `don`, `t`).
//! - [`stem`] = the classic Porter (1980) algorithm FTS5's `porter` wrapper uses.
//!
//! [`match_key`] composes the three; two words share a key iff the index would
//! have matched them. The frontend reuses this exact logic over the
//! `content_match_keys` Tauri command rather than re-implementing Porter in JS.

use std::collections::HashSet;
use std::ops::Range;

/// Casefold + fold diacritics, mirroring unicode61's default `remove_diacritics=1`.
/// ASCII is exact; common Latin accents fold to their base letter; other scripts
/// are lowercased and kept as-is (best effort - SQLite's fold table is larger).
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // Drop diacritic marks - this is unicode61's `remove_diacritics`. PDF text
        // layers split an accented letter into a base + a separate accent char, and
        // the form depends on the extractor: poppler emits NFD combining marks
        // (`a` + U+0301), pdfium emits a spacing accent (`caf´e` = ... U+00B4, e).
        // Dropping both keeps the word keying to its folded form like the index.
        if is_diacritic(c) {
            continue;
        }
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
            continue;
        }
        match fold_latin(c) {
            Some(base) => out.push(base),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// True for diacritic marks unicode61 strips: combining marks (NFD, as poppler
/// emits) AND non-ASCII spacing accents (as pdfium emits for LaTeX-encoded PDFs,
/// e.g. U+00B4 ´, U+00A8 ¨, U+02C6 ̂ in the Spacing Modifier Letters block).
/// ASCII `^`/`` ` ``/`~` are deliberately excluded - they're legitimate text/code.
pub fn is_diacritic(c: char) -> bool {
    matches!(c as u32,
        // combining marks
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
        // spacing diacritics (Latin-1 + Spacing Modifier Letters)
        | 0x00A8 | 0x00AF | 0x00B4 | 0x00B8 | 0x02B0..=0x02FF)
}

/// Folds a precomposed Latin-1 / Latin Extended accented letter to its ASCII base.
/// Returns `None` for anything outside that set (handled by plain lowercasing).
fn fold_latin(c: char) -> Option<char> {
    let base = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'a',
        'ç' | 'Ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
        'ñ' | 'Ñ' => 'n',
        // Note: ø/Ø, æ/œ, ß, đ/ł are NOT folded - unicode61 (remove_diacritics=1)
        // only strips combining diacritics, and these are distinct letters with no
        // such decomposition. Folding them here would disagree with the index.
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
        'ý' | 'ÿ' | 'Ý' => 'y',
        _ => return None,
    };
    Some(base)
}

/// Splits `text` into maximal alphanumeric runs, yielding each token's byte range
/// and slice. Matches unicode61's boundaries (apostrophes/punctuation separate).
pub fn tokenize(text: &str) -> Vec<(Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        // Diacritic marks continue the word - a PDF accent char (combining or
        // spacing) sits between base letters; normalize strips them later. Without
        // this an accented word would split at every accent.
        if c.is_alphanumeric() || is_diacritic(c) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s..i, &text[s..i]));
        }
    }
    if let Some(s) = start {
        out.push((s..text.len(), &text[s..]));
    }
    out
}

/// The key two words share iff `porter unicode61` would have matched them:
/// normalize then Porter-stem. Tokens carrying non-ASCII-letter chars (digits,
/// leftover scripts) skip stemming and key on the normalized form.
pub fn match_key(word: &str) -> String {
    stem(&normalize(word))
}

/// Distinct, non-empty match keys for a set of raw query terms, preserving
/// first-seen order.
pub fn query_keys<I: IntoIterator<Item = String>>(terms: I) -> Vec<String> {
    let mut seen = HashSet::new();
    terms
        .into_iter()
        .map(|t| match_key(&t))
        .filter(|k| !k.is_empty())
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

// ── classic Porter (1980) stemmer ───────────────────────────────────────────────
//
// Faithful port of Martin Porter's reference algorithm, the same one SQLite FTS5's
// `porter` tokenizer implements. Operates on an ASCII-lowercase byte buffer; `k`
// is the inclusive index of the last live char, `j` the working offset set by the
// suffix routines. The buffer tail past `k` is intentionally left stale - every
// reader is bounded by `k`, exactly as the C original.

/// Porter-stems a single normalized token. Words of length <= 2, or carrying any
/// non `[a-z]` byte (digits, unfolded scripts), are returned unchanged.
pub fn stem(word: &str) -> String {
    let bytes: Vec<u8> = word.bytes().collect();
    if bytes.len() <= 2 || !bytes.iter().all(u8::is_ascii_lowercase) {
        return word.to_string();
    }
    let k = bytes.len() as isize - 1;
    let mut s = Porter { b: bytes, k, j: 0 };
    s.step1ab();
    s.step1c();
    s.step2();
    s.step3();
    s.step4();
    s.step5();
    String::from_utf8(s.b[..=s.k as usize].to_vec()).unwrap_or_default()
}

struct Porter {
    b: Vec<u8>,
    k: isize,
    j: isize,
}

impl Porter {
    /// Is `b[i]` a consonant? `y` is a consonant only when preceded by a vowel.
    fn cons(&self, i: isize) -> bool {
        match self.b[i as usize] {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => {
                if i == 0 {
                    true
                } else {
                    !self.cons(i - 1)
                }
            }
            _ => true,
        }
    }

    /// Measure: the number of VC sequences in `b[0..=j]`.
    fn m(&self) -> u32 {
        let mut n = 0;
        let mut i = 0isize;
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    /// Does `b[0..=j]` contain a vowel?
    fn vowelinstem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    /// Is `b[i]` a doubled consonant (`b[i] == b[i-1]`, both consonants)?
    fn doublec(&self, i: isize) -> bool {
        if i < 1 || self.b[i as usize] != self.b[(i - 1) as usize] {
            return false;
        }
        self.cons(i)
    }

    /// consonant-vowel-consonant ending at `i`, last consonant not w/x/y.
    fn cvc(&self, i: isize) -> bool {
        if i < 2 || !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.b[i as usize], b'w' | b'x' | b'y')
    }

    /// Does `b[..=k]` end with `s`? On success sets `j` to the index before `s`.
    fn ends(&mut self, s: &[u8]) -> bool {
        let len = s.len() as isize;
        if len > self.k + 1 {
            return false;
        }
        let start = (self.k + 1 - len) as usize;
        if &self.b[start..=self.k as usize] == s {
            self.j = self.k - len;
            true
        } else {
            false
        }
    }

    /// Replaces the suffix after `j` with `s`, updating `k`.
    fn setto(&mut self, s: &[u8]) {
        self.b.truncate((self.j + 1) as usize);
        self.b.extend_from_slice(s);
        self.k = self.b.len() as isize - 1;
    }

    /// `setto(s)` but only when measure(`b[0..=j]`) > 0.
    fn r(&mut self, s: &[u8]) {
        if self.m() > 0 {
            self.setto(s);
        }
    }

    /// Step 1a/1b: plurals and `-ed`/`-ing`.
    fn step1ab(&mut self) {
        if self.b[self.k as usize] == b's' {
            if self.ends(b"sses") {
                self.k -= 2;
            } else if self.ends(b"ies") {
                self.setto(b"i");
            } else if self.b[(self.k - 1) as usize] != b's' {
                self.k -= 1;
            }
        }
        if self.ends(b"eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends(b"ed") || self.ends(b"ing")) && self.vowelinstem() {
            self.k = self.j;
            if self.ends(b"at") {
                self.setto(b"ate");
            } else if self.ends(b"bl") {
                self.setto(b"ble");
            } else if self.ends(b"iz") {
                self.setto(b"ize");
            } else if self.doublec(self.k) {
                if !matches!(self.b[self.k as usize], b'l' | b's' | b'z') {
                    self.k -= 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.setto(b"e");
            }
        }
    }

    /// Step 1c: terminal `y` -> `i` when the stem has a vowel.
    fn step1c(&mut self) {
        if self.ends(b"y") && self.vowelinstem() {
            self.b[self.k as usize] = b'i';
        }
    }

    /// Step 2: double-suffix contractions (`-ational` -> `-ate`, ...). Longest
    /// suffix first so the first `ends` match is the intended one.
    fn step2(&mut self) {
        const PAIRS: &[(&[u8], &[u8])] = &[
            (b"ational", b"ate"),
            (b"ization", b"ize"),
            (b"iveness", b"ive"),
            (b"fulness", b"ful"),
            (b"ousness", b"ous"),
            (b"biliti", b"ble"),
            (b"tional", b"tion"),
            (b"alism", b"al"),
            (b"aliti", b"al"),
            (b"iviti", b"ive"),
            (b"ousli", b"ous"),
            (b"entli", b"ent"),
            (b"ation", b"ate"),
            (b"ator", b"ate"),
            (b"alli", b"al"),
            (b"izer", b"ize"),
            (b"abli", b"able"),
            (b"enci", b"ence"),
            (b"anci", b"ance"),
            (b"eli", b"e"),
        ];
        for (suf, rep) in PAIRS {
            if self.ends(suf) {
                self.r(rep);
                break;
            }
        }
    }

    /// Step 3: `-icate`, `-ative`, ... reductions.
    fn step3(&mut self) {
        const PAIRS: &[(&[u8], &[u8])] = &[
            (b"icate", b"ic"),
            (b"ative", b""),
            (b"alize", b"al"),
            (b"iciti", b"ic"),
            (b"ical", b"ic"),
            (b"ness", b""),
            (b"ful", b""),
        ];
        for (suf, rep) in PAIRS {
            if self.ends(suf) {
                self.r(rep);
                break;
            }
        }
    }

    /// Step 4: delete residual suffixes (`-al`, `-ance`, `-ic`, ...) when m > 1.
    fn step4(&mut self) {
        const SUFFIXES: &[&[u8]] = &[
            b"ement", b"ance", b"ence", b"able", b"ible", b"ment", b"ant", b"ent", b"ism", b"ate",
            b"iti", b"ous", b"ive", b"ize", b"al", b"er", b"ic", b"ou",
        ];
        for suf in SUFFIXES {
            if self.ends(suf) {
                if self.m() > 1 {
                    self.k = self.j;
                }
                return;
            }
        }
        // `-ion` only after s or t.
        if self.ends(b"ion") && self.m() > 1 && self.j >= 0 {
            if matches!(self.b[self.j as usize], b's' | b't') {
                self.k = self.j;
            }
        }
    }

    /// Step 5a/5b: terminal `-e` and doubled `-l`.
    fn step5(&mut self) {
        self.j = self.k;
        if self.b[self.k as usize] == b'e' {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k - 1)) {
                self.k -= 1;
            }
        }
        if self.b[self.k as usize] == b'l' && self.doublec(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classic-Porter outputs (the same the FTS index produces). Guards parity:
    /// the frontend keys words through the `content_match_keys` command, which
    /// wraps `match_key`, so this fixture pins the behaviour both sides rely on.
    #[test]
    fn porter_reference() {
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("caress", "caress"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("happy", "happi"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("vietnamization", "vietnam"),
            ("predication", "predic"),
            ("operator", "oper"),
            ("feudalism", "feudal"),
            ("decisiveness", "decis"),
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            ("formaliti", "formal"),
            ("sensitiviti", "sensit"),
            ("triplicate", "triplic"),
            ("formative", "form"),
            ("formalize", "formal"),
            ("electriciti", "electr"),
            ("electrical", "electr"),
            ("hopeful", "hope"),
            ("goodness", "good"),
            ("revival", "reviv"),
            ("allowance", "allow"),
            ("inference", "infer"),
            ("airliner", "airlin"),
            ("adjustable", "adjust"),
            ("defensible", "defens"),
            ("irritant", "irrit"),
            ("replacement", "replac"),
            ("adjustment", "adjust"),
            ("dependent", "depend"),
            ("adoption", "adopt"),
            ("communism", "commun"),
            ("activate", "activ"),
            ("effective", "effect"),
            ("bowdlerize", "bowdler"),
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
        ];
        for (word, want) in cases {
            assert_eq!(stem(word), want, "stem({word})");
        }
    }

    #[test]
    fn university_family_shares_key() {
        assert_eq!(match_key("university"), match_key("universities"));
        assert_eq!(match_key("study"), match_key("studies"));
        // Distinct stems must NOT collide (the old prefix bug).
        assert_ne!(match_key("cat"), match_key("category"));
    }

    #[test]
    fn normalize_folds_diacritics_and_case() {
        assert_eq!(normalize("Café"), "cafe");
        assert_eq!(normalize("naïve"), "naive");
        assert_eq!(match_key("café"), match_key("cafe"));
        // ø/æ/ß are distinct letters, not combining diacritics - unicode61 keeps
        // them, so we must NOT fold them or highlighting would disagree with the index.
        assert_eq!(normalize("søk"), "søk");
        assert_ne!(match_key("søk"), match_key("sok"));
    }

    #[test]
    fn nfd_decomposed_text_keys_like_precomposed() {
        // PDF/LaTeX text layers are often NFD: an accent is a separate combining mark.
        let nfd = "cafe\u{0301}"; // e + combining acute
        let nfc = "café"; // precomposed
        assert_eq!(normalize(nfd), normalize(nfc));
        assert_eq!(match_key(nfd), match_key(nfc));
        // And the whole NFD word must tokenize as ONE token, not split at the accent.
        let line = format!("{nfd} nai\u{0308}ve"); // "café naïve", both NFD
        let toks: Vec<&str> = tokenize(&line).iter().map(|(_, t)| *t).collect();
        assert_eq!(toks.len(), 2);
        assert_eq!(match_key(toks[0]), match_key("cafe"));
        assert_eq!(match_key(toks[1]), match_key("naive"));
    }

    #[test]
    fn pdfium_spacing_accent_keys_like_plain() {
        // pdfium extracts LaTeX-encoded accents as a SPACING accent char (U+00B4 ´)
        // beside the base letter, e.g. "café" -> "caf´e" (... f, U+00B4, e).
        let spaced = "caf\u{00B4}e";
        assert_eq!(normalize(spaced), "cafe");
        assert_eq!(match_key(spaced), match_key("cafe"));
        // Must stay ONE token, not split at the accent.
        let toks = tokenize(spaced);
        assert_eq!(toks.len(), 1);
    }

    #[test]
    fn tokenize_splits_on_apostrophe() {
        let toks: Vec<&str> = tokenize("don't stop").iter().map(|(_, t)| *t).collect();
        assert_eq!(toks, ["don", "t", "stop"]);
    }

    #[test]
    fn query_keys_dedup_and_drop_empty() {
        let keys = query_keys(["Running".to_string(), "ran".to_string(), "RUNS".to_string()]);
        // "running" and "runs" both stem to "run"; "ran" is irregular (stays "ran").
        assert!(keys.contains(&"run".to_string()));
        assert!(keys.contains(&"ran".to_string()));
        assert_eq!(keys.iter().filter(|k| *k == "run").count(), 1);
    }
}
