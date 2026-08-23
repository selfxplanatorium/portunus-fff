//! Placeholder inheritance: slide → layout → master.
//!
//! A slide shape that is a placeholder states almost nothing itself — its
//! position, fill, list styles and body properties usually live on the matching
//! placeholder of its layout, and that one in turn on the master's. Matching is
//! by `idx` first and `type` second, which is the order PowerPoint uses: two
//! body placeholders on a layout are distinguished only by their index, while a
//! title has no index at all.

use super::super::xml::{child, elems};
use roxmltree::Node;

/// A `<p:ph>` reference. An absent `type` attribute means `body` for layouts and
/// `obj` for slides; both land in the same family below, so the distinction does
/// not need to be carried.
pub struct Ph {
    pub ty: String,
    pub idx: Option<u32>,
}

/// Which of the master's `p:txStyles` entries drives a shape's text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Title,
    Body,
    Other,
}

/// The `<p:ph>` of a shape, if it is a placeholder. Works for `p:sp`, `p:pic`
/// and `p:graphicFrame`, whose non-visual property elements differ in name
/// (`p:nvSpPr`, `p:nvPicPr`, `p:nvGraphicFramePr`) but agree in shape.
pub fn ph_of(sp: Node) -> Option<Ph> {
    let nv = elems(sp).find(|c| {
        let n = c.tag_name().name();
        n.starts_with("nv") && n.ends_with("Pr")
    })?;
    let nv_pr = child(nv, "nvPr")?;
    let ph = child(nv_pr, "ph")?;
    Some(Ph {
        ty: ph.attribute("type").unwrap_or("body").to_string(),
        idx: ph.attribute("idx").and_then(|v| v.parse::<u32>().ok()),
    })
}

pub fn is_placeholder(sp: Node) -> bool {
    ph_of(sp).is_some()
}

/// `p:cNvPr@hidden`. A hidden shape is not drawn by PowerPoint either.
pub fn is_hidden(sp: Node) -> bool {
    elems(sp)
        .find(|c| {
            let n = c.tag_name().name();
            n.starts_with("nv") && n.ends_with("Pr")
        })
        .and_then(|nv| child(nv, "cNvPr"))
        .and_then(|c| c.attribute("hidden"))
        .map(|v| matches!(v, "1" | "true"))
        .unwrap_or(false)
}

/// Title, body and "everything else" collapse the dozen `p:ph` types down to
/// the three the master actually styles.
pub fn style_kind(ty: &str) -> StyleKind {
    match ty {
        "title" | "ctrTitle" => StyleKind::Title,
        "body" | "subTitle" | "obj" => StyleKind::Body,
        _ => StyleKind::Other,
    }
}

/// Placeholder types that substitute for one another when matching. A slide's
/// `obj` placeholder is routinely served by a layout `body`, and `ctrTitle` by a
/// layout `title`.
fn family(ty: &str) -> u8 {
    match ty {
        "title" | "ctrTitle" => 0,
        "body" | "subTitle" | "obj" | "tbl" | "chart" | "pic" | "clipArt" | "dgm" | "media" => 1,
        _ => 2,
    }
}

/// The placeholder of `tree` (a `p:spTree`) that serves `want`.
pub fn find_ph<'a>(tree: Node<'a, 'a>, want: &Ph) -> Option<Node<'a, 'a>> {
    let cands: Vec<(Node<'a, 'a>, Ph)> = elems(tree)
        .filter_map(|n| ph_of(n).map(|p| (n, p)))
        .collect();
    if cands.is_empty() {
        return None;
    }
    // 1. Same index within the same family — the only unambiguous match when a
    //    layout carries several body placeholders.
    if let Some(i) = want.idx {
        if let Some((n, _)) = cands
            .iter()
            .find(|(_, p)| p.idx == Some(i) && family(&p.ty) == family(&want.ty))
        {
            return Some(*n);
        }
    }
    // 2. Same declared type.
    if let Some((n, _)) = cands.iter().find(|(_, p)| p.ty == want.ty) {
        return Some(*n);
    }
    // 3. Same family (title↔ctrTitle, obj↔body).
    if let Some((n, _)) = cands
        .iter()
        .find(|(_, p)| family(&p.ty) == family(&want.ty))
    {
        return Some(*n);
    }
    // 4. Index alone, whatever the type says.
    want.idx
        .and_then(|i| cands.iter().find(|(_, p)| p.idx == Some(i)).map(|(n, _)| *n))
}

/// The master's list style for a shape kind: `p:txStyles/p:titleStyle` and
/// friends.
pub fn tx_style<'a>(master: Option<Node<'a, 'a>>, kind: StyleKind) -> Option<Node<'a, 'a>> {
    let styles = child(master?, "txStyles")?;
    let want = match kind {
        StyleKind::Title => "titleStyle",
        StyleKind::Body => "bodyStyle",
        StyleKind::Other => "otherStyle",
    };
    child(styles, want)
}

/// `p:cSld/p:spTree` of a slide, layout or master.
pub fn sp_tree<'a>(root: Node<'a, 'a>) -> Option<Node<'a, 'a>> {
    child(child(root, "cSld")?, "spTree")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(xml: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(xml).expect("fixture parses")
    }

    #[test]
    fn index_match_wins_over_type_match() {
        let doc = tree(
            "<spTree><sp><nvSpPr><cNvPr/><nvPr><ph type='body' idx='1'/></nvPr></nvSpPr></sp>\
             <sp><nvSpPr><cNvPr/><nvPr><ph type='body' idx='2'/></nvPr></nvSpPr></sp></spTree>",
        );
        let want = Ph {
            ty: "body".into(),
            idx: Some(2),
        };
        let hit = find_ph(doc.root_element(), &want).expect("match");
        let ph = ph_of(hit).expect("ph");
        assert_eq!(ph.idx, Some(2));
    }

    #[test]
    fn ctr_title_matches_a_layout_title() {
        let doc = tree("<spTree><sp><nvSpPr><cNvPr/><nvPr><ph type='title'/></nvPr></nvSpPr></sp></spTree>");
        let want = Ph {
            ty: "ctrTitle".into(),
            idx: None,
        };
        assert!(find_ph(doc.root_element(), &want).is_some());
    }

    #[test]
    fn absent_type_reads_as_body_and_hidden_is_detected() {
        let doc = tree("<sp><nvSpPr><cNvPr hidden='1'/><nvPr><ph idx='3'/></nvPr></nvSpPr></sp>");
        let sp = doc.root_element();
        let ph = ph_of(sp).expect("ph");
        assert_eq!(ph.ty, "body");
        assert_eq!(ph.idx, Some(3));
        assert!(is_hidden(sp));
    }

    #[test]
    fn style_kinds_map_the_ph_types() {
        assert!(matches!(style_kind("ctrTitle"), StyleKind::Title));
        assert!(matches!(style_kind("subTitle"), StyleKind::Body));
        assert!(matches!(style_kind("sldNum"), StyleKind::Other));
    }
}
