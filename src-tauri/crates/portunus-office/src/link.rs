//! Link destinations and bookmark names, turned into the two strings the emitter
//! is allowed to put in an attribute: an `href` and an `id`.
//!
//! Both are document-controlled, so both are decided here rather than at the
//! point of emission — [`super::model::TextRun::link`] and
//! [`super::model::Run::Anchor`] are documented as *already sanitized*, and this
//! is the file that owes them that.
//!
//! Shared by every dialect rather than living with one: `w:hyperlink@r:id`,
//! `text:a@xlink:href` and a bookmark name in either format are all the same
//! problem, and a second copy of a URL whitelist is a second place for it to go
//! stale. [`href_of`] is the OOXML relationship half; [`sanitize_href`] and
//! [`bookmark_id`] are what a dialect with plain URIs in its markup needs.
//!
//! The frame the HTML lands in cannot navigate anywhere: its sandbox is
//! `allow-scripts` with no `allow-top-navigation` and no `allow-popups`, and its
//! CSP is `default-src 'none'` with a nonced `script-src`, which kills
//! `javascript:` URLs and `on*=` handlers outright (see `buildOfficeSrcdoc` in
//! `src/srcdoc.ts`). The whitelist below is therefore defence in depth and not
//! the only guard — but it is the layer that lives next to the parser, and the
//! other two are one attribute edit away in a `.tsx`.

use super::opc;
use super::xml::attr_local;
use roxmltree::Node;

/// Characters of an `href` kept. Word writes long URLs, but a document that
/// states kilobytes of one is stating something else.
const MAX_HREF_CHARS: usize = 2048;

/// Characters of a bookmark name folded into an id. Names are short by
/// convention (`_Toc123456789`); the cap is what keeps a generated one from
/// putting a megabyte in an attribute.
const MAX_ID_CHARS: usize = 96;

/// Namespace for the ids this renderer mints from document strings, so a
/// bookmark cannot collide with an id the renderer writes itself (`of-fn-…`).
const BOOKMARK_PREFIX: &str = "of-bm-";

/// The destination of one `w:hyperlink`, or `None` when it has none this preview
/// will emit — an unresolvable relationship, an internal target, or a scheme
/// outside the whitelist. `None` is not an error: the run's text still renders,
/// just without an `<a>`.
pub fn href_of(rels: &opc::Rels, n: Node) -> Option<String> {
    // `@r:id` names a relationship whose target is a URI; `@w:anchor` is a jump to
    // a bookmark in this document. A link with both is a URI plus a fragment in
    // the *target* document, which is still the relationship's business, so the
    // anchor is only read when there is no relationship at all.
    if let Some(rid) = attr_local(n, "id") {
        let r = rels.get(rid)?;
        // A non-external target is a package part — another document in the same
        // folder, an embedded object. Not something a reader can follow from here.
        if !r.external {
            return None;
        }
        return sanitize_href(&r.target);
    }
    anchor_href(attr_local(n, "anchor")?)
}

/// A link to a bookmark in this document.
pub fn anchor_href(name: &str) -> Option<String> {
    Some(format!("#{}", bookmark_id(name)?))
}

/// A URI from the document, if it is one of the four shapes that may become an
/// `href`: `http`, `https`, `mailto`, or a bare fragment. Everything else —
/// `javascript:`, `data:`, `file:`, `vbscript:`, a scheme-relative `//host`, a
/// relative path — returns `None`.
pub fn sanitize_href(raw: &str) -> Option<String> {
    // ASCII whitespace and control characters are *removed* rather than rejected,
    // because a browser's URL parser removes them too: `java&#9;script:x` with the
    // tab left in place would fail the prefix test here and still run there.
    let s: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    if s.is_empty() || s.chars().count() > MAX_HREF_CHARS {
        return None;
    }
    if s.starts_with('#') {
        // The stripped string decides *whether* this is a fragment; the name is
        // taken from the raw one, so a bookmark whose name holds a space lands on
        // the same id `w:bookmarkStart` mints for it rather than on a shorter one.
        return anchor_href(raw.trim().trim_start_matches('#'));
    }
    // A whitelist, and matched with the `//` for the two hierarchical schemes:
    // `http:x` is legal and means something a preview cannot resolve.
    let lower = s.to_ascii_lowercase();
    let ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:");
    ok.then_some(s)
}

