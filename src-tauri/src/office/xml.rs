//! Shared XML plumbing for the office extractors: the hardened parser entry
//! point plus the generic text walkers and helpers used by every format.

// Single parser entry point for all document XML. Document XML is untrusted
// input, so DTDs stay disabled: an external entity (`<!ENTITY x SYSTEM
// "file:///etc/passwd">`) would turn a preview into a local-file read (XXE),
// and nested internal entities are the billion-laughs expansion. `nodes_limit`
// bounds peak memory on a hostile-but-well-formed part (MAX_ENTRY_BYTES only
// bounds the input string, not the node arena).
pub fn parse(xml: &str) -> Result<roxmltree::Document<'_>, String> {
    roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 2_000_000,
        },
    )
    .map_err(|e| e.to_string())
}

// ── XML text helper (OOXML: text in <t> elements) ─────────────────────────────

/// Elements that carry no characters but *are* whitespace: a tab, a line break, a
/// carriage return. Emitting nothing for them concatenates the runs on either side
/// into one token, so a tab-separated row indexes as a single unsearchable word.
const OOXML_SPACE_TAGS: &[&str] = &["tab", "br", "cr"];

/// The ODF spellings of the same thing. `text:s` is a run of spaces and `text:tab`
/// a tab; both are elements, never character data.
const ODF_SPACE_TAGS: &[&str] = &["tab", "line-break", "s"];

pub fn xml_text(xml: &str, para_tags: &[&str], text_tags: &[&str]) -> Result<String, String> {
    let doc = parse(xml)?;
    let mut out = String::new();
    xml_walk(doc.root_element(), para_tags, text_tags, &mut out);
    Ok(normalize(&out))
}

pub fn xml_walk(node: roxmltree::Node, para_tags: &[&str], text_tags: &[&str], out: &mut String) {
    let local = node.tag_name().name();
    if text_tags.contains(&local) {
        inner_text(node, out);
        return;
    }
    if node.is_element() && OOXML_SPACE_TAGS.contains(&local) {
        out.push(' ');
        return;
    }
    for child in node.children() {
        xml_walk(child, para_tags, text_tags, out);
    }
    if para_tags.contains(&local) {
        out.push('\n');
    }
}

// ODF text often sits as a direct text node of text:p / text:span rather than
// inside a dedicated <t>; give it a dedicated walker.
pub fn odf_walk(node: roxmltree::Node, out: &mut String) {
    odf_walk_skipping(node, &[], out);
}

/// [`odf_walk`], minus the subtrees whose local name is in `skip`.
///
/// The content index uses it to leave out text the preview does not draw —
/// speaker notes, and the deleted side of a tracked change — because a hit there
/// is a hit no highlight can ever land on.
pub fn odf_walk_skipping(node: roxmltree::Node, skip: &[&str], out: &mut String) {
    if node.is_element() && skip.contains(&node.tag_name().name()) {
        return;
    }
    if node.is_text() {
        if let Some(t) = node.text() {
            out.push_str(t);
        }
        return;
    }
    let local = node.tag_name().name();
    if node.is_element() && ODF_SPACE_TAGS.contains(&local) {
        out.push(' ');
        return;
    }
    for child in node.children() {
        odf_walk_skipping(child, skip, out);
    }
    // A cell's paragraphs, a heading and a list item all end a line. `table-cell`
    // is here because a row of one-paragraph cells would otherwise run together:
    // the cell is the boundary the row's own newline cannot supply.
    if matches!(local, "p" | "h" | "table-cell") {
        out.push('\n');
    }
}

pub fn normalize(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim_end().to_string()
}

// Find an attribute by local name, ignoring its namespace prefix. roxmltree's
// `attribute("val")` matches only the empty namespace, so namespaced attrs like
// `w:val` / `text:outline-level` must be located this way.
pub fn attr_local<'a>(node: roxmltree::Node<'a, 'a>, local: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == local)
        .map(|a| a.value())
}

// ── element lookup ────────────────────────────────────────────────────────────

// Only the *local* name is matched here too: the `a:` / `w:` prefix is
// conventional, not guaranteed, and theme override parts bind the DrawingML
// namespace to a different prefix.

/// First element child with this local name.
pub fn child<'a>(node: roxmltree::Node<'a, 'a>, local: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == local)
}

/// First element in the subtree (`node` included) with this local name.
pub fn descendant<'a>(
    node: roxmltree::Node<'a, 'a>,
    local: &str,
) -> Option<roxmltree::Node<'a, 'a>> {
    node.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == local)
}

/// Element children in document order. Transform and stop lists are
/// order-sensitive, so callers must never collect these into a set.
pub fn elems<'a>(
    node: roxmltree::Node<'a, 'a>,
) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> + 'a {
    node.children().filter(|n| n.is_element())
}

/// First text node in the subtree — the value of a `<v>`-style leaf element.
pub fn text_of<'a>(node: roxmltree::Node<'a, 'a>) -> Option<&'a str> {
    node.descendants().find(|n| n.is_text()).and_then(|n| n.text())
}

