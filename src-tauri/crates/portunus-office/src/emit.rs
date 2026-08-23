//! Renderer scaffolding shared by the xlsx and pptx HTML paths: degradation
//! notes, the `<style>` + body envelope, the bodies that stand in for a section
//! that could not be rendered, and the labelled box a graphic degrades to.
//!
//! Nothing here knows a format's stylesheet or class names — those stay with the
//! renderer and arrive as arguments. What is shared is the *shape* of the output,
//! which is what the two renderers had drifted apart on.

use super::html::{attr, attrs, Writer};
use super::pkg;

/// Notes ride in a muted footer, so a repeat is noise and an unbounded list is a
/// wall of text.
const MAX_NOTES: usize = 12;

/// Byte cap for the self-contained error bodies. Generous next to the one line
/// they hold, so a long underlying error message survives intact.
const ERROR_CAP: usize = 4096;

/// The degradation notes of one render: deduplicated and capped. Repeats are the
/// norm (one per chart on a slide) and the footer only needs to say it once.
#[derive(Default)]
pub struct Notes {
    items: Vec<String>,
}

impl Notes {
    pub fn new() -> Notes {
        Notes::default()
    }

    pub fn add(&mut self, msg: &str) {
        if self.items.len() >= MAX_NOTES || self.items.iter().any(|n| n == msg) {
            return;
        }
        self.items.push(msg.to_string());
    }

    pub fn into_vec(self) -> Vec<String> {
        self.items
    }
}

/// Prepends the stylesheet to a rendered body. `<` is stripped from the CSS as a
/// belt-and-braces measure: every value in it is built by a renderer, and a
/// `</style>` smuggled into a font name or colour would end the element and turn
/// the rest of the stylesheet into document content.
pub fn wrap_style(base_css: &str, extra_css: &str, body: String) -> String {
    let mut out = String::with_capacity(base_css.len() + extra_css.len() + body.len() + 32);
    out.push_str("<style>");
    out.push_str(&base_css.replace('<', ""));
    out.push_str(&extra_css.replace('<', ""));
    out.push_str("</style>");
    out.push_str(&body);
    out
}

/// Body for a section that could not be rendered at all: the reason on an
/// otherwise empty canvas. Self-contained — it carries the stylesheet, because
/// it stands in for a whole section's html rather than being nested inside a
/// normal render. `doc_css` keeps a fixed-geometry canvas (a slide) at its real
/// size so the reader's geometry still works; a sheet passes an empty string.
pub fn error_doc(
    base_css: &str,
    doc_class: &str,
    doc_css: &str,
    msg_class: &str,
    msg: &str,
) -> String {
    let mut w = Writer::new(ERROR_CAP);
    w.open("div", &attrs(&[&attr("class", doc_class), &style_attr(doc_css)]));
    w.open("div", &attr("class", msg_class));
    w.text(msg);
    w.close();
    w.close();
    wrap_style(base_css, "", w.finish())
}

/// The message for a section the renderer had to give up on. A missing or
/// malformed part is a degradation, not a failure: the document's other sections
/// are still listed and reachable, so `noun` names the one that is not.
pub fn degrade_msg(err: &str, noun: &str) -> String {
    if err == pkg::BUDGET_EXCEEDED {
        format!("This {noun} could not be shown: it exceeds the preview size limit.")
    } else {
        format!("This {noun} could not be shown: {err}")
    }
}

/// Positions a placeholder over the whole of its parent box, which is where a
/// shape's or anchor's geometry already is.
pub const FILL_PARENT: &str = "left:0;top:0;width:100%;height:100%;";

/// The dashed box a graphic the preview cannot rasterize degrades to. `class` is
/// the format's own placeholder class and `css` places it.
pub fn placeholder(w: &mut Writer, class: &str, css: &str, label: &str) {
    w.open("div", &attrs(&[&attr("class", class), &style_attr(css)]));
    w.text(label);
    w.close();
}

/// A label for a graphic frame the preview cannot draw, from its
/// `graphicData@uri`. Charts are the common case: they are stored as data plus a
/// layout, never as an image, so a labelled box at the right geometry is the
/// honest answer.
pub fn graphic_label(uri: &str) -> &'static str {
    if uri.contains("/chart") {
        "chart"
    } else if uri.contains("/diagram") {
        "diagram"
    } else if uri.contains("/table") {
        "table"
    } else if uri.contains("/ole") {
        "embedded object"
    } else if uri.contains("/media") {
        "media clip"
    } else {
        "object"
    }
}

/// `style="…"`, or nothing at all when there is nothing to declare (so no bare
/// `style=""` litters the output).
fn style_attr(css: &str) -> String {
    if css.is_empty() {
        String::new()
    } else {
        attr("style", css)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_dedupe_and_cap() {
        let mut n = Notes::new();
        n.add("café");
        n.add("café");
        n.add("naïve");
        assert_eq!(n.into_vec(), ["café", "naïve"]);

        let mut n = Notes::new();
        for i in 0..MAX_NOTES + 10 {
            n.add(&format!("note {i}"));
        }
        assert_eq!(n.into_vec().len(), MAX_NOTES);
    }

    #[test]
    fn graphic_labels_cover_every_frame_kind() {
        let uri = |kind: &str| {
            format!("http://schemas.openxmlformats.org/drawingml/2006/{kind}")
        };
        assert_eq!(graphic_label(&uri("chart")), "chart");
        assert_eq!(graphic_label(&uri("diagram")), "diagram");
        assert_eq!(graphic_label(&uri("table")), "table");
        assert_eq!(
            graphic_label("http://schemas.openxmlformats.org/presentationml/2006/ole"),
            "embedded object"
        );
        assert_eq!(
            graphic_label("http://schemas.microsoft.com/office/2007/relationships/media"),
            "media clip"
        );
        // An unknown or absent uri is still a box with something in it.
        assert_eq!(graphic_label("urn:example:widget"), "object");
        assert_eq!(graphic_label(""), "object");
    }

    #[test]
    fn degrade_msg_names_the_section_and_the_budget_stop() {
        assert_eq!(
            degrade_msg(pkg::BUDGET_EXCEEDED, "sheet"),
            "This sheet could not be shown: it exceeds the preview size limit."
        );
        assert_eq!(
            degrade_msg("café", "slide"),
            "This slide could not be shown: café"
        );
    }

    #[test]
    fn error_doc_is_balanced_and_omits_an_empty_style() {
        let html = error_doc(".x{color:#000;}\n", "xl-doc", "", "xl-empty", "café & <naïve>");
        assert!(html.starts_with("<style>.x{color:#000;}\n</style>"), "{html}");
        assert!(html.contains("<div class=\"xl-doc\"><div class=\"xl-empty\">"), "{html}");
        assert!(!html.contains("style=\"\""), "{html}");
        // Document-derived text is escaped, and the tree closes.
        assert!(html.contains("café &amp; &lt;naïve&gt;"), "{html}");
        assert!(html.ends_with("</div></div>"), "{html}");

        let sized = error_doc("", "pp-doc", "width:96px;", "office-note", "x");
        assert!(
            sized.contains("<div class=\"pp-doc\" style=\"width:96px;\">"),
            "{sized}"
        );
    }
}
