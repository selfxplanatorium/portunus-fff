// Sandboxed-iframe document scaffolds.
//
// Two consumers with deliberately different postures:
//
//  - `buildSrcdoc` (extension `html` previews) - `sandbox=""`, no scripting at
//    all, and a small utility stylesheet so extension authors get the host's
//    typography for free.
//  - `buildOfficeSrcdoc` (rendered office documents) - `sandbox="allow-scripts"`
//    with a nonce'd bootstrap, because a document preview needs to scroll to its
//    match and answer the host's keyboard.
//
// They share only `themeVarDecls` and `SANDBOX_SCROLLBAR_CSS`. The CSP, the base
// stylesheet and the utility classes are per-consumer on purpose.

import { officeSelectionScript, type FrameSelectionOpts } from './components/office/frameSelection';
import { createViewEngine } from './preview/viewEngine';

// ── shared ───────────────────────────────────────────────────────────────────

/**
 * Host scrollbar look (App.css .text-preview-wrap), for documents long enough
 * to scroll. Only ::-webkit-* rules:
 * scrollbar-width/color would override them on WebKitGTK.
 */
export const SANDBOX_SCROLLBAR_CSS =
  `::-webkit-scrollbar{width:10px;height:10px}` +
  `::-webkit-scrollbar-thumb{background:var(--bg-input);border-radius:5px;` +
  `border:2px solid transparent;background-clip:padding-box;min-height:32px;min-width:32px}` +
  `::-webkit-scrollbar-thumb:hover{background:var(--fg-mute);background-clip:padding-box}` +
  `::-webkit-scrollbar-track{background:transparent}` +
  `::-webkit-scrollbar-corner{background:transparent}`;

/**
 * `--name:value;...` for the named custom properties, read off the host root.
 *
 * Unset vars are dropped rather than emitted empty: an empty custom property
 * makes `var(--x, fallback)` resolve to nothing instead of the fallback.
 */
export function themeVarDecls(names: string[]): string {
  const style = getComputedStyle(document.documentElement);
  return names
    .map(v => [v, style.getPropertyValue(v).trim()] as const)
    .filter(([, value]) => value)
    .map(([v, value]) => `${v}:${value}`)
    .join(';');
}

/**
 * 32 random bytes, base64. Used for both the script nonce and the message token:
 * generated per document by the host and never derived from content, so neither
 * can be predicted or replayed by anything the document happens to contain.
 */
