//! Theme part parsing (`ppt/theme/theme1.xml`, `word/theme/theme1.xml`,
//! `xl/theme/theme1.xml`): the colour scheme and the major/minor font faces that
//! every other DrawingML colour lookup resolves against.

use crate::xml::{self, child, descendant};

/// A concrete slot in `a:clrScheme`. The scheme has exactly twelve slots; the
/// names documents actually *write* are often the mapped aliases (see
/// [`ClrMap`]), never these twelve directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeSlot {
    Dk1,
    Lt1,
    Dk2,
    Lt2,
    Accent1,
    Accent2,
    Accent3,
    Accent4,
    Accent5,
    Accent6,
    Hlink,
    FolHlink,
}

/// The slide-master / document colour map: `tx1`, `bg1`, `tx2`, `bg2` are
/// *indirections*, not scheme slots. `<a:schemeClr val="tx1"/>` means "the slot
/// the colour map points tx1 at", which is `dk1` under the default map but is
/// legal to swap (`<p:clrMap tx1="lt1" bg1="dk1" …/>` on a dark master). The
/// indirection is modelled explicitly instead of being folded into the slot enum
/// so a caller holding the master can honour a non-default map.
///
/// `phClr` is deliberately absent: it is a placeholder filled in by whatever
/// context instantiates a style (theme fill/line styles, table styles), so it
/// resolves in `color.rs`, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClrMap {
    pub tx1: SchemeSlot,
    pub bg1: SchemeSlot,
    pub tx2: SchemeSlot,
    pub bg2: SchemeSlot,
}

impl Default for ClrMap {
    fn default() -> Self {
        ClrMap {
            tx1: SchemeSlot::Dk1,
            bg1: SchemeSlot::Lt1,
            tx2: SchemeSlot::Dk2,
            bg2: SchemeSlot::Lt2,
        }
    }
}

impl ClrMap {
    /// Parse a `<p:clrMap>` / `<p:clrMapOvr>` element. Unknown or missing
    /// attributes keep the default mapping.
    pub fn parse(node: roxmltree::Node<'_, '_>) -> ClrMap {
        let mut map = ClrMap::default();
        let set = |slot: &mut SchemeSlot, attr: &str| {
            if let Some(s) = xml::attr_local(node, attr).and_then(SchemeSlot::from_slot_name) {
                *slot = s;
            }
        };
        set(&mut map.tx1, "tx1");
        set(&mut map.bg1, "bg1");
        set(&mut map.tx2, "tx2");
        set(&mut map.bg2, "bg2");
        map
    }
}

impl SchemeSlot {
    /// A literal `a:clrScheme` slot name only — no alias handling. Used for the
    /// right-hand side of a colour map, where aliases are not legal.
    pub fn from_slot_name(name: &str) -> Option<SchemeSlot> {
        Some(match name {
            "dk1" => SchemeSlot::Dk1,
            "lt1" => SchemeSlot::Lt1,
            "dk2" => SchemeSlot::Dk2,
            "lt2" => SchemeSlot::Lt2,
            "accent1" => SchemeSlot::Accent1,
            "accent2" => SchemeSlot::Accent2,
            "accent3" => SchemeSlot::Accent3,
            "accent4" => SchemeSlot::Accent4,
            "accent5" => SchemeSlot::Accent5,
            "accent6" => SchemeSlot::Accent6,
            "hlink" => SchemeSlot::Hlink,
            "folHlink" => SchemeSlot::FolHlink,
            _ => return None,
        })
    }

    /// Resolve an `a:schemeClr val` under an explicit colour map.
    pub fn resolve(name: &str, map: &ClrMap) -> Option<SchemeSlot> {
        match name {
            "tx1" => Some(map.tx1),
            "bg1" => Some(map.bg1),
            "tx2" => Some(map.tx2),
            "bg2" => Some(map.bg2),
            // dk1/lt1/accentN also appear directly in theme style definitions.
            other => SchemeSlot::from_slot_name(other),
        }
    }

