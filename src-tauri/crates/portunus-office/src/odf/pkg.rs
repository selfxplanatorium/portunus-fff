//! ODF package layer: the fixed part paths, the encryption check that has to
//! happen before `content.xml` is read, the archive listing an `xlink:href` is
//! validated against, and the document class.
//!
//! There is no OPC-style relationship graph here and `office::opc` is not used:
//! every ODF part lives at a fixed path at the package root, and a reference
//! between parts is a plain package-relative path written straight into the
//! document XML.
//!
//! `super::render` is the consumer; the two later passes add their own classes to
//! its dispatch.

use crate::emit::Notes;
use crate::pkg::{self, Budget, Zip};
use crate::xml::{self, attr_local, child, elems};

const CONTENT: &str = "content.xml";
const STYLES: &str = "styles.xml";
const META: &str = "meta.xml";
const SETTINGS: &str = "settings.xml";
const MANIFEST: &str = "META-INF/manifest.xml";
/// ODF's required first archive member, stored uncompressed.
const MIMETYPE: &str = "mimetype";
/// Where every producer puts embedded media. Only [`Entries::pictures`] reads it.
#[allow(dead_code)]
const PICTURES_DIR: &str = "Pictures/";

/// Longest `xlink:href` considered at all. Zip entry names are far shorter than
/// this in practice, and the cap keeps a hostile megabyte-long href from being
/// percent-decoded before it is rejected.
const MAX_HREF_BYTES: usize = 2048;

/// Marker error for a package whose `content.xml` is encrypted, so the caller can
/// turn it into the password-protected message rather than surfacing the raw
/// string. Same contract as `pkg::BUDGET_EXCEEDED`.
pub const ENCRYPTED: &str = "office: document is password-protected";

/// Which ODF body a package holds, and therefore which renderer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Text,
    Spreadsheet,
    Presentation,
}

/// An opened ODF package: the archive, the shared inflated-bytes budget, the
/// entry listing, and the fixed parts already read.
///
/// The parts are `String`s because `roxmltree::Document` borrows from them, so
/// the renderer has to own them for as long as it walks the tree.
///
/// Destructure this on arrival —
/// `let Package { mut zip, mut budget, entries, class, content, styles, .. } = open(…)?;`
/// — because held whole, `&package.content` and `&mut package.zip` are
/// conflicting borrows of the same value and no media can be read while the tree
/// is alive.
pub struct Package {
    pub zip: Zip,
    pub budget: Budget,
    pub entries: Entries,
    /// From `office:body`'s child, not from the file extension.
    pub class: Class,
    pub content: String,
    /// The real stylesheet: ODF keeps the named styles, the page layout and the
    /// master pages here, and only the automatic styles in `content.xml`. Its
    /// absence is a genuine degradation, so `open` notes it.
    pub styles: Option<String>,
    /// Document properties. A text document's preview shows none of them; the
    /// sheet and slide passes name their sections from a title.
    #[allow(dead_code)]
    pub meta: Option<String>,
    /// View state — the active table, a slide's own settings. Nothing a text
    /// document renders; the sheet pass reads the active sheet from it.
    #[allow(dead_code)]
    pub settings: Option<String>,
}

