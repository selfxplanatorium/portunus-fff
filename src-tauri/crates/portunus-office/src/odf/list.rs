//! `text:list-style` levels resolved into list markers, plus the counter state a
//! body walk advances.
//!
//! A `text:list-style` is ten independent level definitions
//! (`text:list-level-style-number` / `-bullet` / `-image`) keyed by name in the
//! same two-file keyspace as [`super::style`]: named ones in `styles.xml`,
//! automatic ones in `content.xml`, `content.xml` winning a collision. Impress
//! also hangs one *inside* a `style:style/style:graphic-properties`, named after
//! the style that owns it, so the scan reaches into the style containers by
//! descendant rather than by child.
//!
//! ## The counters, and why the call order is the API
//!
//! Numbering is stateful: one [`Lists`] serves exactly one forward pass over the
//! body, and an item's label depends on every call made before it. ODF differs
//! from WordprocessingML ([`super::super::docx::numbering`]) in three ways, and
//! all three land on this API:
//!
//! - **Nesting is physical.** There is no `w:ilvl` to name a level with; the
//!   level *is* the `text:list` nesting depth, so the walker announces entering
//!   and leaving a list ([`Lists::enter`] / [`Lists::leave`]) and the level is
//!   never passed in.
//! - **A new `text:list` restarts** every level it opens, unless it carries
//!   `text:continue-numbering="true"` (continue the last list closed at this
//!   depth) or `text:continue-list="<xml:id>"` (continue one named earlier list —
//!   which the corpus uses to chain four one-item lists into `1.` … `4.`).
//! - **`text:list-header` consumes no number.** It is still indented like an item,
//!   which is what [`Lists::header`] is for — and why it takes `&self`: a header
//!   cannot be the call that advances a counter.
//!
//! The invariant is docx's: **exactly one [`Lists::item`] call per drawn item, in
//! document order**. A doubled or a skipped call corrupts every later label in
//! that list, so a `text:list-item` holding nothing but a nested `text:list`
//! (no paragraph of its own, therefore no label drawn) must not be counted, and a
//! `text:list-item` whose several paragraphs all draw must be counted once — the
//! marker belongs to its first paragraph. A `<text:number>` inside the item is
//! the producer's own pre-rendered label and is ignored; this module regenerates
//! it, because the stored one is stale the moment anything above it changes.
//!
//! Deliberately *not* read: `loext:num-list-format`, the LibreOffice extension
//! that states the whole label as a `%1%.%2%.` template. Every one of the 555
//! numbered levels in the corpus carries it, but it is derived by the producer
//! from the standard attributes rather than being independent data — LibreOffice
//! spells the same fact twice, once as the template and once as
//! `style:num-prefix` / `style:num-format` / `style:num-suffix` — and the two
//! Impress files carry no `loext:` at all, so only the standard attributes cover
//! the whole corpus. See [`Level::display_levels`] for the one case where the
//! template says more than the standard attributes do.
//!
//! The odt body walk (`super::text::emit_list`) is the consumer.

use std::collections::HashMap;
use std::rc::Rc;

use super::super::fonts;
use super::super::listnum::{alpha, decimal, roman};
use super::super::model::{Align, ListMarker};
use super::super::xml::{self, attr_bool, attr_local, attr_u32, child, elems};
use super::length;
use super::style::{self, Family, Styles, TextProps};
use roxmltree::Node;

/// `text:level` is 1..=10, and `text:display-levels` can address no more than
/// this many either.
pub const LEVELS: usize = 10;

// Every table and string below is document-controlled; each gets a cap so a
// generated package cannot make list numbering the expensive part of a preview.
const MAX_LIST_STYLES: usize = 1024;
const MAX_LABEL: usize = 120;
const MAX_NAME: usize = 128;
const MAX_HREF: usize = 512;
const MAX_BULLET_CHARS: usize = 8;
/// A counter saturates here rather than wrapping. Roman numerals already
/// saturate at 3999 and `alpha` grows logarithmically, so this only bounds the
/// decimal spelling.
const MAX_COUNTER: u32 = 1_000_000;
/// Open lists tracked. Past this a list still pairs its enter/leave (see
/// [`Lists::over`]) but gets no marker; ten is the deepest ODF can address.
const MAX_DEPTH: usize = 64;
/// `xml:id`s remembered for `text:continue-list`.
const MAX_CONTINUED: usize = 2048;
/// Letters in a `style:num-letter-sync` label, before [`MAX_LABEL`] would cut it
/// anyway.
const MAX_SYNC_REPS: usize = 16;
/// Widest indent honoured, in px — about 22 inches, the same bound
/// `style::MAX_INDENT_PX` puts on a paragraph's.
const MAX_INDENT_PX: f32 = 2112.0;
/// Stand-in for an image bullet, which [`ListMarker`] has no member for.
const FALLBACK_BULLET: &str = "\u{2022}";

// ── level definitions ────────────────────────────────────────────────────────

/// `style:num-format`. ODF leaves the value open (native numerals, `①`), so
/// anything outside the five standard spellings falls back to decimal, which
/// keeps the item numbered instead of dropping its marker.
///
/// Private: a caller reads the rendered label and [`Marker::bullet`], never the
/// format that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumFmt {
    Decimal,
    LowerLetter,
    UpperLetter,
    LowerRoman,
    UpperRoman,
    /// `style:num-format=""`: a level that counts but shows no numeral. Present
    /// in the corpus (Impress's unnumbered outline level 1, and a Writer level
    /// whose whole label is its `style:num-suffix`).
    None,
}

fn num_fmt(v: &str) -> NumFmt {
    match v.trim() {
        "" => NumFmt::None,
        "a" => NumFmt::LowerLetter,
        "A" => NumFmt::UpperLetter,
        "i" => NumFmt::LowerRoman,
        "I" => NumFmt::UpperRoman,
        _ => NumFmt::Decimal,
    }
}

/// `text:label-followed-by`: what fills the gap between label and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowedBy {
    /// A tab to `text:list-tab-stop-position`. ODF's default, and the only value
    /// in the corpus — the stop itself is not carried, because it lands where
    /// [`LevelIndent::indent_px`] already puts the text.
    ListTab,
    Space,
    Nothing,
}

fn followed_by(v: &str) -> FollowedBy {
    match v.trim() {
        "space" => FollowedBy::Space,
        "nothing" => FollowedBy::Nothing,
        _ => FollowedBy::ListTab,
    }
}

