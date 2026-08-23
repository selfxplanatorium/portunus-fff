//! HTML emission shared by the office renderers: context-correct escaping, a
//! size-capped writer that cannot emit unbalanced markup, and CSS value/unit
//! helpers for the Office measurement units.

#![allow(dead_code)] // Consumed by the later-stage renderers.

use std::borrow::Cow;

// ── escaping ─────────────────────────────────────────────────────────────────

// Two functions rather than one, because the contexts are not equivalent. Text
// nodes only need `&`, `<`, `>`; an attribute value additionally needs both
// quote kinds, since a value that escapes `<`/`>` but not `"` still lets
// document content close the value and open a new attribute
// (`café" onmouseover="…`) — a scripting hole rather than a display bug. Having
// one lenient function around guarantees it eventually gets used in the
// stricter context, so the strict one has its own name.

/// Escapes a text node. Returns the input borrowed when nothing needs escaping,
/// which is the overwhelmingly common case for document text.
pub fn esc_text(s: &str) -> Cow<'_, str> {
    if !s.bytes().any(|b| matches!(b, b'&' | b'<' | b'>')) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Escapes a double- or single-quoted attribute value.
pub fn esc_attr(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// `name="escaped"` fragment for `Writer::open` / `Writer::void`.
pub fn attr(name: &str, value: &str) -> String {
    format!("{}=\"{}\"", name, esc_attr(value))
}

/// Space-joins attribute fragments, skipping empty ones so callers can build a
/// list with optional members.
pub fn attrs(parts: &[&str]) -> String {
    let mut out = String::new();
    for p in parts {
        if p.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(p);
    }
    out
}

// ── writer ───────────────────────────────────────────────────────────────────

/// Deepest element nesting the writer will emit. A hostile (or merely
/// generated) document can nest thousands of levels; past this the writer
/// degrades to the truncated state rather than handing WebKit a tree deep
/// enough to blow its parser stack.
const MAX_DEPTH: usize = 256;

const TRUNC_NOTE: &str =
    "<div class=\"office-trunc\">Preview truncated: the document exceeds the preview size limit.</div>";

struct Frame {
    tag: &'static str,
    /// False when the open tag was suppressed (byte cap or depth cap). `close`
    /// must then emit nothing, or the output gains a closing tag with no
    /// opening one.
    emitted: bool,
}

/// A size-capped HTML sink. Once the cap trips it stops accepting *content* but
/// keeps tracking the open-element stack, so `finish` still closes everything
/// that was opened and the frontend never receives half-open markup.
pub struct Writer {
    buf: String,
    cap: usize,
    stack: Vec<Frame>,
    truncated: bool,
}

impl Writer {
    pub fn new(cap: usize) -> Self {
        Writer {
            buf: String::with_capacity(cap.min(64 * 1024)),
            cap,
            stack: Vec::new(),
            truncated: false,
        }
    }

    /// True once no further content will be accepted; callers can use it to
    /// abandon an expensive subtree early.
    pub fn is_full(&self) -> bool {
        self.truncated || self.buf.len() >= self.cap
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    // Content is charged all-or-nothing per call: a partially written run could
    // split an escape sequence or a UTF-8 sequence mid-character.
    fn take(&mut self, n: usize) -> bool {
        if self.truncated {
            return false;
        }
        if self.buf.len() + n > self.cap {
            self.truncated = true;
            return false;
        }
        true
    }

    /// Opens an element. `attrs` must already be escaped by the caller (see
    /// `attr` / `Style::to_attr`) — it is written verbatim.
    pub fn open(&mut self, tag: &'static str, attrs: &str) {
        let emitted = if self.stack.len() >= MAX_DEPTH {
            // Depth overflow counts as truncation, but the frame is still
            // pushed so the caller's matching `close` pops the right frame.
            self.truncated = true;
            false
        } else {
            let n = 2 + tag.len() + if attrs.is_empty() { 0 } else { attrs.len() + 1 };
            if self.take(n) {
                self.buf.push('<');
                self.buf.push_str(tag);
                if !attrs.is_empty() {
                    self.buf.push(' ');
                    self.buf.push_str(attrs);
                }
                self.buf.push('>');
                true
            } else {
                false
            }
        };
        self.stack.push(Frame { tag, emitted });
    }

    /// Pops one frame and emits its closing tag. Closing tags deliberately
    /// bypass the byte cap: refusing them would leave markup that was already
    /// emitted unbalanced, so the cap is soft by the size of the open stack.
    pub fn close(&mut self) {
        if let Some(f) = self.stack.pop() {
            if f.emitted {
                self.buf.push_str("</");
                self.buf.push_str(f.tag);
                self.buf.push('>');
            }
        }
    }

    /// Escapes and appends document text.
    pub fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let esc = esc_text(s);
        if self.take(esc.len()) {
            self.buf.push_str(&esc);
        }
    }

    /// Appends `s` verbatim. Trusted markup only — strings this module or a
    /// renderer built itself. Anything derived from document XML must go
    /// through `text` or the attribute helpers instead.
    pub fn raw(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.take(s.len()) {
            self.buf.push_str(s);
        }
    }

    /// Void element (`img`, `br`, `hr`): emitted without a stack frame, because
    /// there is nothing to close. `tag` therefore need not be `'static`.
    pub fn void(&mut self, tag: &str, attrs: &str) {
        let n = 2 + tag.len() + if attrs.is_empty() { 0 } else { attrs.len() + 1 };
        if !self.take(n) {
            return;
        }
        self.buf.push('<');
        self.buf.push_str(tag);
        if !attrs.is_empty() {
            self.buf.push(' ');
            self.buf.push_str(attrs);
        }
        self.buf.push('>');
    }

    /// Closes every still-open element, then appends the truncation note if the
    /// document was cut short. The note is top level, hence emitted last.
    pub fn finish(mut self) -> String {
        while !self.stack.is_empty() {
            self.close();
        }
        if self.truncated {
            self.buf.push_str(TRUNC_NOTE);
        }
        self.buf
    }
}

// ── CSS values ───────────────────────────────────────────────────────────────

// All formatters return `None` for non-finite input. `Some("NaNpx")` would not
// fail in isolation: CSS drops the whole *declaration*, so one poisoned number
// silently takes every other property in that `style` attribute with it.

fn fmt_num(v: f32) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    let mut s = format!("{:.2}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_string();
    }
    Some(s)
}