/// Opens an ODF package and reads its fixed parts.
///
/// A missing or malformed `content.xml` is fatal; a missing `styles.xml`,
/// `meta.xml` or `settings.xml` costs the document some fidelity and lands in
/// `notes` instead.
pub fn open(path: &str, notes: &mut Notes) -> Result<Package, String> {
    let mut zip = pkg::open_zip(path)?;
    let mut budget = Budget::new();
    let entries = Entries(pkg::list_parts(&mut zip, |_| true));

    // Before `content.xml` is touched. An encrypted part is ciphertext, and this
    // is the one place ODF differs dangerously from OOXML: reading it would
    // either fail with a UTF-8 error that says nothing useful or — worse — parse
    // into a plausible-looking nothing. A manifest that cannot be read leaves the
    // check unmade, which is acceptable only because encrypted bytes are
    // effectively never valid UTF-8 and the read then fails anyway.
    if let Ok(Some(m)) = pkg::read_entry(&mut zip, MANIFEST, &mut budget) {
        if manifest_encrypts_content(&m) {
            return Err(ENCRYPTED.to_string());
        }
    }

    let mime_class = pkg::read_entry(&mut zip, MIMETYPE, &mut budget)
        .ok()
        .flatten()
        .and_then(|m| class_from_mimetype(&m));

    let content = pkg::read_entry(&mut zip, CONTENT, &mut budget)?
        .ok_or_else(|| format!("office: {CONTENT} is missing from the package"))?;

    // The renderer parses `content.xml` again for its own walk. Dispatch has to
    // know the class before it can pick a renderer, and a `Document` cannot
    // escape this function while borrowing `content`, so the alternative is a
    // bespoke unhardened scanner for the body tag. Parts top out around a
    // megabyte in the corpus, which is a few milliseconds of roxmltree.
    let class = {
        let doc = xml::parse(&content)?;
        class_from_body(doc.root_element())
            .or(mime_class)
            .ok_or("office: unrecognized OpenDocument body")?
    };

    let styles = optional_part(&mut zip, &mut budget, STYLES, Some(NOTE_STYLES), notes);
    // Silent, both of them: `meta.xml` is the document's properties and
    // `settings.xml` its view state, and a preview shows neither. A footer line
    // exists to explain something the reader can see is wrong.
    let meta = optional_part(&mut zip, &mut budget, META, None, notes);
    let settings = optional_part(&mut zip, &mut budget, SETTINGS, None, notes);

    Ok(Package {
        zip,
        budget,
        entries,
        class,
        content,
        styles,
        meta,
        settings,
    })
}

/// The same sentence the docx path writes when its stylesheet is unreadable: a
/// reader who previews both formats is told the same thing in the same words.
const NOTE_STYLES: &str = "Stylesheet unreadable — text styles missing";

/// A part whose absence costs fidelity but not the render.
///
/// `note` is the reader's sentence, and `None` means the absence has no visible
/// consequence to caption — a footer line about a part nothing draws is noise, and
/// it would push the notes that matter out of a bounded footer. "Not there" and
/// "could not be read" get the same sentence deliberately: to a reader they are the
/// same event, and naming a filename would make the note about the package rather
/// than about the document.
fn optional_part(
    zip: &mut Zip,
    budget: &mut Budget,
    name: &str,
    note: Option<&str>,
    notes: &mut Notes,
) -> Option<String> {
    match pkg::read_entry(zip, name, budget) {
        Ok(Some(x)) => Some(x),
        _ => {
            if let Some(n) = note {
                notes.add(n);
            }
            None
        }
    }
}

// ── document class ───────────────────────────────────────────────────────────

/// The document class from the parsed `content.xml` root.
///
/// `office:body`'s child element is the authoritative signal, ahead of both the
/// `mimetype` entry and the file extension: it *is* the content the renderer has
/// to walk, while the other two are labels a rename or a sloppy producer can
/// leave pointing at the wrong thing. A `.odt` holding `office:spreadsheet`
/// renders as the spreadsheet it is.
///
/// Unrecognized children are skipped rather than failing the lookup, so the
/// `office:drawing` and `office:chart` bodies this preview does not render fall
/// through to the mimetype instead of shadowing a sibling.
fn class_from_body<'a>(root: roxmltree::Node<'a, 'a>) -> Option<Class> {
    let body = child(root, "body")?;
    elems(body).find_map(|n| match n.tag_name().name() {
        "text" => Some(Class::Text),
        "spreadsheet" => Some(Class::Spreadsheet),
        "presentation" => Some(Class::Presentation),
        _ => None,
    })
}