/// A level's indentation, in CSS px, already in the model's own spelling so a
/// caller can drop it straight onto a `Para`.
///
/// ODF states this two ways and the corpus uses both: Writer writes
/// `style:list-level-label-alignment` (`fo:margin-left` / `fo:text-indent`, 700
/// levels), Impress the older `text:space-before` / `text:min-label-width` (630).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelIndent {
    /// Left edge of the item's text (`Para::indent_px`).
    pub indent_px: f32,
    /// First-line offset (`Para::first_line_px`): negative hangs the label left
    /// of the text, which is what every list in the corpus does.
    pub first_line_px: f32,
    pub followed_by: FollowedBy,
    /// `fo:text-align` on the level: how the label sits in the box hung for it.
    /// `end` is 159 of the corpus's levels (right-aligned roman numerals).
    pub align: Option<Align>,
}

impl Default for LevelIndent {
    fn default() -> LevelIndent {
        LevelIndent {
            indent_px: 0.0,
            first_line_px: 0.0,
            followed_by: FollowedBy::ListTab,
            align: None,
        }
    }
}

/// Which of the three `text:list-level-style-*` elements defined a level.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LevelKind {
    Number,
    /// `text:bullet-char`, which is a Symbol/Wingdings code point often enough
    /// that it goes through [`fonts::remap`] before it is drawn.
    Bullet(String),
    /// `xlink:href`, unresolved.
    Image(String),
}

/// One `text:list-level-style-*`.
#[derive(Debug, Clone, PartialEq)]
struct Level {
    kind: LevelKind,
    /// `text:start-value`.
    start: u32,
    fmt: NumFmt,
    /// `style:num-letter-sync`: `aa`, `bb`, `cc` rather than base-26 `aa`, `ab`.
    letter_sync: bool,
    prefix: String,
    suffix: String,
    /// `text:display-levels`: how many levels' numbers the label shows — a
    /// *count*, not a template, so the separator is not stated anywhere. `.` is
    /// what Writer puts between them, and what this module emits. It is also the
    /// one thing `loext:num-list-format` says that the standard attributes do
    /// not, so a document numbering `1-1` rather than `1.1` renders with the
    /// wrong separator. No corpus level states more than one.
    display_levels: usize,
    indent: LevelIndent,
    /// `text:style-name`: a `Family::Text` style that formats the label.
    text_style: Option<String>,
    /// The level's own `style:text-properties`, layered over `text_style`.
    props: TextProps,
}

/// One `text:list-style`: ten levels, any of which the document may leave out.
#[derive(Debug, Clone, PartialEq)]
struct ListStyle {
    levels: Vec<Option<Level>>,
}

// ── what a drawn item shows ──────────────────────────────────────────────────

/// One drawn list item's marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// `None` when the level draws no label at all — an empty
    /// `style:num-format` with no prefix or suffix beside it. The indent still
    /// applies, so that is a `Marker` with no label rather than no `Marker`.
    pub label: Option<ListMarker>,
    pub indent: LevelIndent,
    /// A bullet or image level rather than a numbered one: the label is a glyph,
    /// not a numeral, and nothing about it changes from item to item.
    pub bullet: bool,
    /// `text:list-level-style-image`'s `xlink:href`, as written — the caller
    /// resolves it against the package (`pkg::Entries::resolve_href`). The
    /// `label` beside it is [`FALLBACK_BULLET`], because [`ListMarker`] carries
    /// text and not an image.
    pub image: Option<String>,
}

/// The `text:list` attributes that decide which definition a list uses and which
/// counter it starts from.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListStart<'a> {
    /// `text:style-name`. Absent — or naming a style the document does not
    /// define — inherits the enclosing list's, which is how a nested
    /// `<text:list>` with no attributes at all keeps numbering.
    pub style: Option<&'a str>,
    /// `xml:id`, so a later `text:continue-list` can name this list.
    pub id: Option<&'a str>,
    /// `text:continue-numbering`.
    pub continue_numbering: bool,
    /// `text:continue-list`. Wins over `continue_numbering` when both are
    /// stated: it names one specific list, which is the more specific statement.
    pub continue_list: Option<&'a str>,
}

impl<'a> ListStart<'a> {
    /// Reads the four attributes off a `text:list` element, so a walker cannot
    /// pair the wrong ones up.
    pub fn read(list: Node<'a, 'a>) -> ListStart<'a> {
        ListStart {
            style: attr_local(list, "style-name"),
            id: attr_local(list, "id"),
            continue_numbering: attr_bool(list, "continue-numbering").unwrap_or(false),
            continue_list: attr_local(list, "continue-list"),
        }
    }
}

// One open `text:list`. The level it numbers is its depth in the stack, so the
// counter of every enclosing list is live throughout — which is exactly what
// `text:display-levels` needs to render `1.2.1`.
#[derive(Debug, Clone)]
struct Frame {
    /// Already inherited: the enclosing list's definition when this one names
    /// none, so a lookup never has to walk the stack.
    style: Option<Rc<ListStyle>>,
    id: Option<String>,
    /// `None` means "not yet started", which is distinct from zero: the first
    /// item renders `text:start-value`, not `start + 1`.
    counter: Option<u32>,
    /// A `text:list-item@text:start-value` waiting for the next item.
    pending: Option<u32>,
}

/// A document's list styles, and the counters one walk over its body advances.
pub struct Lists {
    defs: HashMap<String, Rc<ListStyle>>,
    stack: Vec<Frame>,
    /// [`Lists::enter`] calls past [`MAX_DEPTH`], so [`Lists::leave`] still pairs
    /// with them instead of popping a frame that is still open.
    over: usize,
    /// The counter a closed list ended on, by `xml:id`: the only thing
    /// `text:continue-list` can point at.
    by_id: HashMap<String, u32>,
    /// The counter the last list closed at, per depth, for
    /// `text:continue-numbering`.
    closed: [Option<u32>; LEVELS],
}

impl Lists {
    /// A document that defines no list styles, or whose parts would not parse:
    /// every lookup yields `None`. Markers are decoration on top of text, so an
    /// unreadable stylesheet must cost a document its markers and nothing else.
    pub fn empty() -> Lists {
        Lists {
            defs: HashMap::new(),
            stack: Vec::new(),
            over: 0,
            by_id: HashMap::new(),
            closed: [None; LEVELS],
        }
    }

