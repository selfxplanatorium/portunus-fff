//! Footnotes and endnotes: the parts they live in, the marker a reference leaves
//! in the text, and the block they are collected into at the end of the column.
//!
//! Word puts a footnote at the foot of the page that references it. This column
//! is one continuous page with no pagination, so there is no such foot: every
//! referenced note goes to the end, the marker links down to it and the note
//! links back. Both directions matter — a reader who follows a marker past
//! thousands of paragraphs has no way back otherwise — and the relocation is a
//! line in the notes footer, because it is a real difference from the document.
//!
//! A note body is ordinary block content, so it renders through the same
//! [`super::body::walk`] as the page does. That is also why a reference *inside*
//! a note is dropped: the tail block is being written at that point, and a note
//! that references itself would not terminate.

use super::super::html::{attr, attrs, Writer};
use super::super::listnum;
use super::super::model::{Run, Script};
use super::super::xml::{attr_local, elems};
use super::style::RunProps;
use super::{body, Ctx};
use roxmltree::Node;
use std::collections::HashMap;

/// Notes held in the store. Beyond this the *document* is generated: Word's own
/// limit is far lower than anything a reader gets through, and each entry is a map
/// key plus a node.
const MAX_STORED: usize = 2_000;

/// Notes rendered into the tail block, however many are referenced.
const MAX_SHOWN: usize = 64;

/// Bytes the tail block may take of the writer's budget. The notes arrive after
/// all the text the reader asked for, so they may not be what exhausts the cap.
const MAX_TAIL_BYTES: usize = 128 * 1024;

/// Characters of a `w:id`. Ids are small integers; anything longer is not one.
const MAX_ID_CHARS: usize = 16;

pub const NOTE_CAPPED: &str = "Some footnotes not shown";
pub const NOTE_MISSING: &str = "Some footnote text missing";

/// The two note parts are separate id spaces — footnote 2 and endnote 2 are
/// different notes — so the kind travels with every id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Foot,
    End,
}

/// The note bodies of a document, by the id a reference names.
#[derive(Default)]
pub struct Store<'a> {
    foot: HashMap<String, Node<'a, 'a>>,
    end: HashMap<String, Node<'a, 'a>>,
}

impl<'a> Store<'a> {
    /// Indexes the `w:footnote` / `w:endnote` children of the two parts' roots.
    pub fn parse(foot: Option<Node<'a, 'a>>, end: Option<Node<'a, 'a>>) -> Store<'a> {
        Store {
            foot: index(foot),
            end: index(end),
        }
    }

    /// The node is returned with the *store's* lifetime, not the borrow's, so a
    /// caller can hand `ctx` back to the walk while holding it.
    pub fn get(&self, kind: Kind, id: &str) -> Option<Node<'a, 'a>> {
        match kind {
            Kind::Foot => self.foot.get(id).copied(),
            Kind::End => self.end.get(id).copied(),
        }
    }
}

fn index<'a>(root: Option<Node<'a, 'a>>) -> HashMap<String, Node<'a, 'a>> {
    let mut map = HashMap::new();
    let Some(root) = root else {
        return map;
    };
    for n in elems(root) {
        if map.len() >= MAX_STORED {
            break;
        }
        if !matches!(n.tag_name().name(), "footnote" | "endnote") {
            continue;
        }
        // The separator notes are the hairline Word draws above the block and its
        // continuation, not content: ids 0 and 1 in every document Word writes.
        if matches!(
            attr_local(n, "type"),
            Some("separator") | Some("continuationSeparator") | Some("continuationNotice")
        ) {
            continue;
        }
        let Some(id) = note_id(n) else { continue };
        map.insert(id, n);
    }
    map
}

fn note_id(n: Node) -> Option<String> {
    let id = attr_local(n, "id")?.trim();
    (!id.is_empty() && id.len() <= MAX_ID_CHARS).then(|| id.to_string())
}

/// One referenced note: what the tail block renders, and the number the marker
/// shows. Numbered per kind, decimal for footnotes and lower roman for endnotes,
/// which is what Word's defaults look like.
pub struct Ref {
    kind: Kind,
    id: String,
    num: usize,
}