pub fn fmt_px(v: f32) -> Option<String> {
    fmt_num(v).map(|s| s + "px")
}

pub fn fmt_pt(v: f32) -> Option<String> {
    fmt_num(v).map(|s| s + "pt")
}

pub fn fmt_pct(v: f32) -> Option<String> {
    fmt_num(v).map(|s| s + "%")
}

pub fn fmt_deg(v: f32) -> Option<String> {
    fmt_num(v).map(|s| s + "deg")
}

/// A unitless number, for the CSS properties where a bare ratio and a percentage
/// are not the same thing — `line-height` above all, which resolves a percentage at
/// the element that states it and a number at each element that inherits it.
pub fn fmt_ratio(v: f32) -> Option<String> {
    fmt_num(v)
}

// Unit converters. Each is named for the unit it consumes, because the Office
// formats express the same quantity in four different scalings and mixing them
// up produces a plausible-looking layout that is off by 20x.

/// Points → px at the CSS reference resolution (1pt = 1/72in, 96px = 1in).
pub fn pt_to_px(pt: f32) -> f32 {
    pt * 96.0 / 72.0
}

/// EMU (English Metric Units, 914400 per inch — DrawingML positions, sizes,
/// image extents) → px.
pub fn emu_to_px(emu: i64) -> f32 {
    emu as f32 / 914400.0 * 96.0
}

/// Hundredths of a point (DrawingML `sz` on text runs, `a:ln` widths) → px.
pub fn hundredths_pt_to_px(v: i64) -> f32 {
    pt_to_px(v as f32 / 100.0)
}

/// Half-points (docx `w:sz` / `w:szCs` font size) → px.
pub fn half_pt_to_px(v: i64) -> f32 {
    pt_to_px(v as f32 / 2.0)
}

