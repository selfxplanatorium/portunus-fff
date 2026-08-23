#![allow(dead_code)]

pub mod color;
pub mod fill;
pub mod geom;
pub mod line;
pub mod theme;

// Node lookup for the colour/fill/line/geom parsers lives in `office::xml`
// (`child`, `elems`, `descendant`), shared with the xlsx and pptx renderers.