/// The `mimetype` entry, consulted only when the body child is unrecognizable.
/// The template media types render exactly like their document counterparts.
fn class_from_mimetype(mime: &str) -> Option<Class> {
    let m = mime.trim();
    let base = m.strip_suffix("-template").unwrap_or(m);
    match base {
        "application/vnd.oasis.opendocument.text" => Some(Class::Text),
        "application/vnd.oasis.opendocument.spreadsheet" => Some(Class::Spreadsheet),
        "application/vnd.oasis.opendocument.presentation" => Some(Class::Presentation),
        _ => None,
    }
}

// ── encryption ───────────────────────────────────────────────────────────────

/// Whether the manifest marks `content.xml` as encrypted.
///
/// ODF encrypts each part separately and records the algorithm in a
/// `manifest:encryption-data` child of that part's `file-entry`. `content.xml` is
/// the decisive one: a package whose body cannot be decrypted has nothing to
/// render, whatever the state of the parts beside it.
fn manifest_encrypts_content(manifest_xml: &str) -> bool {
    let Ok(doc) = xml::parse(manifest_xml) else {
        return false;
    };
    let hit = elems(doc.root_element())
        .filter(|n| n.tag_name().name() == "file-entry")
        .any(|e| {
            attr_local(e, "full-path").is_some_and(|p| p.trim_start_matches('/') == CONTENT)
                && child(e, "encryption-data").is_some()
        });
    hit
}

// ── entry listing and href resolution ────────────────────────────────────────

/// The archive's entry names, in archive order.
///
/// A `Vec` rather than a set: a package holds tens of entries, an href is
/// resolved a handful of times per render, and archive order is what
/// [`Entries::pictures`] reports.
///
/// Kept as its own value rather than as a method on [`Package`] so a renderer can
/// resolve an href while the archive is mutably borrowed for a media read.
pub struct Entries(Vec<String>);

impl Entries {
    /// The one gate an `xlink:href` from document XML passes through.
    ///
    /// Returns the *archive* entry name, and only when that exact entry exists —
    /// an href is untrusted input, so this validates and resolves in a single
    /// call and there is no separate check a call site can forget.
    ///
    /// Rejected: absolute paths, `..` in any spelling, backslashes, URI schemes
    /// (`http:`, `file:`, `data:`) and Windows drive letters, directory entries,
    /// control characters, and anything the package does not actually contain.
    /// Percent escapes are decoded once before the checks, so `%2e%2e%2f` cannot
    /// smuggle a traversal past them; a second pass is deliberately not made,
    /// because decoding a literal `%2e` inside a real filename twice would be the
    /// bug rather than the fix.
    ///
    /// ODF hrefs are ordinarily package-relative (`Pictures/1000.png`), sometimes
    /// with a `./` prefix, and `content.xml` sits at the package root, so a
    /// relative href is already root-relative and there is nothing above it for a
    /// `..` to resolve against.
    pub fn resolve_href(&self, href: &str) -> Option<&str> {
        let name = sanitize_href(href)?;
        self.0.iter().find(|e| **e == name).map(|e| e.as_str())
    }

    /// Embedded media entries, in archive order. A frame reaches its picture
    /// through [`Entries::resolve_href`]; this is for a pass that has to enumerate
    /// the media a package holds (a sheet's floating images are declared per
    /// table, not per cell).
    #[allow(dead_code)]
    pub fn pictures(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .map(|s| s.as_str())
            .filter(|n| n.starts_with(PICTURES_DIR) && !n.ends_with('/'))
    }
}