/// Twentieths of a point, "dxa" (docx `w:ind`, `w:spacing`, `w:tblW`, page
/// margins) → px.
pub fn dxa_to_px(v: i64) -> f32 {
    pt_to_px(v as f32 / 20.0)
}

/// Eighths of a point (docx border `w:sz`) → px.
pub fn eighth_pt_to_px(v: i64) -> f32 {
    pt_to_px(v as f32 / 8.0)
}

/// Fiftieths of a percent (docx `w:tblW` with `w:type="pct"`) → percent.
pub fn pct50_to_pct(v: i64) -> f32 {
    v as f32 / 50.0
}

/// Accumulates `prop:value;` pairs. `push_opt` drops the pair when the value
/// formatter returned `None`, so one non-finite measurement cannot invalidate
/// the declarations around it.
#[derive(Default)]
pub struct Style {
    buf: String,
}

impl Style {
    pub fn new() -> Self {
        Style { buf: String::new() }
    }

    /// Values are raw CSS here; escaping happens once, in `to_attr`. Values
    /// derived from document XML must be sanitized by their own producer (e.g.
    /// `fonts::css_font_stack`) before they get here.
    pub fn push(&mut self, prop: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        self.buf.push_str(prop);
        self.buf.push(':');
        self.buf.push_str(value);
        self.buf.push(';');
    }

    pub fn push_opt(&mut self, prop: &str, value: Option<String>) {
        if let Some(v) = value {
            self.push(prop, &v);
        }
    }

