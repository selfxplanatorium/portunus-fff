//! Search-term highlighting for the office HTML previews: matches are wrapped in
//! `<mark class="preview-hl">` while the HTML is being emitted, so the highlights
//! are in the document's first painted frame instead of appearing once a JS pass
//! has walked the DOM.
//!
//! Matching goes through [`portunus_text`], which mirrors the content
//! index's FTS5 `porter unicode61` tokenizer. That is not an implementation
//! detail to swap out: if this module keyed words differently, the preview would
//! highlight words the search never matched (and skip the ones it did). Stemming
//! is never re-implemented here.
//!
//! Accepted gap: a term split across two differently-formatted runs
//! (`<b>run</b>ning` — two `w:r` elements) is missed, because runs are marked
//! independently and neither one carries the whole token. Joining runs first
//! would mean buffering a whole paragraph and then re-splitting the marked HTML
//! at run boundaries, which is where the formatting lives; the index matched the
//! word either way, so the cost is a missing highlight, never a wrong one.

#![allow(dead_code)] // Consumed by the later-stage renderers.

use super::html::esc_text;
use portunus_text::{match_key, query_keys, tokenize};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Upper bound on wrapped matches per render. A pathological document (a
/// dictionary, a log dump) can match tens of thousands of times; past this the
/// text is still emitted in full, only unwrapped, so nothing disappears from the
/// preview.
pub const MAX_MARKS: usize = 2000;

/// Marks within this many *marks* of each other count as one cluster. The
/// coverage heuristic in `content_index::best_section` / `read_text_preview` windows
/// over pages and over lines; inline HTML has neither, so the closest analogue is
/// proximity in emitted-mark order, which is document order.
const CLUSTER_MARKS: usize = 8;

/// Prefix of the per-mark element id (`pm-0`, `pm-1`, …).
pub const MARK_ID_PREFIX: &str = "pm-";

/// Distinct words after which the stem memo stops growing. Bounds memory on a
/// document with a very large vocabulary; past it, words are simply re-stemmed.
const MAX_MEMO: usize = 8192;

/// Stem keys for the query terms. Computed once per render.
pub struct Terms {
    keys: Vec<String>,
    /// key → bit index for the cluster bookkeeping. Capped at 64 keys because the
    /// coverage counter is a fixed set of slots, exactly as in
    /// `read_text_preview`.
    idx: HashMap<String, usize>,
}

impl Terms {
    pub fn new(terms: &[String]) -> Terms {
        // Raw terms are tokenized first: a phrase term ("café naïve") would
        // otherwise be stemmed as one nonsense word and match nothing. Then
        // `query_keys` folds, stems, dedups and drops empties — mirroring
        // `preview.rs::normalize_terms`, including dropping 1-char noise.
        //
        // **Raw**, not keyed. This is the one and only keying of the query, and a
        // caller that keys first breaks the terms it most wants marked: Porter is
        // not idempotent, so "University" → "univers" → "univer", which matches no
        // word in any document. See `render_office_doc`, which used to do exactly
        // that, and `stemming_the_query_twice_is_what_would_break_it` below.
        let words = terms.iter().flat_map(|t| {
            tokenize(t)
                .into_iter()
                .map(|(_, w)| w.to_string())
                .filter(|w| w.chars().count() >= 2)
                .collect::<Vec<_>>()
        });
        let keys: Vec<String> = query_keys(words);
        let idx = keys
            .iter()
            .take(64)
            .enumerate()
            .map(|(i, k)| (k.clone(), i))
            .collect();
        Terms { keys, idx }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The keys actually matched against — i.e. whatever
    /// `portunus_text::query_keys` produced.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    fn bit(&self, key: &str) -> Option<usize> {
        self.idx.get(key).copied()
    }
}

/// Wraps matches while the HTML is emitted, and tracks which mark the viewport
/// should jump to. One `Marker` per rendered document: mark ids and the cluster
/// bookkeeping run across every `mark` call, in document order.
///
/// The jump target creates an ordering problem — the best cluster is only known
/// after the whole document has been marked, but the id has to sit on a mark that
/// was emitted long before. Rather than render twice (a second full pass over the
/// XML, purely to place one attribute) or patch bytes back into a finished
/// `Writer` buffer, every mark gets a stable id (`pm-<ordinal>`) as it is
/// emitted, and [`Marker::best_mark_id`] names the winner afterwards. The caller
/// hands that id to the frontend, which scrolls to it — no `id="pmatch"` has to
/// exist in the markup at all.
pub struct Marker {
    count: usize,
    /// Matched key bit per emitted mark, in document order. Bounded by
    /// `MAX_MARKS`.
    bits: Vec<usize>,
    /// How many marks in the current window carry key `i`, plus how many keys
    /// have a nonzero count.
    counts: [u16; 64],
    distinct: i32,
    best_distinct: i32,
    best_start: usize,
    memo: HashMap<String, Option<usize>>,
}

impl Default for Marker {
    fn default() -> Self {
        Marker {
            count: 0,
            bits: Vec::new(),
            counts: [0; 64],
            distinct: 0,
            best_distinct: 0,
            best_start: 0,
            memo: HashMap::new(),
        }
    }
}

impl Marker {
    pub fn new() -> Self {
        Marker::default()
    }

    /// Returns `text` HTML-escaped with matches wrapped.
    ///
    /// Escaping and marking happen in one pass over the *original* string, which
    /// is the only ordering that works. Escaping first and then searching for
    /// match offsets breaks, because one `&` becomes `&amp;` and shifts every
    /// later byte offset so the marks land on the wrong slice; marking first and
    /// escaping afterwards breaks, because it escapes the `<mark>` tags we just
    /// wrote. So the token byte ranges from `portunus_text::tokenize` (which
    /// walks `char_indices` and therefore only ever yields char boundaries) cut
    /// the original text into gap and token segments, and each segment is escaped
    /// as it is appended.
    pub fn mark(&mut self, text: &str, terms: &Terms) -> String {
        if text.is_empty() {
            return String::new();
        }
        if terms.is_empty() {
            return esc_text(text).into_owned();
        }
        let mut out = String::with_capacity(text.len() + 16);
        let mut prev = 0usize;
        for (range, word) in tokenize(text) {
            let Some(bit) = self.bit_of(word, terms) else {
                continue; // no match: the word stays part of the next gap segment
            };
            out.push_str(&esc_text(&text[prev..range.start]));
            if self.count < MAX_MARKS {
                // The id is digits only, so it needs no attribute escaping.
                let _ = write!(
                    out,
                    "<mark class=\"preview-hl\" id=\"{MARK_ID_PREFIX}{}\">",
                    self.count
                );
                out.push_str(&esc_text(word));
                out.push_str("</mark>");
                self.observe(bit);
                self.count += 1;
            } else {
                // Past the cap: stop wrapping, keep emitting the text itself.
                out.push_str(&esc_text(word));
            }
            prev = range.end;
        }
        out.push_str(&esc_text(&text[prev..]));
        out
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Ordinal of the mark that should carry the scroll target: the first mark of
    /// the cluster covering the most *distinct* query keys. `None` when nothing
    /// matched.
    pub fn best_mark(&self) -> Option<usize> {
        (self.best_distinct > 0).then_some(self.best_start)
    }

    /// Element id of `best_mark`, ready for `getElementById` / `scrollIntoView`.
    pub fn best_mark_id(&self) -> Option<String> {
        self.best_mark().map(|n| format!("{MARK_ID_PREFIX}{n}"))
    }

    /// Distinct query keys covered by the winning cluster.
    pub fn best_distinct(&self) -> usize {
        self.best_distinct.max(0) as usize
    }

    /// Adds one mark to the cluster window. Same coverage heuristic as
    /// `content_index::best_section`: rank a window by how many *distinct* query
    /// keys it covers and keep the earliest window on a tie (strict `>`), so a
    /// multi-term query lands where the terms actually meet rather than on the
    /// first dense run of one of them.
    fn observe(&mut self, bit: usize) {
        self.bits.push(bit);
        if self.counts[bit] == 0 {
            self.distinct += 1;
        }
        self.counts[bit] += 1;
        // Slide the trailing edge out of the window.
        if self.bits.len() > CLUSTER_MARKS {
            let out = self.bits[self.bits.len() - 1 - CLUSTER_MARKS];
            self.counts[out] -= 1;
            if self.counts[out] == 0 {
                self.distinct -= 1;
            }
        }
        if self.distinct > self.best_distinct {
            self.best_distinct = self.distinct;
            self.best_start = self.bits.len() - self.bits.len().min(CLUSTER_MARKS);
        }
    }

    /// Match key bit for `word`, memoized: document text repeats words heavily
    /// and Porter stemming is the expensive part of marking.
    fn bit_of(&mut self, word: &str, terms: &Terms) -> Option<usize> {
        if let Some(&hit) = self.memo.get(word) {
            return hit;
        }
        let hit = terms.bit(&match_key(word));
        if self.memo.len() < MAX_MEMO {
            self.memo.insert(word.to_string(), hit);
        }
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(raw: &[&str]) -> Terms {
        Terms::new(&raw.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn marked(text: &str, raw: &[&str]) -> String {
        Marker::new().mark(text, &terms(raw))
    }

    /// Just the marked words, in order.
    fn marks_of(html: &str) -> Vec<String> {
        html.split("<mark")
            .skip(1)
            .filter_map(|seg| {
                let body = seg.split_once('>')?.1;
                Some(body.split_once("</mark>")?.0.to_string())
            })
            .collect()
    }

    #[test]
    fn matching_agrees_with_the_index_stemmer() {
        // The whole point of the module: "run" must highlight "running", because
        // that is what porter-stemmed FTS5 matched. The keys really come out of
        // portunus_text, not from a local approximation.
        let t = terms(&["run"]);
        assert_eq!(
            t.keys(),
            portunus_text::query_keys(["run".to_string()]).as_slice()
        );
        assert_eq!(t.keys(), ["run".to_string()]);

        let html = Marker::new().mark("running fast", &t);
        assert_eq!(marks_of(&html), ["running"]);
        assert_eq!(html, "<mark class=\"preview-hl\" id=\"pm-0\">running</mark> fast");

        // And the other direction: inflected query, uninflected document.
        assert_eq!(marks_of(&marked("we run daily", &["running"])), ["run"]);
        // Diacritics fold the way unicode61 folds them.
        assert_eq!(marks_of(&marked("Café naïve", &["cafe"])), ["Café"]);
    }

    #[test]
    fn whole_tokens_only_never_substrings() {
        assert_eq!(marks_of(&marked("cart art carts", &["art"])), ["art"]);
        assert!(!marked("cart", &["art"]).contains("<mark"));
        assert_eq!(marks_of(&marked("Sheet1 Sheet11", &["Sheet1"])), ["Sheet1"]);
    }

    #[test]
    fn escapes_and_marks_in_the_same_pass() {
        // `&` expands to 5 bytes: an escape-then-search implementation would
        // shift every later offset and mark the wrong slice.
        let html = marked("Widget & <café> naïve", &["café", "widget"]);
        assert_eq!(
            html,
            "<mark class=\"preview-hl\" id=\"pm-0\">Widget</mark> &amp; \
             &lt;<mark class=\"preview-hl\" id=\"pm-1\">café</mark>&gt; naïve"
        );
        // No unescaped document markup survived, and our own tags were not
        // escaped.
        assert!(!html.contains("<café"));
        assert!(!html.contains("&lt;mark"));
    }

    #[test]
    fn marks_at_the_very_start_and_the_very_end() {
        let html = marked("Widget café", &["widget", "café"]);
        assert!(html.starts_with("<mark class=\"preview-hl\" id=\"pm-0\">Widget</mark>"));
        assert!(html.ends_with("café</mark>"));
        assert_eq!(marks_of(&html), ["Widget", "café"]);
        // A single-token string is both cases at once.
        assert_eq!(marks_of(&marked("café", &["café"])), ["café"]);
    }

    #[test]
    fn multibyte_text_slices_on_char_boundaries() {
        // Would panic if a byte offset landed inside é / ï.
        let html = marked("café naïve", &["café"]);
        assert_eq!(html, "<mark class=\"preview-hl\" id=\"pm-0\">café</mark> naïve");
        let html = marked("naïve café naïve", &["naïve"]);
        assert_eq!(marks_of(&html), ["naïve", "naïve"]);
        assert!(html.contains(" café "));
        // NFD input (a separate combining accent) is one token and keys like the
        // index; the marked text is returned byte-for-byte as it came in.
        assert_eq!(
            marks_of(&marked("cafe\u{0301} Widget", &["café"])),
            ["cafe\u{0301}"]
        );
    }

    #[test]
    fn empty_terms_still_escape_the_text() {
        let t = terms(&[]);
        assert!(t.is_empty());
        let mut m = Marker::new();
        assert_eq!(m.mark("a & b <c>", &t), "a &amp; b &lt;c&gt;");
        assert_eq!(m.mark("", &t), "");
        assert_eq!(m.count(), 0);
        assert_eq!(m.best_mark(), None);
        assert_eq!(m.best_mark_id(), None);
        // A 1-char term is noise and yields no keys, exactly like the index path.
        assert!(terms(&["x"]).is_empty());
    }

    #[test]
    fn max_marks_stops_wrapping_but_keeps_the_text_complete() {
        let words = MAX_MARKS + 50;
        let text = vec!["café"; words].join(" ");
        let mut m = Marker::new();
        let html = m.mark(&text, &terms(&["café"]));
        assert_eq!(m.count(), MAX_MARKS);
        assert_eq!(html.matches("<mark").count(), MAX_MARKS);
        assert_eq!(html.matches("</mark>").count(), MAX_MARKS);
        // Every occurrence is still in the output, wrapped or not.
        assert_eq!(html.matches("café").count(), words);
        // Ids stay unique across the whole render and stop at the cap.
        assert!(html.contains("id=\"pm-0\""));
        assert!(html.contains(&format!("id=\"pm-{}\"", MAX_MARKS - 1)));
        assert!(!html.contains(&format!("id=\"pm-{MAX_MARKS}\"")));
    }

    #[test]
    fn mark_ids_are_unique_across_calls() {
        let t = terms(&["café", "widget"]);
        let mut m = Marker::new();
        let a = m.mark("café", &t);
        let b = m.mark("Widget", &t);
        assert!(a.contains("id=\"pm-0\""));
        assert!(b.contains("id=\"pm-1\""));
        assert_eq!(m.count(), 2);
    }

    #[test]
    fn best_cluster_prefers_distinct_keys_over_raw_hit_count() {
        let t = terms(&["café", "naïve"]);
        assert_eq!(t.len(), 2);
        let mut m = Marker::new();
        // Nine hits of one key, then a window that finally covers both.
        let text = format!("{} naïve", vec!["café"; 9].join(" "));
        m.mark(&text, &t);
        assert_eq!(m.count(), 10);
        assert_eq!(m.best_distinct(), 2, "the dense single-key run must not win");
        let best = m.best_mark().expect("a winning cluster");
        // The winner is the window containing the second key, not the head of the
        // long single-key run.
        assert!(best > 0, "best mark {best} should not be the first hit");
        assert!(
            best + CLUSTER_MARKS > 9,
            "the window starting at {best} must reach mark 9"
        );
        assert_eq!(m.best_mark_id(), Some(format!("pm-{best}")));

        // A single key anywhere still gives a target.
        let mut m = Marker::new();
        m.mark("Sheet1 Widget Sheet1", &terms(&["widget"]));
        assert_eq!(m.best_distinct(), 1);
        assert_eq!(m.best_mark(), Some(0));
    }

    #[test]
    fn stemming_the_query_twice_is_what_would_break_it() {
        // The reported bug, as a unit: searching "The University of Edinburgh"
        // marked every word but the one that mattered. `render_office_doc` keyed
        // the terms before handing them over, and this module keyed them again —
        // Porter is not idempotent, so the second pass turned "univers" into
        // "univer", a key no document word ever produces.
        let raw = ["The", "University", "of", "Edinburgh"];
        assert_eq!(
            marks_of(&marked("The University of Edinburgh", &raw)),
            ["The", "University", "of", "Edinburgh"],
        );

        // What the double keying did, spelled out so the trap stays visible: feed
        // this module already-keyed terms and the same text loses that word.
        let keyed: Vec<String> =
            portunus_text::query_keys(raw.iter().map(|s| s.to_string()));
        let pre_keyed: Vec<&str> = keyed.iter().map(String::as_str).collect();
        let lost = marks_of(&marked("The University of Edinburgh", &pre_keyed));
        assert!(
            !lost.contains(&"University".to_string()),
            "pre-keyed terms cannot match; if they now can, the guard above is moot: {lost:?}"
        );
    }
}