    /// Parses `styles.xml` (absent for a package that ships none) and
    /// `content.xml`. Never fails, for the reason [`Styles::parse`] does not.
    pub fn parse(styles_xml: Option<&str>, content_xml: &str) -> Lists {
        let mut out = Lists::empty();
        for src in [styles_xml, Some(content_xml)] {
            let Some(Ok(doc)) = src.map(xml::parse) else {
                continue;
            };
            out.load(doc.root_element());
        }
        out
    }

    fn load(&mut self, root: Node) {
        for container in elems(root).filter(|e| {
            matches!(
                e.tag_name().name(),
                "styles" | "automatic-styles" | "master-styles"
            )
        }) {
            for e in container
                .descendants()
                .filter(|e| e.is_element() && e.tag_name().name() == "list-style")
            {
                if self.defs.len() >= MAX_LIST_STYLES {
                    return;
                }
                let Some(name) = name_of(e, "name") else {
                    continue;
                };
                self.defs.insert(name, Rc::new(parse_list_style(e)));
            }
        }
    }

    /// Whether the document defines this list style — the walker's cue that a
    /// `text:style-name` is a list style rather than a paragraph style.
    ///
    /// The odt walk needs no such cue: a `text:list` names its style in an
    /// attribute of its own. The odp outline does, because a placeholder's
    /// paragraph style is where Impress hangs the list style.
    #[allow(dead_code)]
    pub fn has(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    /// Open lists. 1 while the walk is inside a top-level `text:list`.
    ///
    /// The odt walk carries its own depth (it bounds more than lists); an odp
    /// outline, whose levels come from `text:outline-level`, reads this one.
    #[allow(dead_code)]
    pub fn depth(&self) -> usize {
        self.stack.len() + self.over
    }

    /// Opens a `text:list`. Pairs with exactly one [`Lists::leave`].
    pub fn enter(&mut self, start: ListStart) {
        if self.stack.len() >= MAX_DEPTH {
            self.over += 1;
            return;
        }
        let style = start
            .style
            .and_then(|n| self.defs.get(n).cloned())
            .or_else(|| self.stack.last().and_then(|f| f.style.clone()));
        // A continuation that names a list nothing closed, or a depth nothing has
        // reached, restarts rather than erroring: the numbering is then wrong by
        // one list instead of absent.
        let depth = self.stack.len();
        let counter = match start.continue_list.and_then(|id| self.by_id.get(id)) {
            Some(n) => Some(*n),
            None if start.continue_numbering => self.closed[depth.min(LEVELS - 1)],
            None => None,
        };
        self.stack.push(Frame {
            style,
            id: start.id.map(|v| v.chars().take(MAX_NAME).collect()),
            counter,
            pending: None,
        });
    }

    /// Closes the innermost `text:list`, remembering where it got to so a later
    /// `text:continue-numbering` or `text:continue-list` can pick it up.
    pub fn leave(&mut self) {
        if self.over > 0 {
            self.over -= 1;
            return;
        }
        let Some(f) = self.stack.pop() else { return };
        let Some(n) = f.counter else {
            // A list with no items leaves the depth's continuation point alone:
            // it is not the list a following `text:continue-numbering` means.
            return;
        };
        self.closed[self.stack.len().min(LEVELS - 1)] = Some(n);
        if let Some(id) = f.id {
            if self.by_id.len() < MAX_CONTINUED {
                self.by_id.insert(id, n);
            }
        }
    }

    /// `text:list-item@text:start-value`: the next item at this depth numbers
    /// from `value`. Call before that item's [`Lists::item`].
    pub fn restart_at(&mut self, value: u32) {
        if let Some(f) = self.stack.last_mut() {
            f.pending = Some(value.clamp(1, MAX_COUNTER));
        }
    }

    /// Advances the innermost list's counter and renders its marker.
    ///
    /// **This mutates.** Call it once per drawn `text:list-item`, in document
    /// order, and never for one whose marker is not drawn — a skipped call is a
    /// skipped number, and a doubled one shifts every label after it.
    ///
    /// `base_pt` is the size the item's paragraph resolves to: a level states its
    /// label's size as a percentage often enough (`45%`, all over the Impress
    /// file) that the marker cannot be sized without it.
    pub fn item(&mut self, styles: &Styles, base_pt: f32) -> Option<Marker> {
        let depth = self.stack.len();
        if depth == 0 {
            return None;
        }
        let idx = depth - 1;
        let style = self.stack[idx].style.clone()?;
        let lvl = style.levels.get(idx.min(LEVELS - 1))?.as_ref()?;

        let frame = &mut self.stack[idx];
        frame.counter = Some(match (frame.pending.take(), frame.counter) {
            (Some(v), _) => v,
            (None, Some(n)) => n.saturating_add(1).min(MAX_COUNTER),
            (None, None) => lvl.start,
        });

        let t = label_props(lvl, styles);
        let family = label_family(&t, styles);
        let text = match &lvl.kind {
            LevelKind::Bullet(ch) => bullet_glyph(ch, family.as_deref()),
            LevelKind::Image(_) => FALLBACK_BULLET.to_string(),
            LevelKind::Number => self.numbered_label(lvl, idx),
        };
        Some(Marker {
            label: (!text.is_empty()).then(|| ListMarker {
                label: text.chars().take(MAX_LABEL).collect(),
                color: t.color.and_then(|c| c.color()),
                font: family.as_deref().map(fonts::css_font_stack),
                // `None` means "the paragraph's", so a level that states no size
                // must not pin one here.
                size_pt: t.size.map(|_| style::size_pt(&t, base_pt)),
            }),
            indent: lvl.indent,
            bullet: !matches!(lvl.kind, LevelKind::Number),
            image: match &lvl.kind {
                LevelKind::Image(href) => Some(href.clone()),
                _ => None,
            },
        })
    }

    /// The innermost level's indentation, without touching a counter: what a
    /// `text:list-header` gets, and what any other caller that needs the current
    /// level's geometry should ask for.
    ///
    /// `&self` by design. A header consumes no number, and a method that cannot
    /// advance a counter cannot be the one that accidentally does.
    pub fn header(&self) -> Option<LevelIndent> {
        let idx = self.stack.len().checked_sub(1)?;
        let style = self.stack[idx].style.as_ref()?;
        Some(style.levels.get(idx.min(LEVELS - 1))?.as_ref()?.indent)
    }

    /// Forget every counter and every open list, so a second walk over the same
    /// body numbers as the first did. A render builds its own [`Lists`] and walks
    /// once, so nothing outside the tests needs this — it stays `cfg(test)`
    /// rather than becoming public API that implies re-walking is supported.
    #[cfg(test)]
    fn reset(&mut self) {
        self.stack.clear();
        self.over = 0;
        self.by_id.clear();
        self.closed = [None; LEVELS];
    }

    // `prefix + numerals + suffix`, where the numerals are this level's and, under
    // `text:display-levels`, its ancestors' — each in *its own* format, which is
    // how `1.a.i` happens.
    fn numbered_label(&self, lvl: &Level, idx: usize) -> String {
        let display = lvl.display_levels.clamp(1, idx + 1);
        let mut out = String::with_capacity(lvl.prefix.len() + lvl.suffix.len() + 8);
        out.push_str(&lvl.prefix);
        let mut first = true;
        for d in (idx + 1 - display)..=idx {
            let Some(n) = self.numeral_at(d) else { continue };
            // A level whose format is empty contributes no numeral, and no
            // separator either: `1..3` is not what the document asked for.
            if n.is_empty() {
                continue;
            }
            if !first {
                out.push('.');
            }
            out.push_str(&n);
            first = false;
        }
        out.push_str(&lvl.suffix);
        out
    }

    // The numeral an enclosing frame currently shows. A frame the walk entered
    // but has not put an item in yet shows its start value rather than dropping
    // out of the label, which is what Writer does too.
    fn numeral_at(&self, d: usize) -> Option<String> {
        let f = self.stack.get(d)?;
        let lvl = f.style.as_ref()?.levels.get(d.min(LEVELS - 1))?.as_ref()?;
        Some(render_num(lvl.fmt, lvl.letter_sync, f.counter.unwrap_or(lvl.start)))
    }
}

fn render_num(fmt: NumFmt, sync: bool, n: u32) -> String {
    match fmt {
        NumFmt::Decimal => decimal(n),
        NumFmt::LowerLetter if sync => alpha_sync(n, false),
        NumFmt::UpperLetter if sync => alpha_sync(n, true),
        NumFmt::LowerLetter => alpha(n, false),
        NumFmt::UpperLetter => alpha(n, true),
        NumFmt::LowerRoman => roman(n, false),
        NumFmt::UpperRoman => roman(n, true),
        NumFmt::None => String::new(),
    }
}

/// `style:num-letter-sync`: past `z` the letter repeats (`aa`, `bb`, `cc`) rather
/// than counting in base 26 (`aa`, `ab`, `ac`).
///
/// Lives here and not beside [`alpha`] in `office::listnum` because base-26 is
/// what every *other* format numbers with; this spelling exists nowhere but ODF,
/// and `listnum`'s three generators are deliberately the shared vocabulary.
fn alpha_sync(n: u32, upper: bool) -> String {
    let n = n.max(1);
    let reps = ((n - 1) / 26 + 1) as usize;
    alpha((n - 1) % 26 + 1, upper).repeat(reps.min(MAX_SYNC_REPS))
}

/// The bullet glyph. Producers author these as Symbol/Wingdings code points
/// (`U+F0B7`, `U+F0A7` — 53 of the corpus's levels), which draw as tofu in any
/// substitute font, so they are folded onto real Unicode when the level names
/// such a font. Anything the tables do not cover keeps its original character.
fn bullet_glyph(ch: &str, font: Option<&str>) -> String {
    let raw = font.unwrap_or("");
    if !fonts::is_symbol_font(raw) {
        return ch.to_string();
    }
    ch.chars().map(|c| fonts::remap(raw, c).unwrap_or(c)).collect()
}

/// The label's own formatting: the level's `text:style-name` resolved in the text
/// family, with the level's own `style:text-properties` layered over it.
fn label_props(lvl: &Level, styles: &Styles) -> TextProps {
    let mut t = match lvl.text_style.as_deref() {
        Some(n) => styles.resolve(Family::Text, n).text.clone(),
        None => TextProps::default(),
    };
    style::merge_text(&mut t, &lvl.props);
    t
}

/// The family the label is drawn in.
///
/// [`style::parse_text_props`] resolved the level's own `style:font-name` against
/// an *empty* face table, because the level is parsed with no document in hand,
/// so a declaration name still needs mapping — which is what
/// [`Styles::font_family`] is for. A name that is already a family maps to
/// itself, so the same call covers `fo:font-family` and the properties that came
/// from the resolved text style.
fn label_family(t: &TextProps, styles: &Styles) -> Option<String> {
    let raw = t.font_raw.as_deref()?;
    Some(styles.font_family(raw).unwrap_or(raw).to_string())
}

// ── parsing ──────────────────────────────────────────────────────────────────

fn name_of(n: Node, attr: &str) -> Option<String> {
    let v = attr_local(n, attr)?.trim();
    (!v.is_empty()).then(|| v.chars().take(MAX_NAME).collect())
}

fn parse_list_style(e: Node) -> ListStyle {
    let mut levels: Vec<Option<Level>> = (0..LEVELS).map(|_| None).collect();
    for l in elems(e) {
        let kind = match l.tag_name().name() {
            "list-level-style-number" => LevelKind::Number,
            "list-level-style-bullet" => LevelKind::Bullet(
                attr_local(l, "bullet-char")
                    .unwrap_or("")
                    .chars()
                    .take(MAX_BULLET_CHARS)
                    .collect(),
            ),
            "list-level-style-image" => LevelKind::Image(
                attr_local(l, "href")
                    .unwrap_or("")
                    .chars()
                    .take(MAX_HREF)
                    .collect(),
            ),
            _ => continue,
        };
        // A level outside the ten ODF defines is dropped rather than clamped onto
        // one of them, which would shadow a level the document really states.
        let Some(level) = attr_u32(l, "level").filter(|v| (1..=LEVELS as u32).contains(v)) else {
            continue;
        };
        levels[level as usize - 1] = Some(parse_level(l, kind));
    }
    ListStyle { levels }
}

fn parse_level(l: Node, kind: LevelKind) -> Level {
    let bullet = !matches!(kind, LevelKind::Number);
    Level {
        kind,
        start: attr_u32(l, "start-value").unwrap_or(1).clamp(1, MAX_COUNTER),
        fmt: attr_local(l, "num-format").map(num_fmt).unwrap_or(NumFmt::Decimal),
        letter_sync: attr_bool(l, "num-letter-sync").unwrap_or(false),
        // `style:num-prefix` / `style:num-suffix` wrap a *numeral*, not a bullet.
        // LibreOffice derives both from `loext:num-list-format` on export by
        // taking everything outside the placeholders, so on a bullet level the
        // "suffix" it writes is the bullet character itself — all 115 bullet
        // levels in the corpus carry `style:num-suffix` equal to their
        // `text:bullet-char`, and honouring it would draw every one of them
        // twice.
        prefix: if bullet { String::new() } else { text_attr(l, "num-prefix") },
        suffix: if bullet { String::new() } else { text_attr(l, "num-suffix") },
        display_levels: attr_u32(l, "display-levels")
            .unwrap_or(1)
            .clamp(1, LEVELS as u32) as usize,
        indent: level_indent(child(l, "list-level-properties")),
        text_style: name_of(l, "style-name"),
        // Parsed through the shared property reader rather than by hand, so the
        // level's label obeys the same `fo:`/`style:` vocabulary a run does. The
        // empty face table is resolved later — see `label_family`.
        props: child(l, "text-properties")
            .map(|t| style::parse_text_props(t, &HashMap::new()))
            .unwrap_or_default(),
    }
}

fn text_attr(n: Node, attr: &str) -> String {
    attr_local(n, attr)
        .unwrap_or("")
        .chars()
        .take(MAX_LABEL)
        .collect()
}

/// `style:list-level-properties`, in either of ODF's two spellings.
///
/// `text:list-level-position-and-space-mode` selects, and its default is the
/// older `label-width-and-position`. The one liberty taken: a
/// `style:list-level-label-alignment` child is honoured even when the mode
/// attribute is missing, *provided* the older attributes are absent too —
/// otherwise a producer that wrote the child and forgot the switch would lose its
/// indents entirely, and there is nothing else for the child to have meant.
fn level_indent(props: Option<Node>) -> LevelIndent {
    let mut out = LevelIndent::default();
    let Some(p) = props else { return out };
    out.align = attr_local(p, "text-align").and_then(align_of);
    let legacy =
        attr_local(p, "space-before").is_some() || attr_local(p, "min-label-width").is_some();
    let label_mode = attr_local(p, "list-level-position-and-space-mode")
        .map(|v| v.trim() == "label-alignment")
        .unwrap_or(false);
    match child(p, "list-level-label-alignment").filter(|_| label_mode || !legacy) {
        Some(a) => {
            out.indent_px = len(a, "margin-left").unwrap_or(0.0);
            out.first_line_px = len(a, "text-indent").unwrap_or(0.0);
            out.followed_by = attr_local(a, "label-followed-by")
                .map(followed_by)
                .unwrap_or(FollowedBy::ListTab);
        }
        None => {
            // `text:space-before` is where the label starts and
            // `text:min-label-width` how much room it gets, so the text starts
            // after both and the label hangs back into that width.
            //
            // `text:min-label-distance` — the *minimum* gap between a label and
            // the text — is not honoured: it only moves the text when the drawn
            // label is wider than `min-label-width`, and nothing here measures a
            // glyph run (the same reason `style::to_para` drops
            // `style:auto-text-indent` and the tab stops). No corpus level states
            // one.
            let before = len(p, "space-before").unwrap_or(0.0);
            let width = len(p, "min-label-width").unwrap_or(0.0);
            out.indent_px = (before + width).clamp(-MAX_INDENT_PX, MAX_INDENT_PX);
            out.first_line_px = -width;
        }
    }
    out
}

fn len(n: Node, attr: &str) -> Option<f32> {
    attr_local(n, attr)
        .and_then(length::parse_len)
        .map(|v| v.clamp(-MAX_INDENT_PX, MAX_INDENT_PX))
}

fn align_of(v: &str) -> Option<Align> {
    Some(match v.trim() {
        "start" | "left" => Align::Left,
        "center" => Align::Center,
        "end" | "right" => Align::Right,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drawingml::color::Color;

    const NS: &str = concat!(
        r#" xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""#,
        r#" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0""#,
        r#" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#,
        r#" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0""#,
        r#" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""#,
        r#" xmlns:xlink="http://www.w3.org/1999/xlink""#,
    );

    fn content(body: &str) -> String {
        format!(
            "<office:document-content{NS}><office:automatic-styles>{body}\
             </office:automatic-styles></office:document-content>"
        )
    }

    fn lists(body: &str) -> Lists {
        Lists::parse(None, &content(body))
    }

    /// One list style whose level 1 is a number in `fmt`, plus whatever extra
    /// attributes the test needs.
    fn numbered(fmt: &str, extra: &str) -> String {
        format!(
            r#"<text:list-style style:name="L">
                 <text:list-level-style-number text:level="1" style:num-format="{fmt}" {extra}/>
               </text:list-style>"#
        )
    }

    fn open(l: &mut Lists) {
        l.enter(ListStart {
            style: Some("L"),
            ..Default::default()
        });
    }

    /// The label of the next item, or `""` when the level draws none.
    fn label(l: &mut Lists) -> String {
        let s = Styles::empty();
        l.item(&s, 12.0)
            .and_then(|m| m.label)
            .map(|m| m.label)
            .unwrap_or_default()
    }

    #[test]
    fn every_num_format_spells_its_own_numeral() {
        for (fmt, want) in [
            ("1", "3"),
            ("a", "c"),
            ("A", "C"),
            ("i", "iii"),
            ("I", "III"),
            // An empty format counts but shows nothing, and with no prefix or
            // suffix beside it there is no label at all.
            ("", ""),
            // ODF leaves the value open; an unknown one still numbers.
            ("\u{2460}", "3"),
        ] {
            let mut l = lists(&numbered(fmt, ""));
            open(&mut l);
            label(&mut l);
            label(&mut l);
            assert_eq!(label(&mut l), want, "format {fmt:?}");
            l.leave();
        }
    }

    #[test]
    fn letter_sync_repeats_the_letter_instead_of_counting_in_base_26() {
        let mut plain = lists(&numbered("a", ""));
        let mut sync = lists(&numbered("a", r#"style:num-letter-sync="true""#));
        open(&mut plain);
        open(&mut sync);
        let mut last = (String::new(), String::new());
        for _ in 0..28 {
            last = (label(&mut plain), label(&mut sync));
        }
        // The two agree up to `aa` (item 27) and part company at 28.
        assert_eq!(last.0, "ab");
        assert_eq!(last.1, "bb");
        assert_eq!(alpha_sync(1, false), "a");
        assert_eq!(alpha_sync(26, true), "Z");
        assert_eq!(alpha_sync(27, false), "aa");
        assert_eq!(alpha_sync(52, false), "zz");
        assert_eq!(alpha_sync(53, false), "aaa");
    }

    #[test]
    fn prefix_and_suffix_wrap_the_numeral() {
        let mut l = lists(&numbered(
            "1",
            r#"style:num-prefix="(" style:num-suffix=")""#,
        ));
        open(&mut l);
        assert_eq!(label(&mut l), "(1)");
        assert_eq!(label(&mut l), "(2)");

        // A level whose numeral is empty is still wrapped: the corpus has one
        // whose whole label is its suffix.
        let mut l = lists(&numbered("", r#"style:num-suffix="-""#));
        open(&mut l);
        assert_eq!(label(&mut l), "-");
    }

    #[test]
    fn display_levels_joins_the_enclosing_numbers() {
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1"
                 style:num-suffix="."/>
              <text:list-level-style-number text:level="2" style:num-format="a"
                 text:display-levels="2"/>
              <text:list-level-style-number text:level="3" style:num-format="i"
                 text:display-levels="3" style:num-prefix="[" style:num-suffix="]"/>
            </text:list-style>"#;
        let mut l = lists(body);
        open(&mut l);
        assert_eq!(label(&mut l), "1.");
        assert_eq!(label(&mut l), "2.");
        // Each shown level is spelled in *its own* format, not the deepest one's.
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "2.a");
        assert_eq!(label(&mut l), "2.b");
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "[2.b.i]");
        assert_eq!(label(&mut l), "[2.b.ii]");
        // Deeper than the level being drawn is not addressable, and a count
        // beyond the current depth clamps to it rather than inventing segments.
        l.leave();
        l.leave();
        assert_eq!(label(&mut l), "3.");
    }

    #[test]
    fn an_unentered_parent_level_shows_its_start_value() {
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1"
                 text:start-value="4"/>
              <text:list-level-style-number text:level="2" style:num-format="1"
                 text:display-levels="2"/>
            </text:list-style>"#;
        let mut l = lists(body);
        open(&mut l);
        // A nested list before any item of the outer one.
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "4.1");
    }

    #[test]
    fn start_value_is_where_the_level_begins() {
        let mut l = lists(&numbered("1", r#"text:start-value="30""#));
        open(&mut l);
        assert_eq!(label(&mut l), "30");
        assert_eq!(label(&mut l), "31");
        // `text:list-item@text:start-value` moves the next item and nothing else.
        l.restart_at(2);
        assert_eq!(label(&mut l), "2");
        assert_eq!(label(&mut l), "3");
    }

    #[test]
    fn bullet_char_and_the_levels_own_font_ride_the_marker() {
        let body = r##"<text:list-style style:name="L">
              <text:list-level-style-bullet text:level="1" text:bullet-char="–"
                 style:num-suffix="–">
                <style:text-properties fo:font-family="'Times New Roman'"
                   fo:color="#191b0e" fo:font-size="45%"/>
              </text:list-level-style-bullet>
              <text:list-level-style-bullet text:level="2" text:bullet-char="&#xF0B7;">
                <style:text-properties fo:font-family="Symbol"
                   style:font-charset="x-symbol"/>
              </text:list-level-style-bullet>
            </text:list-style>"##;
        let mut l = lists(body);
        let s = Styles::empty();
        open(&mut l);
        let m = l.item(&s, 20.0).expect("bullet level");
        let lbl = m.label.expect("bullet has a label");
        // The bullet is drawn once: `style:num-suffix` repeats the character on
        // every bullet level LibreOffice writes, and applying it would double it.
        assert_eq!(lbl.label, "\u{2013}");
        assert_eq!(lbl.color, Some(Color::from_rgb(0x19_1b_0e)));
        assert_eq!(
            lbl.font.as_deref(),
            Some("\"Liberation Serif\", Tinos, Times, serif")
        );
        assert_eq!(lbl.size_pt, Some(9.0));
        assert!(m.bullet);
        assert_eq!(m.image, None);
        // A symbol-font code point folds onto real Unicode.
        l.enter(ListStart::default());
        let m = l.item(&s, 20.0).expect("symbol bullet level");
        assert_eq!(m.label.expect("label").label, "\u{2022}");
        // Nothing about a bullet changes from item to item.
        assert_eq!(label(&mut l), "\u{2022}");
    }

    #[test]
    fn a_level_with_no_stated_size_leaves_the_paragraphs_alone() {
        let mut l = lists(&numbered("1", ""));
        let s = Styles::empty();
        open(&mut l);
        let lbl = l.item(&s, 20.0).and_then(|m| m.label).expect("label");
        assert_eq!(lbl.size_pt, None);
        assert_eq!(lbl.font, None);
        assert_eq!(lbl.color, None);
    }

    #[test]
    fn a_text_style_name_formats_the_label_and_the_level_overrides_it() {
        let styles = format!(
            r##"<office:document-styles{NS}><office:font-face-decls>
                 <style:font-face style:name="OpenSymbol1" svg:font-family="OpenSymbol"/>
               </office:font-face-decls><office:styles>
                 <style:style style:name="ListLabel_20_1" style:family="text">
                   <style:text-properties fo:color="#ff0000" fo:font-size="8pt"
                      fo:font-weight="bold"/>
                 </style:style>
               </office:styles></office:document-styles>"##
        );
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-bullet text:level="1" text:style-name="ListLabel_20_1"
                 text:bullet-char="●">
                <style:text-properties style:font-name="OpenSymbol1" fo:font-size="150%"/>
              </text:list-level-style-bullet>
            </text:list-style>"#;
        let s = Styles::parse(Some(&styles), &content(body));
        let mut l = Lists::parse(Some(&styles), &content(body));
        open(&mut l);
        let lbl = l.item(&s, 12.0).and_then(|m| m.label).expect("label");
        // Colour from the text style, size composed over it (150% of 8pt), and
        // the `style:font-name` resolved through the document's face
        // declarations rather than left as the declaration's own name.
        assert_eq!(lbl.color, Some(Color::from_rgb(0xff_00_00)));
        assert_eq!(lbl.size_pt, Some(12.0));
        assert_eq!(lbl.font.as_deref(), Some("\"OpenSymbol\", sans-serif"));
    }

    #[test]
    fn an_image_level_reports_its_href_beside_a_fallback_glyph() {
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-image text:level="1"
                 xlink:href="Pictures/bullet.png"/>
            </text:list-style>"#;
        let mut l = lists(body);
        let s = Styles::empty();
        open(&mut l);
        let m = l.item(&s, 12.0).expect("image level");
        assert_eq!(m.image.as_deref(), Some("Pictures/bullet.png"));
        assert_eq!(m.label.expect("fallback glyph").label, "\u{2022}");
        assert!(m.bullet);
    }

    #[test]
    fn both_indentation_spellings_land_in_px() {
        // Writer's spelling, which every odt level in the corpus uses.
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1">
                <style:list-level-properties
                   text:list-level-position-and-space-mode="label-alignment"
                   fo:text-align="end">
                  <style:list-level-label-alignment text:label-followed-by="space"
                     fo:margin-left="0.5in" fo:text-indent="-0.25in"/>
                </style:list-level-properties>
              </text:list-level-style-number>
            </text:list-style>"#;
        let l = lists(body);
        let lvl = l.defs["L"].levels[0].as_ref().expect("level");
        assert_eq!(lvl.indent.indent_px, 48.0);
        assert_eq!(lvl.indent.first_line_px, -24.0);
        assert_eq!(lvl.indent.followed_by, FollowedBy::Space);
        assert_eq!(lvl.indent.align, Some(Align::Right));

        // Impress's spelling: the text starts past both lengths and the label
        // hangs back into the width reserved for it.
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-bullet text:level="1" text:bullet-char="●">
                <style:list-level-properties text:space-before="1.2cm"
                   text:min-label-width="0.6cm" text:min-label-distance="0.3cm"/>
              </text:list-level-style-bullet>
            </text:list-style>"#;
        let l = lists(body);
        let ind = l.defs["L"].levels[0].as_ref().expect("level").indent;
        assert!((ind.indent_px - 68.031).abs() < 0.01, "{ind:?}");
        assert!((ind.first_line_px + 22.677).abs() < 0.01, "{ind:?}");
        assert_eq!(ind.followed_by, FollowedBy::ListTab);
        assert_eq!(ind.align, None);

        // A level with no properties at all indents by nothing rather than
        // guessing an indent the document never stated.
        let l = lists(&numbered("1", ""));
        assert_eq!(
            l.defs["L"].levels[0].as_ref().expect("level").indent,
            LevelIndent::default()
        );
    }

    #[test]
    fn the_label_alignment_child_is_read_without_its_mode_switch() {
        // No `text:list-level-position-and-space-mode`, and no older attributes
        // for it to have deferred to.
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1">
                <style:list-level-properties>
                  <style:list-level-label-alignment fo:margin-left="1in"
                     fo:text-indent="-0.5in"/>
                </style:list-level-properties>
              </text:list-level-style-number>
            </text:list-style>"#;
        let l = lists(body);
        let ind = l.defs["L"].levels[0].as_ref().expect("level").indent;
        assert_eq!((ind.indent_px, ind.first_line_px), (96.0, -48.0));

        // With the older attributes present and the switch absent, ODF's default
        // wins and the child is ignored.
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1">
                <style:list-level-properties text:space-before="1in"
                   text:min-label-width="0.5in">
                  <style:list-level-label-alignment fo:margin-left="4in"/>
                </style:list-level-properties>
              </text:list-level-style-number>
            </text:list-style>"#;
        let l = lists(body);
        let ind = l.defs["L"].levels[0].as_ref().expect("level").indent;
        assert_eq!((ind.indent_px, ind.first_line_px), (144.0, -48.0));
    }

    #[test]
    fn a_new_list_restarts_unless_it_continues_numbering() {
        let mut l = lists(&numbered("1", ""));
        open(&mut l);
        assert_eq!(label(&mut l), "1");
        assert_eq!(label(&mut l), "2");
        l.leave();
        // A second list is a second list.
        open(&mut l);
        assert_eq!(label(&mut l), "1");
        l.leave();
        // Unless it says otherwise.
        l.enter(ListStart {
            style: Some("L"),
            continue_numbering: true,
            ..Default::default()
        });
        assert_eq!(label(&mut l), "2");
        l.leave();
        // An empty list in between is not the list a continuation means.
        open(&mut l);
        l.leave();
        l.enter(ListStart {
            style: Some("L"),
            continue_numbering: true,
            ..Default::default()
        });
        assert_eq!(label(&mut l), "3");
        l.leave();
        // A fresh walk starts over.
        l.reset();
        open(&mut l);
        assert_eq!(label(&mut l), "1");
    }

    #[test]
    fn a_nested_list_restarts_its_own_level_and_keeps_the_parents() {
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1"/>
              <text:list-level-style-number text:level="2" style:num-format="1"
                 text:display-levels="2"/>
            </text:list-style>"#;
        let mut l = lists(body);
        open(&mut l);
        assert_eq!(label(&mut l), "1");
        // A nested list inherits the enclosing definition when it names none.
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "1.1");
        assert_eq!(label(&mut l), "1.2");
        l.leave();
        assert_eq!(label(&mut l), "2");
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "2.1");
        l.leave();
        // …and keeps them when it asks to.
        l.enter(ListStart {
            continue_numbering: true,
            ..Default::default()
        });
        assert_eq!(label(&mut l), "2.2");
    }

    #[test]
    fn continue_list_continues_the_list_it_names() {
        // The corpus shape: one-item lists chained by `xml:id`.
        let mut l = lists(&numbered("1", r#"style:num-suffix=".""#));
        l.enter(ListStart {
            style: Some("L"),
            id: Some("list1"),
            ..Default::default()
        });
        assert_eq!(label(&mut l), "1.");
        l.leave();
        // An unrelated list in between must not be what gets continued.
        open(&mut l);
        assert_eq!(label(&mut l), "1.");
        l.leave();
        l.enter(ListStart {
            style: Some("L"),
            id: Some("list2"),
            continue_list: Some("list1"),
            ..Default::default()
        });
        assert_eq!(label(&mut l), "2.");
        l.leave();
        // The chain continues through the list that itself continued.
        l.enter(ListStart {
            style: Some("L"),
            continue_list: Some("list2"),
            ..Default::default()
        });
        assert_eq!(label(&mut l), "3.");
        l.leave();
        // A name nothing closed restarts rather than failing.
        l.enter(ListStart {
            style: Some("L"),
            continue_list: Some("naïve"),
            ..Default::default()
        });
        assert_eq!(label(&mut l), "1.");
    }

    #[test]
    fn a_list_header_consumes_no_number() {
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="1" style:num-format="1">
                <style:list-level-properties text:min-label-width="0.5in"/>
              </text:list-level-style-number>
            </text:list-style>"#;
        let mut l = lists(body);
        open(&mut l);
        assert_eq!(label(&mut l), "1");
        // The header is indented like an item…
        let ind = l.header().expect("level 1 indent");
        assert_eq!((ind.indent_px, ind.first_line_px), (48.0, -48.0));
        // …and the next item is still the second one.
        assert_eq!(label(&mut l), "2");
        // Outside any list there is no indent to report.
        l.leave();
        assert!(l.header().is_none());
    }

    #[test]
    fn missing_and_blank_definitions_degrade_to_no_marker() {
        // A list style that defines level 2 and nothing else.
        let body = r#"<text:list-style style:name="L">
              <text:list-level-style-number text:level="2" style:num-format="1"/>
            </text:list-style>"#;
        let mut l = lists(body);
        let s = Styles::empty();
        open(&mut l);
        assert!(l.item(&s, 12.0).is_none());
        assert!(l.header().is_none());
        l.enter(ListStart::default());
        assert_eq!(label(&mut l), "1");
        l.leave();

        // A list naming a style the document does not define, with nothing to
        // inherit from.
        let mut l = lists(&numbered("1", ""));
        l.enter(ListStart {
            style: Some("café"),
            ..Default::default()
        });
        assert!(l.item(&s, 12.0).is_none());
        l.leave();
        // An item outside any list.
        assert!(l.item(&s, 12.0).is_none());
        // Levels outside the ten ODF defines are dropped, not clamped onto one.
        let l = lists(
            r#"<text:list-style style:name="L">
                 <text:list-level-style-number text:level="0" style:num-format="1"/>
                 <text:list-level-style-number text:level="11" style:num-format="1"/>
               </text:list-style>"#,
        );
        assert!(l.defs["L"].levels.iter().all(|v| v.is_none()));
        // A list style with no name is not addressable and is skipped.
        let l = lists(r#"<text:list-style><text:list-level-style-number text:level="1"/></text:list-style>"#);
        assert!(l.defs.is_empty());
        // Unparseable parts cost the document its markers and nothing else.
        let mut broken = Lists::parse(Some("<office:document-styles>"), "<not xml");
        assert!(!broken.has("L"));
        broken.enter(ListStart::default());
        assert!(broken.item(&s, 12.0).is_none());
        assert!(Lists::empty().header().is_none());
    }

    #[test]
    fn a_list_style_nested_in_a_graphic_style_is_still_indexed_by_name() {
        // Impress's shape: the outline bullets of a presentation style, named
        // after the style that owns them.
        let styles = format!(
            r#"<office:document-styles{NS}><office:styles>
                 <style:style style:name="outline1" style:family="presentation">
                   <style:graphic-properties>
                     <text:list-style style:name="outline1">
                       <text:list-level-style-bullet text:level="1"
                          text:bullet-char="●"/>
                     </text:list-style>
                   </style:graphic-properties>
                 </style:style>
               </office:styles></office:document-styles>"#
        );
        let mut l = Lists::parse(Some(&styles), &content(""));
        assert!(l.has("outline1"));
        l.enter(ListStart {
            style: Some("outline1"),
            ..Default::default()
        });
        assert_eq!(label(&mut l), "\u{25cf}");
    }

    #[test]
    fn document_controlled_sizes_are_capped() {
        let long: String = "café ".repeat(200);
        let body = format!(
            r#"<text:list-style style:name="L">
                 <text:list-level-style-number text:level="1" style:num-format="1"
                    text:start-value="4294967295" style:num-prefix="{long}"
                    text:display-levels="99">
                   <style:list-level-properties text:space-before="99999in"
                      text:min-label-width="99999in"/>
                 </text:list-level-style-number>
                 <text:list-level-style-image text:level="2" xlink:href="{long}"/>
               </text:list-style>"#
        );
        let mut l = lists(&body);
        let s = Styles::empty();
        let lvl = l.defs["L"].levels[0].as_ref().expect("level").clone();
        assert_eq!(lvl.start, MAX_COUNTER);
        assert_eq!(lvl.prefix.chars().count(), MAX_LABEL);
        assert_eq!(lvl.display_levels, LEVELS);
        // An absurd length is refused by the parser and leaves the indent at
        // zero rather than pushing the column off the page.
        assert_eq!(lvl.indent, LevelIndent::default());
        // The rendered label is bounded whatever the prefix was — here it is all
        // prefix, and the numeral is what falls off the end.
        open(&mut l);
        assert_eq!(label(&mut l).chars().count(), MAX_LABEL);
        l.enter(ListStart::default());
        let m = l.item(&s, 12.0).expect("image level");
        assert_eq!(m.image.map(|h| h.chars().count()), Some(MAX_HREF));
        l.leave();

        // A start value past the cap saturates rather than wrapping, and stays
        // there.
        let mut l = lists(&numbered("1", r#"text:start-value="4294967295""#));
        open(&mut l);
        assert_eq!(label(&mut l), MAX_COUNTER.to_string());
        assert_eq!(label(&mut l), MAX_COUNTER.to_string());
        l.leave();

        // Nesting past the depth cap still pairs enter with leave, so a list that
        // closes deep does not leave the shallower counters dangling.
        let mut l = lists(&numbered("1", ""));
        for _ in 0..MAX_DEPTH + 8 {
            open(&mut l);
        }
        assert_eq!(l.depth(), MAX_DEPTH + 8);
        assert!(l.item(&s, 12.0).is_none());
        for _ in 0..MAX_DEPTH + 8 {
            l.leave();
        }
        assert_eq!(l.depth(), 0);
        open(&mut l);
        assert_eq!(label(&mut l), "1");
    }
}