/// Normalizes an href to a candidate entry name. Whether the package holds it is
/// [`Entries::resolve_href`]'s check, not this one's.
fn sanitize_href(href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.len() > MAX_HREF_BYTES {
        return None;
    }
    // A fragment or query is not part of an entry name. Splitting rather than
    // rejecting means a hyperlink href cannot accidentally match a part, while a
    // filename that really does hold a `#` simply fails to resolve.
    let href = href.split(['#', '?']).next()?;
    let s = percent_decode(href)?;
    // A colon before the first separator is a URI scheme or a drive letter, never
    // a package-relative path.
    let first_seg = &s[..s.find('/').unwrap_or(s.len())];
    if first_seg.contains(':') {
        return None;
    }
    if s.starts_with('/') || s.contains('\\') {
        return None;
    }
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let mut segs: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            // An empty trailing segment is a directory entry, which is never a
            // resolvable target; dropping it here leaves a name the archive
            // listing will not match.
            "" | "." => continue,
            ".." => return None,
            _ => segs.push(seg),
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(segs.join("/"))
}

/// Percent-decoding, once. A malformed escape yields `None` rather than being
/// left literal: `%` is not a character a producer puts in a package entry name,
/// so a broken escape means the href does not name one.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex_val(*b.get(i + 1)?)?;
            let lo = hex_val(*b.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    Some(match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::TestPkg;

    fn content_xml(body_child: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
 <office:automatic-styles/>
 <office:body><office:{body_child}><text:p>café</text:p></office:{body_child}></office:body>
</office:document-content>"#
        )
        .into_bytes()
    }

    fn mimetype(kind: &str) -> Vec<u8> {
        format!("application/vnd.oasis.opendocument.{kind}").into_bytes()
    }

    fn manifest(extra: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
 <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
{extra}
</manifest:manifest>"#
        )
        .into_bytes()
    }

    /// The `manifest:encryption-data` child that marks `path` as ciphertext.
    fn encrypted_entry(path: &str) -> String {
        format!(
            r#" <manifest:file-entry manifest:full-path="{path}" manifest:media-type="text/xml">
  <manifest:encryption-data manifest:checksum-type="SHA1/1K" manifest:checksum="Y2Fmw6k=">
   <manifest:algorithm manifest:algorithm-name="http://www.w3.org/2001/04/xmlenc#aes256-cbc"
     manifest:initialisation-vector="bmFpdmU="/>
  </manifest:encryption-data>
 </manifest:file-entry>"#
        )
    }

    fn open_ok(pkg: &TestPkg) -> (Package, Vec<String>) {
        let mut notes = Notes::new();
        let p = open(pkg.path(), &mut notes).expect("package opens");
        (p, notes.into_vec())
    }

    /// `Package` holds a `Zip` and is deliberately not `Debug`, so the failure
    /// side is unwrapped by hand rather than through `expect_err`.
    fn open_err(pkg: &TestPkg) -> String {
        let mut notes = Notes::new();
        open(pkg.path(), &mut notes)
            .err()
            .expect("package must not open")
    }

    #[test]
    fn exposes_the_fixed_parts_and_the_media_listing() {
        let pkg = TestPkg::new(
            "odf-parts",
            &[
                ("mimetype", mimetype("text")),
                ("META-INF/manifest.xml", manifest("")),
                ("styles.xml", b"<s xmlns:s='x'/>".to_vec()),
                ("meta.xml", b"<m/>".to_vec()),
                ("settings.xml", b"<c/>".to_vec()),
                ("Pictures/", Vec::new()),
                ("Pictures/1000.png", b"\x89PNG".to_vec()),
                ("Pictures/naive.jpg", b"\xff\xd8\xff".to_vec()),
                ("Thumbnails/thumbnail.png", b"\x89PNG".to_vec()),
                ("content.xml", content_xml("text")),
            ],
        );
        let (p, notes) = open_ok(&pkg);
        assert_eq!(p.class, Class::Text);
        assert!(p.content.contains("café"));
        assert_eq!(p.styles.as_deref(), Some("<s xmlns:s='x'/>"));
        assert_eq!(p.meta.as_deref(), Some("<m/>"));
        assert_eq!(p.settings.as_deref(), Some("<c/>"));
        assert!(notes.is_empty(), "{notes:?}");
        // Archive order, media only: no directory entry and no thumbnail.
        assert_eq!(
            p.entries.pictures().collect::<Vec<_>>(),
            ["Pictures/1000.png", "Pictures/naive.jpg"]
        );
    }

    #[test]
    fn missing_content_is_fatal() {
        let pkg = TestPkg::new(
            "odf-nocontent",
            &[
                ("mimetype", mimetype("text")),
                ("styles.xml", b"<s/>".to_vec()),
            ],
        );
        let err = open_err(&pkg);
        assert!(err.contains("content.xml"), "{err}");
    }

    #[test]
    fn malformed_content_is_fatal() {
        let pkg = TestPkg::new(
            "odf-badcontent",
            &[
                ("mimetype", mimetype("text")),
                ("content.xml", b"<office:body>".to_vec()),
            ],
        );
        let mut notes = Notes::new();
        assert!(open(pkg.path(), &mut notes).is_err());
    }

    #[test]
    fn missing_optional_parts_degrade_and_only_the_visible_one_is_noted() {
        let pkg = TestPkg::new(
            "odf-nostyles",
            &[
                ("mimetype", mimetype("text")),
                ("content.xml", content_xml("text")),
            ],
        );
        let (p, notes) = open_ok(&pkg);
        assert_eq!(p.class, Class::Text);
        assert!(p.styles.is_none() && p.meta.is_none() && p.settings.is_none());
        // One note, not three: the stylesheet is what the reader can see the loss
        // of. Document properties and view state are not drawn at all.
        assert_eq!(notes, vec![NOTE_STYLES.to_string()], "{notes:?}");
    }

    #[test]
    fn encrypted_content_is_reported_and_never_parsed() {
        // Ciphertext, not XML: reaching the parser at all would surface a UTF-8
        // error instead of the honest reason.
        let pkg = TestPkg::new(
            "odf-encrypted",
            &[
                ("mimetype", mimetype("text")),
                (
                    "META-INF/manifest.xml",
                    manifest(&encrypted_entry("content.xml")),
                ),
                ("content.xml", vec![0x00, 0xff, 0xfe, 0x9c, 0x01]),
            ],
        );
        assert_eq!(open_err(&pkg), ENCRYPTED);
    }

    #[test]
    fn encryption_on_another_part_is_not_the_body() {
        let pkg = TestPkg::new(
            "odf-encrypted-other",
            &[
                ("mimetype", mimetype("text")),
                (
                    "META-INF/manifest.xml",
                    manifest(&encrypted_entry("Pictures/1000.png")),
                ),
                ("content.xml", content_xml("text")),
            ],
        );
        let (p, _) = open_ok(&pkg);
        assert_eq!(p.class, Class::Text);
        // A leading slash on the full-path is still the body.
        assert!(manifest_encrypts_content(&String::from_utf8(manifest(
            &encrypted_entry("/content.xml")
        ))
        .unwrap()));
        // An unparseable manifest cannot claim anything.
        assert!(!manifest_encrypts_content("<manifest:manifest"));
    }

    #[test]
    fn body_child_decides_the_class_over_a_mismatched_extension() {
        for (body, mime, want) in [
            ("text", "text", Class::Text),
            ("spreadsheet", "spreadsheet", Class::Spreadsheet),
            ("presentation", "presentation", Class::Presentation),
            // A `.odt` whose body is a spreadsheet renders as what it is, and the
            // stale mimetype beside it does not get a vote.
            ("spreadsheet", "text", Class::Spreadsheet),
            ("presentation", "text", Class::Presentation),
        ] {
            let pkg = TestPkg::new(
                "odf-class",
                &[
                    ("mimetype", mimetype(mime)),
                    ("content.xml", content_xml(body)),
                ],
            );
            let (p, _) = open_ok(&pkg);
            assert_eq!(p.class, want, "body {body}, mimetype {mime}");
        }
    }

    #[test]
    fn mimetype_answers_when_the_body_is_unrecognized() {
        // `office:drawing` has no renderer here, so the label breaks the tie.
        let pkg = TestPkg::new(
            "odf-drawingbody",
            &[
                ("mimetype", mimetype("presentation")),
                ("content.xml", content_xml("drawing")),
            ],
        );
        let (p, _) = open_ok(&pkg);
        assert_eq!(p.class, Class::Presentation);

        // Templates render as their document counterparts.
        assert_eq!(
            class_from_mimetype("application/vnd.oasis.opendocument.spreadsheet-template"),
            Some(Class::Spreadsheet)
        );
        assert_eq!(class_from_mimetype("text/plain"), None);
        assert_eq!(class_from_mimetype(""), None);
    }

    #[test]
    fn an_unrecognizable_class_is_an_error() {
        let pkg = TestPkg::new(
            "odf-noclass",
            &[
                ("mimetype", b"application/octet-stream".to_vec()),
                ("content.xml", content_xml("drawing")),
            ],
        );
        let err = open_err(&pkg);
        assert!(err.contains("body"), "{err}");
    }

    // ── href resolution ──────────────────────────────────────────────────────

    fn entries() -> Entries {
        Entries(
            [
                "mimetype",
                "Pictures/",
                "Pictures/1000.png",
                "Pictures/café.png",
                "Object 1/content.xml",
                "ObjectReplacements/Object 1",
                "content.xml",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        )
    }

    #[test]
    fn resolves_package_relative_hrefs() {
        let e = entries();
        assert_eq!(e.resolve_href("Pictures/1000.png"), Some("Pictures/1000.png"));
        // The `./` prefix real producers emit.
        assert_eq!(
            e.resolve_href("./ObjectReplacements/Object 1"),
            Some("ObjectReplacements/Object 1")
        );
        // Percent escapes are decoded before the archive is consulted, because
        // producers encode spaces and non-ASCII names either way.
        assert_eq!(
            e.resolve_href("./Object%201/content.xml"),
            Some("Object 1/content.xml")
        );
        assert_eq!(
            e.resolve_href("Pictures/caf%C3%A9.png"),
            Some("Pictures/café.png")
        );
        assert_eq!(e.resolve_href("  Pictures/1000.png  "), Some("Pictures/1000.png"));
    }

    #[test]
    fn rejects_traversal_absolute_and_external_hrefs() {
        let e = entries();
        for href in [
            // Traversal, plain and encoded (single and mixed).
            "../secret",
            "../../etc/passwd",
            "..%2Fsecret",
            "%2e%2e/secret",
            "%2E%2E%2Fsecret",
            "Pictures/../Pictures/1000.png",
            "Pictures/%2e%2e/%2e%2e/etc/passwd",
            // Absolute, POSIX and Windows.
            "/etc/passwd",
            "/Pictures/1000.png",
            "%2Fetc%2Fpasswd",
            "\\Pictures\\1000.png",
            "Pictures\\1000.png",
            "C:/Pictures/1000.png",
            // External schemes.
            "http://example.org/1000.png",
            "https://example.org/1000.png",
            "file:///etc/passwd",
            "data:image/png;base64,iVBORw0K",
            "mailto:nobody@example.org",
            // Nothing at all, or a directory rather than a part.
            "",
            "   ",
            "#anchor",
            "Pictures/",
            "Pictures",
            ".",
            "./",
            // Present in the href, absent from the archive.
            "Pictures/missing.png",
            "Pictures/1000.PNG",
            // Malformed escapes and control characters.
            "Pictures/%zz.png",
            "Pictures/1000.png%",
            "Pictures/%00.png",
        ] {
            assert_eq!(e.resolve_href(href), None, "{href:?} must be rejected");
        }
        // Double-encoding is decoded once, so it cannot become a traversal.
        assert_eq!(e.resolve_href("%252e%252e/secret"), None);
        // Over the length cap before anything is decoded.
        let long = format!("Pictures/{}.png", "a".repeat(MAX_HREF_BYTES));
        assert_eq!(e.resolve_href(&long), None);
    }
}