impl Ref {
    fn label(&self) -> String {
        match self.kind {
            Kind::Foot => listnum::decimal(self.num as u32),
            Kind::End => listnum::roman(self.num as u32, false),
        }
    }

    /// Id of the note's block in the tail.
    fn note_anchor(&self) -> String {
        format!("{}{}", self.prefix(), self.num)
    }

    /// Id of the marker in the text, which the note links back to.
    fn ref_anchor(&self) -> String {
        format!("{}ref-{}", self.prefix(), self.num)
    }

    fn prefix(&self) -> &'static str {
        match self.kind {
            Kind::Foot => "of-fn-",
            Kind::End => "of-en-",
        }
    }
}

/// A `w:footnoteReference` / `w:endnoteReference`: registers the note and pushes
/// the marker — an anchor for the note to link back to, then a superscript number
/// linking down to it.
pub fn reference(ctx: &mut Ctx, out: &mut Vec<Run>, n: Node, rp: &RunProps, base_pt: f32) {
    // See the module note: the tail block is mid-write, and a self-referencing
    // note would not terminate.
    if ctx.in_note {
        return;
    }
    let kind = if n.tag_name().name().starts_with("endnote") {
        Kind::End
    } else {
        Kind::Foot
    };
    let Some(id) = note_id(n) else { return };
    // A reference whose body is not in the package drops real text, so it is a
    // note in the footer rather than nothing at all. No marker either: a number
    // that leads nowhere is worse than the honest gap.
    if ctx.note_store.get(kind, &id).is_none() {
        ctx.notes.add(NOTE_MISSING);
        return;
    }
    if ctx.used_notes.len() >= MAX_SHOWN {
        ctx.notes.add(NOTE_CAPPED);
        return;
    }
    let num = ctx.used_notes.iter().filter(|r| r.kind == kind).count() + 1;
    let r = Ref { kind, id, num };
    let (label, target, anchor) = (r.label(), r.note_anchor(), r.ref_anchor());
    ctx.used_notes.push(r);

    out.push(Run::Anchor(anchor));
    let mut run = body::text_run(ctx, label, rp, base_pt, Some(&format!("#{target}")));
    if let Run::Text(t) = &mut run {
        // Word's own footnote reference style is a superscript; a document that
        // states one anyway agrees with this rather than fighting it.
        t.script = Some(Script::Super);
    }
    out.push(run);
}

/// The notes referenced by the page, as one hairline-separated block at the end
/// of the column. Consumes the collected references, so it renders once.
pub fn emit_tail(ctx: &mut Ctx, w: &mut Writer) {
    let used = std::mem::take(&mut ctx.used_notes);
    if used.is_empty() {
        return;
    }
    let start = w.len();
    w.open("div", &attr("class", "of-fnotes"));
    for r in &used {
        if w.is_full() {
            break;
        }
        // The tail's own byte budget, checked between notes: the writer's cap is
        // the whole document's, and a document whose footnotes are longer than its
        // body would spend it here.
        if w.len().saturating_sub(start) > MAX_TAIL_BYTES {
            ctx.notes.add(NOTE_CAPPED);
            break;
        }
        let Some(node) = ctx.note_store.get(r.kind, &r.id) else {
            continue;
        };
        w.open(
            "div",
            &attrs(&[&attr("class", "of-fn"), &attr("id", &r.note_anchor())]),
        );
        // The number is drawn here rather than by the `w:footnoteRef` inside the
        // note body, because that element is the only thing carrying it and a note
        // written without one would have no way back to the text.
        w.open(
            "a",
            &attrs(&[
                &attr("class", "of-fnb"),
                &attr("href", &format!("#{}", r.ref_anchor())),
            ]),
        );
        w.text(&r.label());
        w.close();
        ctx.in_note = true;
        body::walk(ctx, w, node, 0, 0);
        ctx.in_note = false;
        w.close();
    }
    w.close();
}