    /// Resolve an `a:schemeClr val` under the default colour map. This is what
    /// the colour parser uses when no master map was threaded in.
    pub fn from_name(name: &str) -> Option<SchemeSlot> {
        SchemeSlot::resolve(name, &ClrMap::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorScheme {
    pub dk1: u32,
    pub lt1: u32,
    pub dk2: u32,
    pub lt2: u32,
    pub accent1: u32,
    pub accent2: u32,
    pub accent3: u32,
    pub accent4: u32,
    pub accent5: u32,
    pub accent6: u32,
    pub hlink: u32,
    pub fol_hlink: u32,
}

impl Default for ColorScheme {
    // The Office 2013+ default ("Office" theme). Used when a package ships no
    // theme part or the part is unparseable, so colours degrade to plausible
    // rather than to black.
    fn default() -> Self {
        ColorScheme {
            dk1: 0x000000,
            lt1: 0xFFFFFF,
            dk2: 0x44546A,
            lt2: 0xE7E6E6,
            accent1: 0x4472C4,
            accent2: 0xED7D31,
            accent3: 0xA5A5A5,
            accent4: 0xFFC000,
            accent5: 0x5B9BD5,
            accent6: 0x70AD47,
            hlink: 0x0563C1,
            fol_hlink: 0x954F72,
        }
    }
}

impl ColorScheme {
    pub fn get(&self, slot: SchemeSlot) -> u32 {
        match slot {
            SchemeSlot::Dk1 => self.dk1,
            SchemeSlot::Lt1 => self.lt1,
            SchemeSlot::Dk2 => self.dk2,
            SchemeSlot::Lt2 => self.lt2,
            SchemeSlot::Accent1 => self.accent1,
            SchemeSlot::Accent2 => self.accent2,
            SchemeSlot::Accent3 => self.accent3,
            SchemeSlot::Accent4 => self.accent4,
            SchemeSlot::Accent5 => self.accent5,
            SchemeSlot::Accent6 => self.accent6,
            SchemeSlot::Hlink => self.hlink,
            SchemeSlot::FolHlink => self.fol_hlink,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub colors: ColorScheme,
    /// Latin typeface of `a:majorFont` (headings).
    pub major_font: String,
    /// Latin typeface of `a:minorFont` (body).
    pub minor_font: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            colors: ColorScheme::default(),
            major_font: "Calibri Light".to_string(),
            minor_font: "Calibri".to_string(),
        }
    }
}

impl Theme {
    /// Parse a theme part. A missing or malformed piece falls back to the
    /// corresponding default rather than failing the whole document.
    pub fn parse(theme_xml: &str) -> Result<Theme, String> {
        let doc = xml::parse(theme_xml)?;
        Ok(Theme::from_root(doc.root_element()))
    }

    pub fn from_root(root: roxmltree::Node<'_, '_>) -> Theme {
        let mut theme = Theme::default();
        // `a:themeElements` holds the real scheme; `a:extraClrSchemeLst` further
        // down the part also contains `a:clrScheme` elements, so look inside
        // themeElements first and only then scan the whole part (theme *override*
        // parts carry the schemes at the root).
        let scope = child(root, "themeElements").unwrap_or(root);
        if let Some(cs) = child(scope, "clrScheme").or_else(|| descendant(root, "clrScheme")) {
            theme.colors = parse_color_scheme(cs);
        }
        if let Some(fs) = child(scope, "fontScheme").or_else(|| descendant(root, "fontScheme")) {
            if let Some(f) = font_of(fs, "majorFont") {
                theme.major_font = f;
            }
            if let Some(f) = font_of(fs, "minorFont") {
                theme.minor_font = f;
            }
        }
        theme
    }

    pub fn color(&self, slot: SchemeSlot) -> u32 {
        self.colors.get(slot)
    }
}

fn parse_color_scheme(node: roxmltree::Node<'_, '_>) -> ColorScheme {
    let mut cs = ColorScheme::default();
    for slot in node.children().filter(|n| n.is_element()) {
        let Some(rgb) = slot_color(slot) else { continue };
        match slot.tag_name().name() {
            "dk1" => cs.dk1 = rgb,
            "lt1" => cs.lt1 = rgb,
            "dk2" => cs.dk2 = rgb,
            "lt2" => cs.lt2 = rgb,
            "accent1" => cs.accent1 = rgb,
            "accent2" => cs.accent2 = rgb,
            "accent3" => cs.accent3 = rgb,
            "accent4" => cs.accent4 = rgb,
            "accent5" => cs.accent5 = rgb,
            "accent6" => cs.accent6 = rgb,
            "hlink" => cs.hlink = rgb,
            "folHlink" => cs.fol_hlink = rgb,
            _ => {}
        }
    }
    cs
}

/// The colour inside one scheme slot. Deliberately *not* `color::parse_color_elem`:
/// resolving a colour needs a `Theme`, and the theme is what is being built here.
/// The schema only allows a plain `a:srgbClr` / `a:sysClr` in a scheme slot, so
/// there are no transforms to apply.
fn slot_color(slot: roxmltree::Node<'_, '_>) -> Option<u32> {
    for c in slot.children().filter(|n| n.is_element()) {
        match c.tag_name().name() {
            "srgbClr" => {
                if let Some(v) = xml::attr_local(c, "val").and_then(parse_hex_rgb) {
                    return Some(v);
                }
            }
            "sysClr" => {
                // `lastClr` is the value the producing app last saw for the
                // system colour; it is the only portable answer.
                if let Some(v) = xml::attr_local(c, "lastClr").and_then(parse_hex_rgb) {
                    return Some(v);
                }
                if let Some(v) = xml::attr_local(c, "val").and_then(sys_color) {
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
}

fn font_of(font_scheme: roxmltree::Node<'_, '_>, which: &str) -> Option<String> {
    let group = child(font_scheme, which)?;
    let latin = child(group, "latin")?;
    let face = xml::attr_local(latin, "typeface")?.trim();
    // An empty typeface means "inherit", `+mj-lt` is a self-reference; both keep
    // the default face.
    if face.is_empty() || face.starts_with('+') {
        return None;
    }
    Some(face.to_string())
}

/// `RRGGBB`, tolerating a `#` prefix and lowercase digits.
pub fn parse_hex_rgb(val: &str) -> Option<u32> {
    let s = val.trim().trim_start_matches('#');
    // Byte-slicing needs the 6-byte prefix to be a char boundary; document XML
    // is untrusted and may hold multi-byte junk here.
    if s.len() < 6 || !s.is_char_boundary(6) {
        return None;
    }
    u32::from_str_radix(&s[..6], 16).ok()
}

/// Fallback for `a:sysClr` without a `lastClr`. Only the handful that actually
/// show up in documents; anything else is left to the caller's default.
pub fn sys_color(name: &str) -> Option<u32> {
    Some(match name {
        "windowText" | "captionText" | "menuText" | "infoText" | "btnText" => 0x000000,
        "window" | "menu" | "highlightText" | "background" | "inactiveCaption" => 0xFFFFFF,
        "btnFace" | "menuBar" | "scrollBar" | "threeDFace" => 0xF0F0F0,
        "btnShadow" | "threeDShadow" | "grayText" => 0x808080,
        "highlight" | "activeCaption" | "hotLight" => 0x0078D7,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEME: &str = r#"<?xml version="1.0"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Widget">
  <a:themeElements>
    <a:clrScheme name="Widget">
      <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
      <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="1F3864"/></a:dk2>
      <a:lt2><a:srgbClr val="EEECE1"/></a:lt2>
      <a:accent1><a:srgbClr val="808080"/></a:accent1>
      <a:accent2><a:srgbClr val="C0504D"/></a:accent2>
      <a:hlink><a:srgbClr val="0000FF"/></a:hlink>
      <a:folHlink><a:srgbClr val="800080"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="Widget">
      <a:majorFont><a:latin typeface="Cambria"/></a:majorFont>
      <a:minorFont><a:latin typeface="Calibri"/></a:minorFont>
    </a:fontScheme>
  </a:themeElements>
  <a:extraClrSchemeLst>
    <a:extraClrScheme>
      <a:clrScheme name="decoy">
        <a:dk1><a:srgbClr val="FF0000"/></a:dk1>
        <a:accent1><a:srgbClr val="FF0000"/></a:accent1>
      </a:clrScheme>
    </a:extraClrScheme>
  </a:extraClrSchemeLst>
</a:theme>"#;

    #[test]
    fn parses_scheme_and_fonts_ignoring_extra_schemes() {
        let t = Theme::parse(THEME).expect("theme parses");
        assert_eq!(t.colors.dk1, 0x000000);
        assert_eq!(t.colors.lt1, 0xFFFFFF);
        assert_eq!(t.colors.dk2, 0x1F3864);
        assert_eq!(t.colors.accent1, 0x808080, "extraClrSchemeLst must not win");
        assert_eq!(t.colors.hlink, 0x0000FF);
        assert_eq!(t.major_font, "Cambria");
        assert_eq!(t.minor_font, "Calibri");
        // Slots absent from the part keep the Office defaults.
        assert_eq!(t.colors.accent3, ColorScheme::default().accent3);
    }

    #[test]
    fn tx_bg_names_alias_onto_dk_lt_slots() {
        let t = Theme::parse(THEME).expect("theme parses");
        assert_eq!(SchemeSlot::from_name("tx1"), Some(SchemeSlot::Dk1));
        assert_eq!(SchemeSlot::from_name("bg1"), Some(SchemeSlot::Lt1));
        assert_eq!(SchemeSlot::from_name("tx2"), Some(SchemeSlot::Dk2));
        assert_eq!(SchemeSlot::from_name("bg2"), Some(SchemeSlot::Lt2));
        assert_eq!(t.color(SchemeSlot::from_name("tx1").unwrap()), 0x000000);
        assert_eq!(t.color(SchemeSlot::from_name("bg1").unwrap()), 0xFFFFFF);
        assert_eq!(t.color(SchemeSlot::from_name("tx2").unwrap()), 0x1F3864);
        assert_eq!(t.color(SchemeSlot::from_name("bg2").unwrap()), 0xEEECE1);
        // phClr is not a theme slot: it is resolved by the calling context.
        assert_eq!(SchemeSlot::from_name("phClr"), None);
        assert_eq!(SchemeSlot::from_slot_name("tx1"), None);
    }

    #[test]
    fn inverted_color_map_swaps_the_aliases() {
        let xml_src = r#"<p:clrMap xmlns:p="p" bg1="dk1" tx1="lt1" bg2="dk2" tx2="lt2"/>"#;
        let doc = xml::parse(xml_src).expect("parses");
        let map = ClrMap::parse(doc.root_element());
        assert_eq!(SchemeSlot::resolve("tx1", &map), Some(SchemeSlot::Lt1));
        assert_eq!(SchemeSlot::resolve("bg1", &map), Some(SchemeSlot::Dk1));
        assert_eq!(
            SchemeSlot::resolve("accent1", &map),
            Some(SchemeSlot::Accent1)
        );
    }

    #[test]
    fn malformed_theme_falls_back_to_defaults() {
        let doc = xml::parse("<a:theme xmlns:a='a'/>").expect("parses");
        assert_eq!(Theme::from_root(doc.root_element()), Theme::default());
        assert!(Theme::parse("not xml at all").is_err());
    }

    #[test]
    fn hex_parse_is_tolerant_but_bounded() {
        assert_eq!(parse_hex_rgb("4472C4"), Some(0x4472C4));
        assert_eq!(parse_hex_rgb("#ff0000"), Some(0xFF0000));
        assert_eq!(parse_hex_rgb("FF00"), None);
        // Multi-byte input must not panic on the 6-byte slice.
        assert_eq!(parse_hex_rgb("naïve0"), None);
        assert_eq!(parse_hex_rgb("café!!"), None);
    }
}