/// A bookmark name as an id-safe ASCII token, or `None` for a name with nothing
/// in it. Everything outside `[A-Za-z0-9_-]` becomes `-` rather than being
/// dropped: the mapping only has to be *consistent* between the bookmark and the
/// links to it, and dropping non-ASCII would fold every Cyrillic bookmark in a
/// document onto the same empty id.
pub fn bookmark_id(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let body: String = name
        .chars()
        .take(MAX_ID_CHARS)
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '-',
        })
        .collect();
    Some(format!("{BOOKMARK_PREFIX}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_whitelisted_schemes_become_an_href() {
        for ok in [
            "https://example.org/Widget?a=1&b=2#frag",
            "http://example.org",
            "HTTPS://EXAMPLE.ORG",
            "mailto:widget@example.org",
        ] {
            assert_eq!(sanitize_href(ok).as_deref(), Some(ok), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\tscript:alert(1)",
            "  javascript:alert(1)  ",
            "data:text/html;base64,PHNjcmlwdD4=",
            "file:///etc/passwd",
            "vbscript:msgbox",
            // Scheme-relative: inherits the frame's scheme, which is not a scheme
            // this whitelist ever approved.
            "//example.org/Widget",
            // Relative references have no scheme to whitelist.
            "other.docx",
            "/word/document.xml",
            "http:example.org",
            "",
            "   ",
        ] {
            assert_eq!(sanitize_href(bad), None, "{bad}");
        }
        // Length is bounded: the string reaches an attribute.
        assert_eq!(sanitize_href(&format!("https://example.org/{}", "a".repeat(4096))), None);
    }

    #[test]
    fn a_bare_fragment_lands_on_a_sanitized_id() {
        assert_eq!(sanitize_href("#café Widget").as_deref(), Some("#of-bm-caf--Widget"));
        assert_eq!(sanitize_href("#").as_deref(), None);
    }

    #[test]
    fn bookmark_ids_are_ascii_consistent_and_bounded() {
        assert_eq!(bookmark_id("_Toc1234").as_deref(), Some("of-bm-_Toc1234"));
        // A name that could close the attribute or open an element cannot: every
        // character outside the id alphabet is one dash.
        assert_eq!(
            bookmark_id("a\" onmouseover=\"x").as_deref(),
            Some("of-bm-a--onmouseover--x")
        );
        // Non-ASCII maps consistently rather than vanishing, so the link to it
        // still lands.
        assert_eq!(bookmark_id("café"), bookmark_id("café"));
        assert_eq!(bookmark_id("naïve").as_deref(), Some("of-bm-na-ve"));
        assert_eq!(bookmark_id("  "), None);
        assert_eq!(bookmark_id(""), None);
        let long = bookmark_id(&"W".repeat(MAX_ID_CHARS + 50)).expect("an id");
        assert_eq!(long.len(), BOOKMARK_PREFIX.len() + MAX_ID_CHARS);
    }

    #[test]
    fn a_hyperlink_reads_its_relationship_and_falls_back_to_its_anchor() {
        let src = "<root xmlns:w=\"w\" xmlns:r=\"r\">\
             <w:hyperlink r:id=\"rId1\"/><w:hyperlink r:id=\"rId2\"/>\
             <w:hyperlink r:id=\"rId3\"/><w:hyperlink r:id=\"rId9\"/>\
             <w:hyperlink w:anchor=\"café Widget\"/><w:hyperlink/></root>";
        let doc = super::super::xml::parse(src).expect("fixture parses");
        let mut rels = opc::Rels::new();
        let rel = |target: &str, external: bool| opc::Relationship {
            target: target.to_string(),
            kind: "hyperlink".to_string(),
            external,
        };
        rels.insert("rId1".to_string(), rel("https://example.org/", true));
        rels.insert("rId2".to_string(), rel("javascript:alert(1)", true));
        // An internal target: a part of the package, not a destination.
        rels.insert("rId3".to_string(), rel("other.xml", false));
        let hrefs: Vec<Option<String>> = super::super::xml::elems(doc.root_element())
            .map(|n| href_of(&rels, n))
            .collect();
        assert_eq!(
            hrefs,
            [
                Some("https://example.org/".to_string()),
                None,
                None,
                // An rId the relationship table does not hold.
                None,
                Some("#of-bm-caf--Widget".to_string()),
                None,
            ]
        );
    }
}