/// Appends every descendant text node of `node`. Text is routinely split across
/// sibling runs and `xml:space="preserve"` fragments, so reading only the first
/// one drops most of a document's content.
pub fn inner_text(node: roxmltree::Node<'_, '_>, out: &mut String) {
    for d in node.descendants() {
        if d.is_text() {
            if let Some(t) = d.text() {
                out.push_str(t);
            }
        }
    }
}

/// Whether the subtree holds any non-whitespace text, without allocating it.
pub fn has_inner_text(node: roxmltree::Node<'_, '_>) -> bool {
    node.descendants()
        .any(|d| d.is_text() && d.text().map(|t| !t.trim().is_empty()).unwrap_or(false))
}

// ── typed attributes ──────────────────────────────────────────────────────────

/// The OOXML boolean spelling, plus the `TRUE`/`True`/`on` variants real
/// producers emit. Deliberately lenient: every flag behind it is presentational,
/// so honouring a non-conforming spelling beats silently reading it as false.
pub fn truthy(v: &str) -> bool {
    matches!(v.trim(), "1" | "true" | "TRUE" | "True" | "on")
}

pub fn attr_bool(node: roxmltree::Node<'_, '_>, local: &str) -> Option<bool> {
    attr_local(node, local).map(truthy)
}

pub fn attr_u32(node: roxmltree::Node<'_, '_>, local: &str) -> Option<u32> {
    attr_local(node, local)?.trim().parse().ok()
}

pub fn attr_i64(node: roxmltree::Node<'_, '_>, local: &str) -> Option<i64> {
    attr_local(node, local)?.trim().parse().ok()
}

pub fn attr_f32(node: roxmltree::Node<'_, '_>, local: &str) -> Option<f32> {
    let v: f32 = attr_local(node, local)?.trim().parse().ok()?;
    v.is_finite().then_some(v)
}

// ── natural sort for pptx slide filenames ─────────────────────────────────────

pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.bytes().peekable();
    let mut bi = b.bytes().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_num(&mut ai);
                let nb = take_num(&mut bi);
                match na.cmp(&nb) {
                    Ordering::Equal => continue,
                    ord => return ord,
                }
            }
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => {
                    ai.next();
                    bi.next();
                }
                ord => return ord,
            },
        }
    }
}

pub fn take_num<I: Iterator<Item = u8>>(it: &mut std::iter::Peekable<I>) -> u64 {
    let mut n: u64 = 0;
    while let Some(&d) = it.peek() {
        if !d.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add((d - b'0') as u64);
        it.next();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_doctype_entities() {
        // XXE: a DTD-declared external entity must never be resolved (which
        // would read the referenced file into the document text).
        let xml = r#"<!DOCTYPE foo [<!ENTITY x SYSTEM "file:///etc/passwd">]><r>&x;</r>"#;
        let err = parse(xml).expect_err("DTD must be rejected");
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_rejects_billion_laughs() {
        let xml = r#"<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]><r>&lol2;</r>"#;
        assert!(parse(xml).is_err());
    }

    #[test]
    fn parse_accepts_plain_document() {
        let doc = parse("<r><p>café</p></r>").expect("plain XML parses");
        assert_eq!(doc.root_element().tag_name().name(), "r");
    }

    #[test]
    fn whitespace_only_elements_keep_words_apart() {
        // The recall bug: `<w:tab/>` and `<a:br/>` carry no characters, so a walker
        // that emitted nothing for them turned "café⇥naïve" into one token that no
        // query could ever match.
        let ooxml = "<d><p><t>café</t><tab/><t>naïve</t></p><p><t>one</t><br/><t>two</t></p></d>";
        assert_eq!(
            xml_text(ooxml, &["p"], &["t"]).unwrap(),
            "café naïve\none two"
        );

        let odf = parse("<d><p>café<tab/>naïve</p><p>one<line-break/>two</p></d>").unwrap();
        let mut out = String::new();
        odf_walk(odf.root_element(), &mut out);
        assert_eq!(normalize(&out), "café naïve\none two");
    }

    #[test]
    fn odf_walk_skipping_drops_named_subtrees() {
        let doc = parse("<d><p>kept</p><notes><p>hidden</p></notes></d>").unwrap();
        let mut out = String::new();
        odf_walk_skipping(doc.root_element(), &["notes"], &mut out);
        assert_eq!(normalize(&out), "kept");
        // The skip has to reach a nested subtree, not just a root-level child.
        let deep = parse("<d><body><p>kept</p><notes><p>hidden</p></notes></body></d>").unwrap();
        let mut out = String::new();
        odf_walk_skipping(deep.root_element(), &["notes"], &mut out);
        assert_eq!(normalize(&out), "kept");
    }

    #[test]
    fn natural_cmp_orders_slide_numbers_numerically() {
        assert_eq!(
            natural_cmp("ppt/slides/slide2.xml", "ppt/slides/slide10.xml"),
            std::cmp::Ordering::Less
        );
        assert_eq!(natural_cmp("slide10.xml", "slide2.xml"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("slide2.xml", "slide2.xml"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn normalize_collapses_blank_line_runs() {
        assert_eq!(normalize("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(normalize("café   \n\n\n naïve"), "café\n\n naïve");
    }
}