export function randomToken(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

// ── extension `html` previews ────────────────────────────────────────────────

const THEME_VARS = [
  '--fg', '--fg-mute', '--fg-dim', '--fg-desc',
  '--bg', '--bg-deep', '--bg-card',
  '--accent', '--accent-soft', '--accent-border',
  '--radius', '--radius-sm', '--line', '--border', '--text-on-accent',
  // Accent-bleed: the selected result's sampled color, set on documentElement by
  // App.tsx. Flows the album-art / icon hue into the sandboxed preview HTML.
  '--item-accent', '--item-on-accent',
  // Scrollbar thumb, so the iframe's own scrollbars match the host's.
  '--bg-input',
  // UI scale factor. The frame element is unzoomed by --ui-zoom-inv (App.css),
  // so the document re-applies the zoom itself.
  '--ui-zoom',
];

const EXT_UTILS_CSS = [
  '.text-mute{color:var(--fg-mute)}.text-dim{color:var(--fg-dim)}',
  '.text-desc{color:var(--fg-desc)}.text-accent{color:var(--accent)}',
  '.text-xs{font-size:10px;letter-spacing:.04em}.text-sm{font-size:11px}',
  '.text-lg{font-size:16px}.text-hero{font-size:42px;font-weight:200;line-height:1.1}',
  '.text-label{font-size:10px;text-transform:uppercase;letter-spacing:.07em;color:var(--fg-mute)}',
  '.mono{font-family:ui-monospace,"SF Mono","Fira Code",monospace;font-size:12px}',
  '.truncate{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}',
  '.row{display:flex;align-items:center;gap:8px}',
  '.col{display:flex;flex-direction:column;gap:6px}',
  '.fill{flex:1;min-width:0}.between{justify-content:space-between}.wrap{flex-wrap:wrap}',
  '.card{background:var(--bg-card);border-radius:var(--radius-sm);padding:10px 12px}',
  '.surface{background:var(--bg-deep);border-radius:var(--radius-sm);padding:10px 12px}',
  '.divider{height:1px;border:none;background:var(--line);margin:6px 0}',
  '.tag{display:inline-block;font-size:10px;background:var(--accent-soft);border-radius:3px;padding:1px 5px;color:var(--fg-mute)}',
  '.tag-accent{display:inline-block;font-size:10px;background:var(--accent);border-radius:3px;padding:1px 5px;color:var(--text-on-accent)}',
  '.bar{height:3px;border-radius:2px;background:var(--accent)}',
  '.accent-line{border-left:2px solid var(--accent-border);padding-left:8px}',
].join('');

/**
 * Document scaffold for an `html` preview.
 *
 * `:root{zoom}` mirrors the launcher's own UI scale, which App.css cancels on
 * the frame element - see the `.ext-preview-html` comment for why the frame
 * itself must not be zoomed by its parent document.
 *
 * Two rules that must not be reintroduced: no percentage/viewport heights, and
 * no `overflow` on `body`. A body locked to the viewport height keeps that
 * height when a horizontal scrollbar appears, so it overflows by exactly the
 * scrollbar's thickness and grows a spurious vertical one; and `overflow` on
 * body adds a second scroll container beside the viewport's. Body is
 * content-sized, the viewport is the only scroller, nothing is ever clipped.
 */
export function buildSrcdoc(content: string): string {
  const vars = themeVarDecls(THEME_VARS);
  return (
    `<!DOCTYPE html><html><head>` +
    `<meta http-equiv="Content-Security-Policy" ` +
    `content="default-src 'none'; style-src 'unsafe-inline' data:; img-src data:;">` +
    `<style>:root{${vars};zoom:var(--ui-zoom,1)}` +
    `*{box-sizing:border-box;margin:0;padding:0}` +
    `body{background:transparent;color:var(--fg);font-size:13px;line-height:1.5;` +
    `font-family:system-ui,-apple-system,sans-serif}` +
    SANDBOX_SCROLLBAR_CSS +
    `${EXT_UTILS_CSS}</style>` +
    `</head><body>${content}</body></html>`
  );
}

// ── rendered office documents ────────────────────────────────────────────────

/** Which renderer produced the document (mirrors the backend `Shape`). */
export type OfficeVariant = 'doc' | 'sheet' | 'slide';

export interface OfficeSrcdocOpts {
  /** Per-document random token every inbound message must carry. */
  token: string;
  /** `id` of the mark to centre before `ready` (OfficeDoc.bestMarkId). */
  bestMarkId?: string | null;
  /** Start with matched-term highlighting suppressed (Ctrl+H is off). */
  hlOff?: boolean;
  /** Slide canvas size in CSS px (OfficeDoc.natural). */
  natural?: [number, number] | null;
  /** docx page width + padding in CSS px (OfficeDoc.page). */
  page?: [number, number, number] | null;
  /**
   * Reader zoom to open at. Baked in rather than sent after `ready` so a sheet
   * switch or a new file does not visibly snap from 100% back to the user's zoom.
   */
  zoom?: number;
  /**
   * Lower zoom bound for this document. A slide is a fixed canvas that has to be
   * *shrunk* to fit a 340px side panel - often past OFFICE_ZOOM_MIN - so the floor
   * is per-document rather than a constant. Omitted means OFFICE_ZOOM_MIN.
   */
  zoomMin?: number;
}

/** Wrapper the reader's zoom transform is applied to. */
const OFFICE_ZOOM_ID = 'ozoom';

/** Hidden viewport-sized probe the frame measures itself against. */
const OFFICE_VP_ID = 'ovp';

/**
 * The viewport probe. `position:fixed` with 100% of each axis *is* the viewport by
 * construction, so its layout box is the viewport expressed in the same pre-scale
 * px the content is laid out in - and the ratio of its *client* rect to that box
 * is how the frame converts pointer coordinates into those px, whatever the root's
 * own `zoom` does to the mapping between them.
 */
const OFFICE_VP_CSS =
  `#${OFFICE_VP_ID}{position:fixed;left:0;top:0;width:100%;height:100%;` +
  `visibility:hidden;pointer-events:none}`;

/**
 * What counts as selectable text per variant, and what chrome a drag may cross
 * without selecting it.
 *
 * A sheet is narrow on purpose: `xl-t` is the renderer's mark for "this cell has
 * something in it", so a press on a filled cell selects and a press on an empty
 * one pans, and the row/column gutter is excluded outright - its numbers and
 * letters are chrome, and letting a drag pick them up would put them in the copied
 * TSV. A slide is the same idea one level up: `pp-tb` is a shape's text box and
 * `pp-tbl` its table, so a press inside either selects and a press on the canvas,
 * a picture or a placeholder pans. The doc variant needs no such split: a page is
 * text throughout, so left-drag selects anywhere and panning is middle-drag only.
 *
 * A docx table needs nothing here either - the engine's cell and block detection
 * is by tag (`closest('td,th')`, `BLOCK_TAGS`), not by any renderer's class, so a
 * selection across `.of-tc` cells already copies as TSV and one across headings
 * already breaks by line.
 */
/**
 * The element a variant's viewport-pinned chrome lives on, if it has any: the
 * frame publishes `--fx`/`--fy` on it - how far the content has been panned past
 * its origin - and the renderer's own stylesheet counter-translates whatever must
 * stay pinned (a sheet's frozen panes). Empty means the variant has none, and the
 * bootstrap emits no pinning code at all.
 */
const PIN_HOSTS: Record<OfficeVariant, string> = {
  sheet: '.xl-frozen',
  doc: '',
  slide: '',
};

const SELECTORS: Record<OfficeVariant, FrameSelectionOpts> = {
  sheet: { text: '.xl-t', exclude: 'th', host: '' },
  // The docx renderer wraps its page in `.of-page`. That class is the doc shape's
  // own - deliberately not the sheet renderer's `.xl-doc` root, which is
  // `Shape::Sheet` and carries the sheet's own CSS.
  doc: { text: '.of-page', exclude: '', host: '.of-page' },
  slide: { text: '.pp-tb,.pp-tbl', exclude: '', host: '.pp-doc' },
};

/** Reader zoom bounds. Below 0.4 a sheet is unreadable; above 3 nothing fits. */
export const OFFICE_ZOOM_MIN = 0.4;
export const OFFICE_ZOOM_MAX = 3;
export const OFFICE_ZOOM_STEP = 1.1;

export function clampOfficeZoom(z: number, min = OFFICE_ZOOM_MIN): number {
  if (!Number.isFinite(z)) return 1;
  const lo = Number.isFinite(min) && min > 0 ? Math.min(min, OFFICE_ZOOM_MIN) : OFFICE_ZOOM_MIN;
  return Math.round(Math.min(OFFICE_ZOOM_MAX, Math.max(lo, z)) * 100) / 100;
}

/**
 * Custom properties an office document may read. Deliberately narrower than
 * THEME_VARS: the *chrome* is themed (row/column headers, notes, match
 * highlights), the *paper* is not. A document authors its own ink and
 * fills, and recolouring those to match the launcher's palette would
 * misrepresent the file's contents.
 */
const OFFICE_THEME_VARS = [
  '--fg', '--fg-mute', '--fg-dim',
  '--bg-preview', '--bg-deep', '--bg-card',
  '--accent', '--line', '--border', '--radius-sm',
  // The document re-applies the launcher's UI scale itself: App.css cancels it on
  // the frame element (see `.office-frame`), and the frame's own reader converts
  // pointer coordinates through it.
  '--ui-zoom',
];

/**
 * Structural CSS shared by all three office variants.
 *
 * Every selector is a single class or element so the document's own per-format
 * rules (emitted after this block, at equal specificity) win on source order -
 * the same discipline the backend's BASE_CSS keeps.
 */
const OFFICE_BASE_CSS =
  `*{box-sizing:border-box;margin:0;padding:0}` +
  // Match highlighting. Mirrors `mark.preview-hl` in App.css - #fff is ink over
  // the accent fill (as it is there), not a theme surface. No padding: office
  // cells are `white-space:pre;overflow:hidden`, so a mark that grows the inline
  // box would shift the grid.
  `mark.preview-hl{background:color-mix(in srgb,var(--accent) 70%,transparent);` +
  `color:#fff;font-weight:600;border-radius:2px}` +
  // Ctrl+H toggle: a class flip on <html>, so it costs no reload and no reflow
  // of the document's own styles.
  `html.hl-off mark.preview-hl{background:none;color:inherit;font-weight:inherit}` +
  // Text selection (frameSelection.ts). Mirrors .sel-overlay / .sel-rect /
  // .sel-caret in App.css, including the multiply blend the host uses over white
  // PDF pages and images - office paper is white for the same reason, and the
  // blend is what keeps dark glyphs legible through the tint.
  `.osel{position:absolute;left:0;top:0;width:0;height:0;overflow:visible;` +
  `pointer-events:none;z-index:1}` +
  // Scale probe: a hidden box of known CSS size, measured to convert painted
  // client rects into the coordinates the rects are written in. See scaleOf().
  `.osel .osp{position:absolute;left:0;top:0;width:100px;height:100px;visibility:hidden}` +
  `.osel .osr{position:absolute;background:color-mix(in srgb,var(--accent) 40%,transparent);` +
  `mix-blend-mode:multiply;border-radius:2px}` +
  `.osel .osc{position:absolute;width:2px;background:var(--accent);` +
  `box-shadow:0 0 4px color-mix(in srgb,var(--accent) 50%,transparent);` +
  `animation:osel-blink 1.1s steps(1) infinite}` +
  `.osel .osc.mv{animation:none}` +
  `@keyframes osel-blink{50%{opacity:0}}` +
  // The engine keeps a live native selection as the caret's home (WebKit's goal
  // column for vertical movement lives on it), but draws its own rects on top. So
  // the native painting has to go, or every selection is tinted twice.
  `::selection{background:transparent;color:inherit}`;

/**
 * Per-variant scaffold.
 *
 * Nothing here scrolls: `overflow:hidden` on html and body, and every bit of
 * movement is the wrapper's `transform`, written by the bootstrap. Why that is not
 * negotiable is stated once, in `preview/viewEngine`.
 *
 * Two properties of these rules are load-bearing, and both are about keeping a
 * zoom free of layout:
 *
 *  - Every wrapper is content-sized (`width:max-content`) and never sized against
 *    the viewport. A width that depended on the zoom - `calc(100% / z)` - makes
 *    zooming relayout rather than scale: at 2x the column halves, so anything
 *    fitted to it re-wraps, anything centred in it re-centres (a centred title
 *    visibly slides sideways as you zoom) and anything carrying real px widths
 *    overflows it. Centring a document narrower than the frame is the engine's
 *    `recenterFit`, not a flex box.
 *  - Reader zoom is a `transform`, not `zoom`. `zoom` scales computed font-size,
 *    and WebCore then re-applies its "smart minimum" to the result
 *    (`computedFontSizeFromSpecifiedSize`: a size that was legible before zoom is
 *    floored at `minimumLogicalFontSize`, default 9px). The sheet's base font is
 *    14.667px, so every zoom below ~0.61 produced the same 9px text while the boxes
 *    around it kept shrinking. WebKitGTK exposes only the *hard* minimum
 *    (`minimum-font-size`, already 0); the smart minimum has no public setter.
 *
 * The launcher's own `--ui-zoom` stays on `zoom` - it is the UI scale the rest of
 * the app uses, and it is not a per-document control.
 */
function officeVariantCss(variant: OfficeVariant, opts: OfficeSrcdocOpts): string {
  const root =
    `:root{zoom:var(--ui-zoom,1)}` +
    // Body is the transform's containing block and the paper's backdrop; the
    // wrapper is positioned out of flow so its box can never feed back into body's.
    // The transform itself is only ever written by the bootstrap, which runs before
    // the first paint - a static opening scale here would just be a wrong frame
    // (scaled but not yet positioned) for any paint that beat it.
    `html,body{height:100%;overflow:hidden}` +
    `body{background:var(--bg-preview);position:relative}` +
    `#${OFFICE_ZOOM_ID}{position:absolute;left:0;top:0;transform-origin:0 0;` +
    `width:max-content}` +
    OFFICE_VP_CSS;
  switch (variant) {
    case 'sheet':
      // The grid is a `table-layout:fixed` table of explicit column widths, so it
      // is content-sized with nothing to size against.
      return root + `body.pannable{cursor:grab}`;
    case 'doc': {
      // Page geometry arrives as [width, padX, padY] in CSS px. `width` is the
      // *whole* page including its margins - `box-sizing:border-box` from the base
      // CSS is what makes the padding sit inside it, so a Letter page is 816px wide
      // and not 816 + 2 x 96. The fallback is US Letter with 1in margins, for a
      // document whose renderer reported no geometry at all. A renderer with
      // asymmetric margins overrides the padding from its own stylesheet, which
      // comes later in document order.
      const [w, px, py] = opts.page ?? [816, 96, 96];
      // Laid out once at the width the document was authored for, with no
      // `max-width:100%`: a percentage cap is what made zoom relayout the page -
      // shrinking it below its authored width re-centres centred paragraphs (a
      // title slides sideways and clips as you zoom in) and pushes content that
      // carries real px widths - a table with a px `<colgroup>` - off the paper.
      //
      // Vertical air and a shadow so the paper reads as a sheet lying on the
      // chrome rather than as the frame's own background. Deliberately a shadow and
      // not a border: in this UI a border means interactive.
      return (
        root +
        `.of-page{width:${w}px;padding:${py}px ${px}px;background:#fff;color:#000;` +
        `margin:10px 0;box-shadow:0 1px 10px rgba(0,0,0,0.3)}`
      );
    }
    case 'slide':
      // A slide is a fixed canvas (the renderer emits its px size inline), so the
      // wrapper is content-sized exactly as the sheet's is. Everything outside a
      // text box pans, so the whole canvas offers the grab cursor and the
      // renderer's `cursor:text` rules take it back where text is.
      return (
        root +
        `.pp-doc{box-shadow:0 1px 10px rgba(0,0,0,.35)}` +
        `body.pannable{cursor:grab}`
      );
  }
}

/**
 * The one piece of trusted script in an office preview, emitted here rather than
 * by the Rust renderer so it lives in a single reviewable place: the renderer
 * only ever produces inert markup, and any script that shows up in a document is
 * by definition an escaping bug (and is blocked by the nonce'd CSP).
 *
 * Runs last in <body>, so the document is parsed by the time it executes. It
 * centres the best match *before* posting `ready`, which is the host's cue to
 * reveal the buffer - the match is on screen in the first painted frame.
 *
 * It owns the reader's whole position - pan and zoom, as one transform, over the
 * shared view engine - and two things the host cannot reach across the frame
 * boundary: *focus custody* (a click inside a subframe moves focus there, and the
 * host's card-level `mousedown` preventDefault never sees it) and the *wheel*
 * (delivered to the frame, never to the host).
 */
function officeBootstrap(variant: OfficeVariant, opts: OfficeSrcdocOpts): string {
  const token = JSON.stringify(opts.token);
  const mark = JSON.stringify(opts.bestMarkId ?? null);
  const zoom = clampOfficeZoom(opts.zoom ?? 1, opts.zoomMin);
  const zmin = clampOfficeZoom(opts.zoomMin ?? OFFICE_ZOOM_MIN, opts.zoomMin);
  // Only slides carry fixed-size text boxes that PowerPoint shrinks text to fit.
  const autofits = variant === 'slide';
  const pinSel = PIN_HOSTS[variant];
  return (
    `(function(){` +
    `var T=${token};` +
    // `parent` is the app origin; targetOrigin '*' because this document's own
    // origin is opaque and there is nothing here worth withholding - the token is
    // what authenticates traffic in the other direction.
    `var post=function(m){m.token=T;parent.postMessage(m,'*')};` +
    `var root=document.documentElement;` +
    `var W=document.getElementById(${JSON.stringify(OFFICE_ZOOM_ID)});` +
    `var VP=document.getElementById(${JSON.stringify(OFFICE_VP_ID)});` +
    // ── the view ──
    // The reader's whole position is `{z,tx,ty}`, and every gesture is one call into
    // the shared engine, stringified in from src/preview/viewEngine.ts - the same code
    // object the PDF reader runs. It is a closure factory precisely so it can cross
    // this boundary: a reference out of it would arrive here as a minified name that
    // does not exist in this document. Bounds and steps are this reader's policy and
    // are handed to the factory, so nothing below multiplies a zoom factor by hand.
    `var V=(${String(createViewEngine)})(` +
    `{min:${zmin},max:${OFFICE_ZOOM_MAX},` +
    `step:${OFFICE_ZOOM_STEP},wheelStep:${OFFICE_ZOOM_STEP},snap:100});` +
    `var view={z:${zoom},tx:0,ty:0};` +
    // Geometry in *layout* px: the content at z=1, and the viewport. `US` is the
    // painted-per-layout ratio (the launcher's UI scale, which this document
    // re-applies as `zoom` on :root), with `OX`/`OY` the viewport's origin in client
    // px - together they turn a pointer event into the space the view lives in.
    // Measured from the fixed probe rather than assumed, because WebKitGTK hands a
    // zoomed frame a layout viewport that disagrees with its painted box.
    //
    // Measured on the events that can change it and cached in between. Nothing a
    // gesture does can: a transform does not affect layout, so the content box is
    // fixed, and the probe follows the frame. Reading it per gesture instead would
    // force a synchronous layout of the whole document on every wheel tick, right
    // after `paint` dirtied it - the read-after-write thrash this reader is built to
    // avoid.
    `var G={cw:0,ch:0,vw:0,vh:0},US=1,OX=0,OY=0;` +
    `var measure=function(){` +
    `if(VP){var b=VP.getBoundingClientRect();` +
    `G.vw=VP.offsetWidth;G.vh=VP.offsetHeight;` +
    `US=G.vw>0?b.width/G.vw:1;OX=b.left;OY=b.top;}` +
    `if(W){G.cw=W.offsetWidth;G.ch=W.offsetHeight;}};` +
    // Client px in, layout px out. Deltas pass through the scale alone, points also
    // through the viewport's origin.
    `var ld=function(d){return d/US;};` +
    `var lx=function(c){return (c-OX)/US;};` +
    `var ly=function(c){return (c-OY)/US;};` +
    // ── applying it ──
    // One property. Scale and translate are written together, so no frame can show
    // the content at the new scale with the old offset - the whole reason this reader
    // stopped scrolling. Coalesced to one write per frame, because a trackpad emits
    // gestures faster than the compositor draws and every extra write also re-runs
    // the frozen-pane pin below. `view` itself moves synchronously, so the factor
    // echoed to the host never lags the gesture.
    `var SEL=null,raf=0;` +
    (pinSel
      // Frozen panes used to be `position:sticky`, which needs a scrollport this
      // document no longer has. They pin instead by counter-translating: a frozen
      // track's sticky offset was, by construction, its own natural position in the
      // grid (see the frozen-pane note in sheet.rs), so the pin is one pair of numbers
      // for the whole grid - how far the content has been panned past the grid's
      // origin - rather than anything per-track. Written as custom properties on the
      // grid, so the style invalidation is scoped to it and only happens when the
      // value moves by a visible amount.
      ? `var PIN=document.querySelector(${JSON.stringify(pinSel)}),PGX=0,PGY=0,PX=-1,PY=-1;` +
        `var pin=function(){if(!PIN)return;` +
        `var x=Math.max(0,-view.tx/view.z-PGX),y=Math.max(0,-view.ty/view.z-PGY);` +
        `if(Math.abs(x-PX)<0.5&&Math.abs(y-PY)<0.5)return;` +
        `PX=x;PY=y;` +
        `PIN.style.setProperty('--fx',x+'px');PIN.style.setProperty('--fy',y+'px');};`
      : `var pin=function(){};`) +
    `var paint=function(){raf=0;` +
    `W.style.transform='translate('+view.tx+'px,'+view.ty+'px) scale('+view.z+')';` +
    // Grab is offered only where there is somewhere to pan to, as the PDF reader
    // does with the same predicate.
    `document.body.classList.toggle('pannable',V.overflows(view,G));` +
    `pin();if(SEL)SEL.moved();};` +
    `var setView=function(v){` +
    `if(v.z===view.z&&v.tx===view.tx&&v.ty===view.ty)return;` +
    `view=v;if(!raf)raf=requestAnimationFrame(paint);};` +
    // ── the operations ──
    // Nothing else in this document may move the content.
    //
    // The host's zoom, which is either a gesture it caught (Ctrl +/-/0 while the
    // frame is unfocused) or an automatic re-fit. Both zoom about the middle - the
    // engine's default anchor - and the re-fit additionally re-centres what now
    // fits, which is the whole difference between them.
    `var zoomTo=function(z,rid,gesture){var nv=V.zoomAt(view,z,null,null,G);` +
    `setView(gesture?nv:V.clampView(nv,G,true));` +
    `post({type:'zoomed',factor:view.z,requestId:rid});};` +
    // Absolute vertical position, for Home/End. `start` is the top of the content,
    // `end` its bottom; a number is a scroll offset, so it moves the content the
    // other way.
    `var goTop=function(t){setView(V.clampView({z:view.z,tx:view.tx,` +
    `ty:t==='start'?0:t==='end'?-(G.ch*view.z):-(+t||0)},G));};` +
    `var home=function(){setView(V.clampView({z:view.z,tx:0,ty:0},G,true));};` +
    // A resize re-clamps and re-centres against the viewport the view was clamped
    // against; the observer catches the content growing instead - an image that
    // decoded late, a font that swapped.
    `var remeasure=function(){measure();setView(V.clampView(view,G,true));};` +
    `window.addEventListener('resize',remeasure);` +
    `if(window.ResizeObserver)new ResizeObserver(remeasure).observe(W);` +
    // ── input ──
    // Ctrl+wheel zooms toward the cursor, a plain wheel pans, ctrl +/-/0 zoom about
    // the centre - all of it the engine's, so this reader and the PDF one cannot
    // drift on what a gesture means. Both handlers preventDefault: there is nothing
    // here for the browser to scroll, and a ctrl+wheel left alone becomes the
    // WebView's own zoom.
    `var echo=function(z){if(view.z!==z)post({type:'zoomed',factor:view.z});};` +
    `window.addEventListener('wheel',function(e){e.preventDefault();var z=view.z;` +
    `setView(V.wheel(view,{ctrl:e.ctrlKey,dx:ld(e.deltaX),dy:ld(e.deltaY)},` +
    `lx(e.clientX),ly(e.clientY),G));echo(z);` +
    `},{passive:false});` +
    // Backstop for the case where focus did end up inside the frame despite the
    // custody rules below - the host's own ctrl+/-/0 listener would never fire.
    `window.addEventListener('keydown',function(e){` +
    `if(!e.ctrlKey||e.altKey||e.metaKey)return;` +
    `var nv=V.keyZoom(view,e.key,G);if(!nv)return;var z=view.z;` +
    `setView(nv);echo(z);e.preventDefault();});` +
    // ── selection, pan, and focus custody ──
    // All three belong to one mousedown decision, so frameSelection.ts owns the
    // handlers outright and this passes it the three things it cannot reach: the
    // viewport box (in client px, the space its caret rects are in), how to pan, and
    // the body cursor.
    //
    // Focus custody rides along in its mousedown handler, and is why the engine
    // hit-tests carets by hand instead of letting the browser select: the launcher
    // pins focus to the search input (App.tsx cancels every mousedown that would
    // move it), and a subframe breaks that, because its mousedown is dispatched
    // inside *this* document where the host never sees it - after which every
    // keybind is dead.
    //
    // Pan deltas arrive in client px (raw pointer movement, and caret boxes measured
    // against the viewport) and are converted here: the content moves with the
    // cursor, so the grab point stays under it whatever the zoom.
    `SEL=${officeSelectionScript(SELECTORS[variant])}(post,` +
    `function(){return{w:G.vw*US,h:G.vh*US};},` +
    `function(dx,dy){setView(V.panBy(view,ld(dx),ld(dy),G));},` +
    `function(){return view.z;},W,` +
    `function(c){document.body.style.cursor=c;});` +
    // …and if focus lands in the frame anyway (a route preventDefault does not
    // cover, e.g. the frame being tabbed into), hand it straight back.
    `window.addEventListener('focus',function(){post({type:'refocus'});});` +
    `window.addEventListener('message',function(e){` +
    // e.origin is the literal string "null" for an opaque origin and so proves
    // nothing; identity comes from the sender being our parent plus the token.
    `if(e.source!==parent)return;` +
    `var d=e.data;if(!d||typeof d!=='object'||d.token!==T)return;` +
    `switch(d.type){` +
    // The host speaks in scroll deltas, the vocabulary its other previews scroll by;
    // a positive dy means "further down the document", so the content goes the other
    // way. `pages` is in viewport-heights, resolved here because the host does not
    // know the frame's client height.
    `case 'scrollBy':setView(V.panBy(view,-(d.dx||0),` +
    `-((d.dy||0)+(d.pages||0)*G.vh*0.9),G));break;` +
    `case 'scrollTo':goTop(d.top);break;` +
    `case 'hl':root.classList.toggle('hl-off',!d.on);break;` +
    `case 'section':home();break;` +
    `case 'zoom':zoomTo(+d.factor||1,d.requestId,d.anchor==='center');break;` +
    // Everything else is the selection engine's (selEnter / selKey / selClear).
    `default:SEL.msg(d);` +
    `}` +
    `});` +
    // ── autofit ──
    // PowerPoint's "shrink text on overflow" bakes a font scale computed against
    // *its* fonts. The preview substitutes families it does not have, so the same
    // text needs a different scale - and measurement only exists here. Every size
    // inside such a box is written as `calc(… * var(--af,1))`, so one custom
    // property re-fits the whole box.
    //
    // The inner block is what gets measured: a bottom-anchored box overflows
    // upwards, where scrollHeight cannot see it. Runs before the opening view is
    // computed, because it changes the content's height.
    (autofits
      ? `var afit=function(){` +
        `var xs=document.querySelectorAll('.pp-tb[data-af]');` +
        `for(var i=0;i<xs.length;i++){var e=xs[i],c=e.firstElementChild;` +
        `if(!c)continue;var h=e.clientHeight;if(!h)continue;` +
        `if(c.offsetHeight<=h+1)continue;` +
        // Largest scale that still fits, to within a few percent. PowerPoint's own
        // floor is a quarter of the authored size.
        `var lo=0.25,hi=1;for(var k=0;k<6;k++){var m=(lo+hi)/2;` +
        `e.style.setProperty('--af',String(m));` +
        `if(c.offsetHeight<=h+1)lo=m;else hi=m;}` +
        `e.style.setProperty('--af',String(lo));}};afit();`
      : '') +
    // ── the opening view ──
    // Measured against an untransformed wrapper, so a rect read off the document is
    // in content px directly. The best match is centred before `ready` - the host's
    // cue to reveal the buffer - so it is on screen in the first painted frame; with
    // no match, the document opens at its top, centred if it fits.
    `W.style.transform='none';measure();` +
    `var wr=W.getBoundingClientRect();` +
    (pinSel
      ? `if(PIN){var pr=PIN.getBoundingClientRect();` +
        `PGX=(pr.left-wr.left)/US;PGY=(pr.top-wr.top)/US;}`
      : '') +
    `var m=${mark};var el=m?document.getElementById(m):null;` +
    `if(el){var r=el.getBoundingClientRect();` +
    `view=V.centerOn(view.z,(r.left-wr.left+r.width/2)/US,(r.top-wr.top+r.height/2)/US,G);}` +
    `else view=V.clampView(view,G,true);` +
    `paint();` +
    `post({type:'ready'});` +
    `})();`
  );
}

/**
 * Document scaffold for a rendered office document.
 *
 * Sandbox posture, deliberately different from the extension previews and worth
 * understanding before loosening it:
 *
 *  - `sandbox="allow-scripts"` and never `allow-same-origin`. That *pair* is the
 *    sandbox escape - a same-origin scripted frame can reach the parent DOM. With
 *    scripts alone the origin stays opaque: no parent DOM, no app-origin storage,
 *    no Tauri IPC bridge. Everything the frame can do it does by postMessage,
 *    through the small protocol above.
 *  - `script-src 'nonce-…'` with 32 fresh random bytes per document, generated by
 *    the host and never derived from content. So a `<script>` that reached the
 *    markup through an escaping bug carries no nonce and never runs, and the same
 *    CSP kills `on*=` handlers and `javascript:` URLs.
 *  - `style-src 'unsafe-inline'` with no nonce, on purpose: office HTML is dense
 *    with `style=` attributes, and adding a nonce to style-src would *disable*
 *    'unsafe-inline' and strip every one of them.
 *  - `default-src 'none'`, `img-src data:` (media is inlined by the renderer),
 *    `form-action 'none'`, `base-uri 'none'` - nothing loads off the network.
 */
export function buildOfficeSrcdoc(
  html: string,
  variant: OfficeVariant,
  opts: OfficeSrcdocOpts,
): string {
  const vars = themeVarDecls(OFFICE_THEME_VARS);
  const nonce = randomToken();
  return (
    `<!DOCTYPE html><html${opts.hlOff ? ` class="hl-off"` : ''}><head>` +
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; ` +
    `style-src 'unsafe-inline'; img-src data:; script-src 'nonce-${nonce}'; ` +
    `form-action 'none'; base-uri 'none';">` +
    // Order matters only in one direction: the reset and the mark styling go
    // first so the variant scaffold (and, after it, the document's own rules)
    // override them on source order rather than needing extra specificity.
    // No scrollbar styling here, unlike the extension previews: nothing in an
    // office document scrolls (see `officeVariantCss`).
    `<style>:root{${vars}}` +
    OFFICE_BASE_CSS +
    officeVariantCss(variant, opts) +
    `</style></head><body>` +
    // Viewport probe. Every variant measures itself against it - it is both the
    // reader's viewport and the frame's client-to-layout px conversion.
    `<i id="${OFFICE_VP_ID}"></i>` +
    `<div id="${OFFICE_ZOOM_ID}">${html}</div>` +
    `<script nonce="${nonce}">${officeBootstrap(variant, opts)}</script>` +
    `</body></html>`
  );
}