    /// Appends an already-formed `prop:value;` declaration.
    ///
    /// For the emitters that produce a whole declaration rather than a value —
    /// `line_css` and `fill_css` — so a caller does not have to take the property
    /// name back off. Anything that is not a terminated declaration is dropped
    /// rather than concatenated into the next one.
    pub fn push_decl(&mut self, decl: &str) {
        let d = decl.trim();
        if !d.ends_with(';') || !d.contains(':') {
            return;
        }
        self.buf.push_str(d);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn css(&self) -> &str {
        &self.buf
    }

    /// `style="…"` ready for `Writer::open`, or an empty string when no pair
    /// survived (so no bare `style=""` litters the output).
    pub fn to_attr(&self) -> String {
        if self.buf.is_empty() {
            return String::new();
        }
        attr("style", &self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_text_escapes_markup_only() {
        assert_eq!(esc_text("<&>"), "&lt;&amp;&gt;");
        assert_eq!(esc_text("a \"b\" 'c'"), "a \"b\" 'c'");
    }

    #[test]
    fn esc_attr_escapes_both_quote_kinds() {
        assert_eq!(
            esc_attr("x\" y' <&>"),
            "x&quot; y&#39; &lt;&amp;&gt;"
        );
    }

    #[test]
    fn escapers_borrow_when_nothing_needs_escaping() {
        assert!(matches!(esc_text("café naïve"), Cow::Borrowed(_)));
        assert!(matches!(esc_attr("example.org/Widget"), Cow::Borrowed(_)));
        assert!(matches!(esc_text("a<b"), Cow::Owned(_)));
        assert!(matches!(esc_attr("a\"b"), Cow::Owned(_)));
    }

    #[test]
    fn writer_emits_balanced_markup() {
        let mut w = Writer::new(4096);
        w.open("div", &attr("class", "p"));
        w.text("café & naïve");
        w.void("br", "");
        w.close();
        assert_eq!(
            w.finish(),
            "<div class=\"p\">café &amp; naïve<br></div>"
        );
    }

    #[test]
    fn writer_closes_open_tags_after_cap_trips() {
        // The invariant that matters: content past the cap is dropped, but every
        // tag opened before it still gets closed, innermost first.
        let mut w = Writer::new(64);
        w.open("div", "");
        w.open("p", "");
        w.open("span", "");
        w.text(&"Widget ".repeat(64));
        w.text("more");
        assert!(w.is_full());
        assert!(w.truncated());
        let out = w.finish();
        assert!(
            out.ends_with(&format!("</span></p></div>{}", TRUNC_NOTE)),
            "unbalanced or missing note: {}",
            out
        );
        assert!(out.contains("office-trunc"));
        assert!(!out.contains("more"));
    }

    #[test]
    fn writer_close_after_cap_does_not_desync_the_stack() {
        // A `close` for an element whose `open` was suppressed must emit
        // nothing; the outer element opened before the cap must still close.
        let mut w = Writer::new(16);
        w.open("div", "");
        w.text("café café café café");
        w.open("span", ""); // suppressed: writer is already full
        w.close(); // must not emit </span>
        w.close();
        let out = w.finish();
        assert_eq!(out.matches("<span").count(), 0);
        assert_eq!(out.matches("</span>").count(), 0);
        assert_eq!(out.matches("<div>").count(), 1);
        assert!(out.starts_with("<div></div>"));
    }

    #[test]
    fn writer_depth_overflow_stays_balanced() {
        let mut w = Writer::new(1 << 20);
        let over = MAX_DEPTH + 40;
        for _ in 0..over {
            w.open("span", "");
        }
        w.text("Widget");
        for _ in 0..over {
            w.close();
        }
        let out = w.finish();
        assert!(out.contains("office-trunc"));
        assert_eq!(out.matches("<span>").count(), MAX_DEPTH);
        assert_eq!(out.matches("</span>").count(), MAX_DEPTH);
    }

    #[test]
    fn writer_finish_closes_a_stack_the_caller_left_open() {
        let mut w = Writer::new(4096);
        w.open("table", "");
        w.open("tr", "");
        w.open("td", "");
        w.text("Widget");
        assert_eq!(w.finish(), "<table><tr><td>Widget</td></tr></table>");
    }

    #[test]
    fn fmt_px_rejects_non_finite_and_trims_zeros() {
        assert_eq!(fmt_px(f32::NAN), None);
        assert_eq!(fmt_px(f32::INFINITY), None);
        assert_eq!(fmt_px(f32::NEG_INFINITY), None);
        assert_eq!(fmt_px(12.0).as_deref(), Some("12px"));
        assert_eq!(fmt_px(12.5).as_deref(), Some("12.5px"));
        assert_eq!(fmt_pt(9.0).as_deref(), Some("9pt"));
        assert_eq!(fmt_pct(50.0).as_deref(), Some("50%"));
        assert_eq!(fmt_deg(135.5).as_deref(), Some("135.5deg"));
        assert_eq!(fmt_deg(-0.0).as_deref(), Some("0deg"));
        assert_eq!(fmt_deg(f32::NAN), None);
    }

    #[test]
    fn unit_converters_use_96dpi() {
        assert_eq!(emu_to_px(914400), 96.0);
        assert_eq!(pt_to_px(72.0), 96.0);
        assert_eq!(half_pt_to_px(24), 16.0); // 12pt
        assert_eq!(dxa_to_px(1440), 96.0); // 72pt = 1in
        assert_eq!(hundredths_pt_to_px(7200), 96.0);
        assert_eq!(eighth_pt_to_px(8), pt_to_px(1.0));
        assert_eq!(pct50_to_pct(2500), 50.0);
    }

    #[test]
    fn style_skips_unformattable_values_and_escapes_once() {
        let mut s = Style::new();
        s.push_opt("width", fmt_px(f32::NAN));
        s.push_opt("height", fmt_px(24.0));
        s.push("font-family", "\"Widget Sans\", sans-serif");
        assert_eq!(s.css(), "height:24px;font-family:\"Widget Sans\", sans-serif;");
        let a = s.to_attr();
        assert!(a.starts_with("style=\""));
        assert!(!a.contains("width"));
        // The font stack's own quotes must be entity-escaped so they cannot end
        // the attribute value.
        assert!(a.contains("&quot;Widget Sans&quot;"));
        assert_eq!(Style::new().to_attr(), "");
    }

    #[test]
    fn attrs_joins_and_skips_empties() {
        assert_eq!(
            attrs(&[&attr("class", "r"), "", &attr("id", "a\"b")]),
            "class=\"r\" id=\"a&quot;b\""
        );
    }
}
