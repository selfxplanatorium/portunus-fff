import { useState, useEffect, useLayoutEffect, useRef, useContext, useCallback, Fragment } from "react";
import type { ReactNode, MouseEvent as ReactMouseEvent, CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { SearchResult } from "../types";
import { formatBytes, formatDate, fileKind, textPreviewLang, isImagePreviewable, isSvg, isCsv, officeShape, fileCategory, folderSummary } from "../utils";
import OfficePreview from "./office/OfficePreview";
import { ColoredIconsContext } from "../coloredIcons";
import { cellMatches, tokenize, keyOf, ensureKeys, loadQueryKeys, warmHighlightKeys } from "../highlight";
import MarkdownView from "./MarkdownView";
import { selection } from "../selection/controller";
import { PdfTextLayer, OcrTextLayer } from "./TextLayer";
import { useTermHighlight } from "../hooks/useTermHighlight";
import { createViewEngine, type View, type Geom } from "../preview/viewEngine";

/** Splits text into nodes with matched words wrapped in `<mark class="preview-hl">`.
 * Sync: requires the words to have been keyed already (`ensureKeys`). */
function highlightText(text: string, qkeys: Set<string>): ReactNode {
  if (!qkeys.size) return text;
  const out: ReactNode[] = [];
  let last = 0;
  let key = 0;
  for (const t of tokenize(text)) {
    const k = keyOf(t.word);
    if (k === undefined || !qkeys.has(k)) continue;
    if (t.start > last) out.push(text.slice(last, t.start));
    out.push(<mark key={key++} className="preview-hl">{text.slice(t.start, t.end)}</mark>);
    last = t.end;
  }
  if (!out.length) return text;
  if (last < text.length) out.push(text.slice(last));
  return out;
}
import { EnterIcon, CopyIcon, FolderOpenIcon, CheckIcon, FolderFilledIcon, ChevronRightIcon, FileGlyphIcon, CategoryGlyph } from "../icons";
import hljs from "../hljs";

// ── pdf ───────────────────────────────────────────────────────────────────────

// Cache keyed by `path#page@width` so the same page rendered at different widths
// (small side preview vs high-DPI zoomed Quicklook) doesn't collide. Page count
// is cached per path (independent of which page/width is rendered).
const pdfPromiseCache = new Map<string, Promise<string>>();
const pdfUrlCache = new Map<string, string>();
const pdfPageCount = new Map<string, number>();
// Page aspect (w/h) per path, learned on first image load. Lets a revisited PDF
// size its explicit-dims host correctly on the first paint, instead of falling
// back to CSS (which can't height-contain inside the inline-block highlight host)
// or stale-aspect squish for a frame while the new page decodes.
const pdfAspect = new Map<string, number>();

function pdfKey(path: string, page: number, width: number): string {
  return `${path}#${page}@${width}`;
}

// Content-match section per (path, query), deterministic so safe to cache. A
// section is a PDF page, a worksheet or a slide. Lets a revisited file resolve its
// match section synchronously - the reader stays mounted and seeds straight to the
// (already-rendered) section instead of unmounting for the async
// content_match_section fetch and blanking a frame.
const contentMatchSectionCache = new Map<string, number>();
function contentMatchKey(path: string, query: string): string {
  return `${path}\0${query}`;
}

// Normalized [x, y, w, h] highlight boxes (0..1, top-left origin) per page, from
// the backend text layer. Keyed path#page#terms - width-independent (normalized),
// so the side preview and Quicklook share entries. Deduped via a promise cache.
type HlRect = [number, number, number, number];
const pdfRectsPromiseCache = new Map<string, Promise<HlRect[]>>();
const pdfRectsCache = new Map<string, HlRect[]>();
// Shared empty box list, so "no highlights" is a stable reference across renders.
const NO_RECTS: HlRect[] = [];
// How long a bitmap swap will wait for its highlight boxes before going up without
// them (see warmRects). Long enough for a normal text-layer extraction, short enough
// that a pathological one can't read as a stuck preview.
const RECTS_WARM_MAX_MS = 300;

function pdfRectsKey(path: string, page: number, terms: string[]): string {
  return `${path}#${page}#${terms.join(" ")}`;
}

function getPdfRects(path: string, page: number, terms: string[]): Promise<HlRect[]> {
  const key = pdfRectsKey(path, page, terms);
  if (!pdfRectsPromiseCache.has(key)) {
    pdfRectsPromiseCache.set(
      key,
      invoke<HlRect[]>("pdf_match_rects", { path, page, terms })
        .then((rects) => {
          pdfRectsCache.set(key, rects);
          return rects;
        })
        .catch((e) => {
          pdfRectsPromiseCache.delete(key);
          throw e;
        }),
    );
  }
  return pdfRectsPromiseCache.get(key)!;
}

// Currently-previewed PDF page, read by App.launch to open at the right page.
export const pdfView = { path: "", page: 0 };

// Count of mounted Quicklook PDF previews. The side-panel preview stays mounted
// under the overlay; without this both would react to Ctrl+←/→ and fight over the
// page. The side preview defers its page-nav while a Quicklook is open.
let pdfQuicklookMounted = 0;

// Fired when a Quicklook PDF unmounts so the side preview can re-sync its page to
// pdfView (which holds the page the user navigated to in Quicklook).
const PDF_SYNC_EVENT = "portunus-pdf-sync";

const PDF_QL_FIXED_ZOOM = 0.75;
// Quicklook zoom factor bounds and per-keystroke multiplier (1.0 = the 0.75-of-width
// baseline, shown as 100% in the HUD). Ctrl +/- step by ZOOM_STEP; Ctrl+wheel steps finer.
// The reader itself: bounds and per-gesture steps are policy the engine applies,
// so nothing below multiplies a factor by hand.
const pdfEngine = createViewEngine({ min: 0.4, max: 2, step: 1.25, wheelStep: 1.1 });
// Pages always rasterize at this pixel width, regardless of zoom or panel size - zoom
// is pure CSS upscaling of this one bitmap, so a page renders once and every zoom level
// (and both the side preview and Quicklook) shares the single cache entry.
const PDF_RENDER_WIDTH = 1000;
// Zoom factor at which the page width exactly fills the reader (displayW === vp.w).
// Switching pages clamps to this so the new page's full width is visible and centered.
const PDF_FIT_WIDTH_ZOOM = 1 / PDF_QL_FIXED_ZOOM;

// Cap the rendered-page cache; evict the oldest blob URLs (and revoke them) so a
// long session of zooming/paging across PDFs doesn't leak detached bitmaps.
const PDF_URL_CACHE_CAP = 64;
function storePdfUrl(key: string, url: string) {
  pdfUrlCache.set(key, url);
  while (pdfUrlCache.size > PDF_URL_CACHE_CAP) {
    const oldest = pdfUrlCache.keys().next().value as string | undefined;
    if (oldest === undefined) break;
    const u = pdfUrlCache.get(oldest);
    if (u) URL.revokeObjectURL(u);
    pdfUrlCache.delete(oldest);
    pdfPromiseCache.delete(oldest);
  }
}

function getPdfUrl(path: string, page: number, width: number): Promise<string> {
  const key = pdfKey(path, page, width);
  if (!pdfPromiseCache.has(key)) {
    pdfPromiseCache.set(
      key,
      // Raw ArrayBuffer across IPC (not a JSON number[], which ~5x's the payload and
      // dominates render time at high zoom). Layout: u32 LE page-count header + JPEG.
      invoke<ArrayBuffer>("render_pdf_page", { path, page, width })
        .then((buf) => {
          pdfPageCount.set(path, new DataView(buf).getUint32(0, true));
          // View past the 4-byte header, not buf.slice(4) - slice copies the whole
          // (multi-MB at high zoom) JPEG just to drop 4 bytes; a Uint8Array view doesn't.
          const url = URL.createObjectURL(new Blob([new Uint8Array(buf, 4)], { type: "image/jpeg" }));
          storePdfUrl(key, url);
          return url;
        })
        .catch((e) => {
          pdfPromiseCache.delete(key);
          throw e;
        }),
    );
  }
  return pdfPromiseCache.get(key)!;
}

// Overlay of normalized highlight boxes, positioned in percent so it tracks the
// rendered page <img> at any width/zoom. `pointer-events:none` keeps scroll and
// Quicklook grab-pan working through it.
function PdfHighlightLayer({ rects, style }: { rects: HlRect[]; style?: CSSProperties }) {
  if (!rects.length) return null;
  return (
    <div className="pdf-hl-layer" style={style}>
      {rects.map(([x, y, w, h], i) => (
        <div
          key={i}
          className="pdf-hl-box"
          style={{ left: `${x * 100}%`, top: `${y * 100}%`, width: `${w * 100}%`, height: `${h * 100}%` }}
        />
      ))}
    </div>
  );
}

function PdfPreview({ path, page, terms = [], highlight = true, quicklook = false }: { path: string; page: number; terms?: string[]; highlight?: boolean; quicklook?: boolean }) {
  // `cur` is the displayed page; moves with Ctrl+←/→. Only Quicklook seeds from the
  // live page the side preview last showed (via pdfView), so opening Quicklook keeps
  // the current page. The side preview itself always honors the resolved content-match
  // `page` - reading pdfView back there let a previously-shown page (pdfView is a
  // never-search-scoped global) override a new search's match page on remount.
  const startPage = () => (quicklook && pdfView.path === path ? pdfView.page : page);
  const [cur, setCur] = useState(startPage);
  // Quicklook view transform: zoom factor z (1.0 = PDF_QL_FIXED_ZOOM of reader width)
  // plus the page's top-left offset (tx, ty) within the viewport. Zoom and pan are a
  // pure CSS transform on the page - no box resize, so the bitmap never re-rasters
  // mid-zoom (the cause of the old "text jumps a few frames" glitch) and the point
  // under the cursor stays put by construction. Persists across page flips; resets on
  // remount. Side preview ignores it.
  const [view, setView] = useState({ z: 1, tx: 0, ty: 0 });
  const [grabbing, setGrabbing] = useState(false);
  const [count, setCount] = useState<number | null>(() => pdfPageCount.get(path) ?? null);
  const [aspect, setAspect] = useState(() => pdfAspect.get(path) ?? 0);
  // On switching files, adopt the new path's cached aspect if known; otherwise
  // keep the current value (a brief squish beats a 0 -> overflow flash) until load.
  useEffect(() => { const a = pdfAspect.get(path); if (a) setAspect(a); }, [path]);

  // Highlight boxes for the *displayed* page (empty when highlighting is off / no
  // terms / no text layer). Keyed on `shown` - the {path, page} the visible <img>
  // actually shows - rather than the requested `path`/`cur`, so the boxes never
  // describe a page other than the one on screen. Normalized, so width/zoom-independent.
  //
  // *Derived* from the cache, not held in state: as state they lagged the bitmap by a
  // commit, and by a whole `pdf_match_rects` round-trip on a miss - so switching files
  // swapped the page long before the boxes, leaving the old file's boxes sitting on the
  // new page until they blinked to the right ones. Derived, a cache miss draws no boxes
  // at all, and the render effect below warms the cache *before* it swaps `src`, so the
  // boxes and the bitmap they belong to land in the same paint.
  const [shown, setShown] = useState(() => ({ path, page: startPage() }));
  // Flipped in the same commit as `src` (the bitmap is decoded before that commit, so
  // it paints in that same frame) and again from the image's onLoad as a backstop for
  // any path that skipped the decode probe. No-ops when nothing changed, so the extra
  // call can't churn a render.
  const markShown = useCallback((p: string, pg: number) => {
    setShown(prev => (prev.path === p && prev.page === pg ? prev : { path: p, page: pg }));
  }, []);
  const wantRects = highlight && terms.length > 0;
  const rectsKey = pdfRectsKey(shown.path, shown.page, terms);
  const [, bumpRects] = useState(0);
  const rects = wantRects ? pdfRectsCache.get(rectsKey) ?? NO_RECTS : NO_RECTS;
  // Backstop for a page that reached the screen with cold boxes: turning highlighting
  // on mid-view, or a bitmap that came from cache before the warm below could run.
  useEffect(() => {
    if (!wantRects || pdfRectsCache.has(rectsKey)) return;
    let cancelled = false;
    getPdfRects(shown.path, shown.page, terms)
      .then(() => { if (!cancelled) bumpRects(n => n + 1); })
      .catch(e => { console.error("[pdf] pdf_match_rects failed:", e); });
    return () => { cancelled = true; };
    // rectsKey covers path+page+terms.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rectsKey, wantRects]);

  // Resolves once the boxes for `page` are cached, or null when there is nothing to
  // wait for (highlighting off, no terms, already cached). Callers await it before
  // committing a new bitmap so the page never appears without its highlights. Bounded:
  // a pathological text-layer extraction must not hold the page hostage - past the
  // deadline the bitmap goes up and the backstop effect above fills the boxes in.
  const warmRects = (p: number): Promise<unknown> | null =>
    wantRects && !pdfRectsCache.has(pdfRectsKey(path, p, terms))
      ? Promise.race([
          getPdfRects(path, p, terms).catch(() => {}),
          new Promise(res => setTimeout(res, RECTS_WARM_MAX_MS)),
        ])
      : null;

  // outerRef is the non-scrolling parent; vp is its inner box - the Quicklook viewport
  // the page is scaled/clamped against, and the fit box for the side preview.
  const outerRef = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const [vp, setVp] = useState({ w: 0, h: 0 });
  useLayoutEffect(() => {
    const outer = outerRef.current;
    if (!outer) return;
    const measure = () => {
      // Both modes measure the non-scrolling parent: Quicklook zooms via a CSS
      // transform (no scrollbar to feed back into the width), the side preview
      // contain-fits the page into the box.
      const w = outer.clientWidth;
      const h = outer.clientHeight;
      // Skip no-op updates so a stable size can't churn renders / re-measures.
      setVp(prev => (prev.w === w && prev.h === h ? prev : { w, h }));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(outer);
    return () => ro.disconnect();
  }, [quicklook]);

  // Side preview: contain-fit the page into the wrap (a definite box so the highlight
  // overlay has something to align against).
  const displayW = !quicklook && vp.w > 0 && vp.h > 0 && aspect > 0
    ? Math.floor(Math.min(vp.w, vp.h * aspect)) : 0;
  const displayH = aspect > 0 && displayW > 0 ? Math.round(displayW / aspect) : undefined;
  // Quicklook: the page's unscaled (z=1) box - 0.75 of the reader width. Zoom is the CSS
  // scale applied on top, so this stays fixed and the bitmap renders once.
  const baseW = quicklook && vp.w > 0 ? Math.round(vp.w * PDF_QL_FIXED_ZOOM) : 0;
  const baseH = aspect > 0 && baseW > 0 ? Math.round(baseW / aspect) : 0;
  // Mirror geometry + view into refs so the wheel/key/pan handlers read the latest
  // values without re-subscribing their listeners on every zoom.
  const geomRef = useRef<Geom>({ cw: baseW, ch: baseH, vw: vp.w, vh: vp.h });
  const geom: Geom = { cw: baseW, ch: baseH, vw: vp.w, vh: vp.h };
  geomRef.current = geom;
  const viewRef = useRef(view);
  viewRef.current = view;
  // Fixed render width: the bitmap is rasterized once at PDF_RENDER_WIDTH and the
  // transform scales it. Zoom never re-renders, so the cache key is zoom-free.
  const renderWidth = PDF_RENDER_WIDTH;

  // Every gesture is one `pdfEngine` call - the same code object the office reader
  // runs inside its frame, so the two readers cannot drift on what a wheel tick or
  // a Ctrl+0 means. Only the client-to-viewport conversion is this reader's own.
  // Stable identity (refs only), so listeners don't churn.
  const applyView = useCallback((f: (v: View, g: Geom) => View | null) => {
    setView((prev) => f(prev, geomRef.current) ?? prev);
  }, []);

  // Pointer coordinates relative to the reader's own box.
  const localXY = useCallback((clientX: number, clientY: number) => {
    const r = wrapRef.current?.getBoundingClientRect();
    return r ? { x: clientX - r.left, y: clientY - r.top } : { x: clientX, y: clientY };
  }, []);

  // Re-clamp the view whenever the geometry changes (viewport resize, aspect arriving
  // after load): re-centers a fitting page and keeps an overflowing one in bounds.
  useLayoutEffect(() => {
    if (!quicklook) return;
    setView((prev) => {
      const c = pdfEngine.clampView(prev, geomRef.current, true);
      return c.tx === prev.tx && c.ty === prev.ty ? prev : c;
    });
  }, [quicklook, baseW, baseH, vp.w, vp.h]);

  const key = pdfKey(path, cur, renderWidth);
  const [src, setSrc] = useState<string | null>(() => pdfUrlCache.get(key) ?? null);
  const srcRef = useRef(src);
  useEffect(() => { srcRef.current = src; }, [src]);
  const [loaded, setLoaded] = useState(() => pdfUrlCache.has(key));
  const [rendering, setRendering] = useState(false);
  // Side-preview-only: delayed skeleton + reveal animation.
  const [showSkeleton, setShowSkeleton] = useState(false);
  const [reveal, setReveal] = useState(false);
  const skeletonShownRef = useRef(false);
  const [error, setError] = useState(false);

  // A flip stays "pending" from the keypress until the new page's bitmap is actually
  // shown. Only then do we clamp zoom to fit-width and snap the view to top-centered -
  // applied in the same commit as the src swap so the old page never jumps before the
  // new one renders (the render is debounced).
  const flipPendingRef = useRef(false);
  const consumeFlip = () => {
    if (!flipPendingRef.current) return;
    flipPendingRef.current = false;
    // Clamp to fit-width and reset the pan to the page's top (h-centered). clampView
    // turns a fit-width page into tx=0, and ty=0 keeps the top in view.
    applyView((prev, g) => pdfEngine.clampView(
      { z: Math.min(prev.z, PDF_FIT_WIDTH_ZOOM), tx: 0, ty: 0 }, g, true));
  };
  // Mark the flip during render (not in an effect) so it's set before the render effect
  // runs - the cached-swap path calls consumeFlip synchronously inside that effect.
  const prevCurRef = useRef(cur);
  if (quicklook && prevCurRef.current !== cur) {
    prevCurRef.current = cur;
    flipPendingRef.current = true;
  }

  // Adopt a new file / match page during render, not in an effect. In an effect the
  // render *before* it commits with the new `path` but the old `cur`, so `key` names a
  // page of the new file that nobody asked for and the render effect below happily
  // rasterizes it - then the corrected page starts too and whichever resolves first
  // paints. That was the "wrong page flashes, then snaps to the match" glitch.
  // Adjusting here makes React re-render before committing, so the mismatched pair
  // never reaches an effect.
  const pageSrcRef = useRef(`${path}\0${page}`);
  const pageSrcKey = `${path}\0${page}`;
  if (pageSrcRef.current !== pageSrcKey) {
    pageSrcRef.current = pageSrcKey;
    setCur(startPage());
  }

  // Track the displayed page so launch can open the PDF at it.
  useEffect(() => { pdfView.path = path; pdfView.page = cur; }, [path, cur]);

  // Register this mount for page-nav arbitration (see pdfQuicklookMounted), and on
  // unmount tell the side preview to adopt the page we ended on.
  useEffect(() => {
    if (!quicklook) return;
    pdfQuicklookMounted++;
    return () => { pdfQuicklookMounted--; window.dispatchEvent(new Event(PDF_SYNC_EVENT)); };
  }, [quicklook]);

  // Side preview: when a Quicklook closes, sync to the page it left off on.
  useEffect(() => {
    if (quicklook) return;
    const onSync = () => { if (pdfView.path === path) setCur(pdfView.page); };
    window.addEventListener(PDF_SYNC_EVENT, onSync);
    return () => window.removeEventListener(PDF_SYNC_EVENT, onSync);
  }, [quicklook, path]);

  useEffect(() => {
    // Quicklook waits for its measurement so the first render targets the real width.
    if (quicklook && vp.w === 0) return;
    let cancelled = false;
    const cached = pdfUrlCache.get(key);
    if (cached) {
      const commit = () => {
        setSrc(cached); markShown(path, cur); setLoaded(true); setRendering(false);
        setShowSkeleton(false); setReveal(false); setError(false);
        setCount(pdfPageCount.get(path) ?? null);
        if (quicklook) consumeFlip();
      };
      // Bitmap is cached but the boxes may not be; wait for them rather than paint a
      // bare page and pop the highlights in afterwards.
      const warm = warmRects(cur);
      if (!warm) { commit(); return; }
      warm.then(() => { if (!cancelled) commit(); });
      return () => { cancelled = true; };
    }
    setError(false);

    if (quicklook) {
      // Zoom/page re-render: keep the current page visible (CSS-scaled) so zooming
      // feels instant and never blanks; just swap to the sharp bitmap when ready.
      setRendering(true);
      const renderTimer = setTimeout(() => {
        getPdfUrl(path, cur, renderWidth)
          .then(async (url) => {
            try { const probe = new Image(); probe.src = url; await probe.decode(); } catch { /* onLoad covers it */ }
            // Highlights ride along with the bitmap (see warmRects).
            await warmRects(cur);
            if (cancelled) return;
            setSrc(url); markShown(path, cur); setLoaded(true); setRendering(false);
            setCount(pdfPageCount.get(path) ?? null);
            consumeFlip();
          })
          .catch((e) => {
            console.error("[pdf] render_pdf_page failed:", e);
            if (cancelled) return;
            setRendering(false);
            if (!srcRef.current) setError(true); // only when there's nothing to show
          });
      }, 80);
      return () => { cancelled = true; clearTimeout(renderTimer); };
    }

    // Side preview: keep the current image up; show a delayed skeleton only if the
    // render is slow enough to need masking.
    setReveal(false);
    skeletonShownRef.current = false;
    const skeletonTimer = setTimeout(() => {
      if (cancelled) return;
      skeletonShownRef.current = true;
      setSrc(null); setLoaded(false); setShowSkeleton(true);
    }, 140);
    const renderTimer = setTimeout(() => {
      getPdfUrl(path, cur, renderWidth)
        .then(async (url) => {
          try { const probe = new Image(); probe.src = url; await probe.decode(); } catch { /* onLoad covers it */ }
          // Highlights ride along with the bitmap (see warmRects).
          await warmRects(cur);
          if (cancelled) return;
          clearTimeout(skeletonTimer);
          setReveal(skeletonShownRef.current);
          setSrc(url); markShown(path, cur); setLoaded(true); setShowSkeleton(false);
          setCount(pdfPageCount.get(path) ?? null);
        })
        .catch((e) => {
          console.error("[pdf] render_pdf_page failed:", e);
          if (!cancelled) { clearTimeout(skeletonTimer); setSrc(null); setLoaded(false); setShowSkeleton(false); setError(true); }
        });
    }, 80);
    return () => { cancelled = true; clearTimeout(skeletonTimer); clearTimeout(renderTimer); };
  }, [key, path, cur, renderWidth, quicklook, vp.w]);


  // Quicklook only: prefetch the adjacent pages so Ctrl+←/→ flips render instantly.
  // Best-effort and cache-deduped; bounded to valid pages.
  useEffect(() => {
    if (!quicklook || vp.w === 0) return;
    const t = setTimeout(() => {
      for (const p of [cur - 1, cur + 1]) {
        if (p < 0 || (count != null && p > count - 1)) continue;
        if (!pdfUrlCache.has(pdfKey(path, p, renderWidth))) {
          getPdfUrl(path, p, renderWidth).catch(() => {});
        }
      }
    }, 150);
    return () => clearTimeout(t);
  }, [quicklook, path, cur, renderWidth, count, vp.w]);

  // Ctrl+←/→ flips pages; Ctrl +/-/0 zoom (Quicklook only). Clamped to [0, count-1].
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || !document.hasFocus()) return;
      // In keyboard select mode Ctrl+←/→ means word-move, not page-flip.
      if (selection.isKeyboardMode()) return;
      if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
        // While a Quicklook PDF is open, the background side preview ignores page-nav.
        if (!quicklook && pdfQuicklookMounted > 0) return;
        setCur((c) => {
          // Until the page count is known, don't advance past the current page -
          // otherwise rapid Ctrl+→ overshoots into a non-existent page and flashes
          // "Preview unavailable" until the count arrives.
          const max = count != null ? count - 1 : c;
          return e.key === "ArrowRight" ? Math.min(c + 1, max) : Math.max(c - 1, 0);
        });
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      // Zoom keys: Quicklook only (the side preview is fit-to-panel). Anchored to
      // center; Ctrl+0 is the engine's `reset`, so it means here what it means in
      // the office reader.
      if (!quicklook) return;
      const next = pdfEngine.keyZoom(viewRef.current, e.key, geomRef.current);
      if (!next) return;
      setView(next);
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [count, quicklook]);

  // Ctrl+wheel zooms toward the cursor; plain wheel pans (there's no native scrollbar -
  // the page is positioned by transform). Native non-passive listener because React's
  // onWheel is passive and can't preventDefault (which the WebView needs to not zoom).
  useEffect(() => {
    if (!quicklook) return;
    const el = wrapRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const p = localXY(e.clientX, e.clientY);
      applyView((v, g) => pdfEngine.wheel(
        v, { ctrl: e.ctrlKey, dx: e.deltaX, dy: e.deltaY }, p.x, p.y, g));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [quicklook, applyView, localXY]);

  // The page overflows the reader (zoomed past fit) → grab-to-pan is meaningful.
  const isScrollable = quicklook && pdfEngine.overflows(view, geom);

  // Teardown for an in-progress pan drag, so an unmount mid-drag (file switch, Esc)
  // can't leak the window mousemove/mouseup listeners. Cleared when the drag ends.
  const panCleanupRef = useRef<(() => void) | null>(null);
  useEffect(() => () => panCleanupRef.current?.(), []);

  // Drag-to-pan the page. Left-drag on page whitespace (drags starting on the
  // text layer select instead), or middle-drag anywhere. Only when scrollable
  // so a plain click still falls through. Listeners live on window for the
  // drag's duration.
  const onPanStart = (e: ReactMouseEvent) => {
    if (!isScrollable) return;
    const overText = (e.target as HTMLElement).closest(".pdf-text-layer") != null;
    if (e.button === 0 ? overText : e.button !== 1) return;
    e.preventDefault();
    const startX = e.clientX, startY = e.clientY;
    const start = viewRef.current;
    setGrabbing(true);
    const onMove = (ev: MouseEvent) => {
      applyView((_, g) => pdfEngine.panBy(start, ev.clientX - startX, ev.clientY - startY, g));
    };
    const cleanup = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      panCleanupRef.current = null;
    };
    const onUp = () => { setGrabbing(false); cleanup(); };
    panCleanupRef.current = cleanup;
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  if (quicklook) {
    return (
      <div className="pdf-ql-wrap" ref={outerRef}>
        <div
          className={`pdf-ql${isScrollable ? " is-scrollable" : ""}${grabbing ? " is-grabbing" : ""}`}
          ref={wrapRef}
          onMouseDown={onPanStart}
        >
          {src && (
            <span
              className="pdf-hl-host pdf-ql-host"
              style={{
                width: baseW || undefined,
                height: baseH || undefined,
                transform: `translate(${view.tx}px, ${view.ty}px) scale(${view.z})`,
              }}
            >
              <img
                ref={imgRef}
                src={src}
                alt="PDF preview"
                className="pdf-ql-page"
                draggable={false}
                style={{ width: "100%", height: "100%" }}
                onLoad={e => {
                  setLoaded(true);
                  markShown(path, cur);
                  const t = e.currentTarget;
                  if (t.naturalWidth && t.naturalHeight) {
                    const ratio = t.naturalWidth / t.naturalHeight;
                    setAspect(ratio);
                    pdfAspect.set(path, ratio);
                  }
                }}
              />
              <PdfTextLayer path={path} page={cur} scale={view.z} />
              <PdfHighlightLayer rects={rects} />
            </span>
          )}
          {!src && !error && <div className="pdf-ql-spinner" />}
          {error && <span className="pdf-preview-msg">Preview unavailable</span>}
          {rendering && src && <div className="pdf-ql-rendering" />}
        </div>
        <div className="pdf-ql-hud">
          <span>{count != null ? `Page ${cur + 1} / ${count}` : `Page ${cur + 1}`}</span>
          <span className="pdf-ql-hud-dot" />
          <span>{Math.round(view.z * 100)}%</span>
          <span className="pdf-ql-hud-keys"><kbd>ctrl</kbd><kbd>←→</kbd> page</span>
          <span className="pdf-ql-hud-keys"><kbd>ctrl</kbd><kbd>±/0</kbd> zoom</span>
          {terms.length > 0 && (
            <span className="pdf-ql-hud-keys"><kbd>ctrl</kbd><kbd>H</kbd> highlight {highlight ? "off" : "on"}</span>
          )}
        </div>
      </div>
    );
  }

  return (
    <div ref={outerRef} className={`pdf-preview-wrap${showSkeleton && !loaded && !error ? " is-loading" : ""}`}>
      {!error && showSkeleton && (
        <div
          className="pdf-skeleton"
          style={{ opacity: loaded ? 0 : 1, animation: loaded ? "none" : undefined }}
        />
      )}
      {src && (
        <span className="pdf-hl-host pdf-side-host" style={displayW ? { width: displayW, height: displayH } : undefined}>
          <img
            src={src}
            alt="PDF preview"
            className={reveal ? "pdf-img-revealed" : undefined}
            style={displayW ? { width: displayW, height: displayH } : undefined}
            onLoad={e => {
              setLoaded(true);
              markShown(path, cur);
              const t = e.currentTarget;
              if (t.naturalWidth && t.naturalHeight) {
                const ratio = t.naturalWidth / t.naturalHeight;
                setAspect(ratio);
                pdfAspect.set(path, ratio);
              }
            }}
          />
          <PdfTextLayer path={path} page={cur} />
          <PdfHighlightLayer rects={rects} />
        </span>
      )}
      {loaded && (
        <span className="pdf-page-label">
          {count != null ? `Page ${cur + 1} / ${count}` : `Page ${cur + 1}`}
        </span>
      )}
      {error && <span className="pdf-preview-msg">Preview unavailable</span>}
    </div>
  );
}

// ── image preview ────────────────────────────────────────────────────────────

const imgPromiseCache = new Map<string, Promise<string>>();
const imgUrlCache = new Map<string, string>();

// Side preview renders at 800; Quicklook at a larger width so an enlarged image
// stays crisp instead of upscaling the 800px thumbnail.
const IMG_QL_WIDTH = 1600;

function imgKey(path: string, width: number): string { return `${path}@${width}`; }

// Normalized [x, y, w, h] highlight boxes per OCR'd image, from `image_match_rects`
// (DB cache or on-demand OCR). Keyed path#terms (width-independent, normalized), so
// side preview and Quicklook share entries. Mirrors the PDF rects cache.
const imgRectsPromiseCache = new Map<string, Promise<HlRect[]>>();
const imgRectsCache = new Map<string, HlRect[]>();

function imgRectsKey(path: string, terms: string[]): string {
  return `${path}#${terms.join(" ")}`;
}

function getImageRects(path: string, terms: string[]): Promise<HlRect[]> {
  const key = imgRectsKey(path, terms);
  if (!imgRectsPromiseCache.has(key)) {
    imgRectsPromiseCache.set(
      key,
      invoke<HlRect[]>("image_match_rects", { path, terms })
        .then((rects) => {
          imgRectsCache.set(key, rects);
          return rects;
        })
        .catch((e) => {
          imgRectsPromiseCache.delete(key);
          throw e;
        }),
    );
  }
  return imgRectsPromiseCache.get(key)!;
}

function getImgUrl(path: string, width: number): Promise<string> {
  const key = imgKey(path, width);
  if (!imgPromiseCache.has(key)) {
    imgPromiseCache.set(
      key,
      invoke<ArrayBuffer>("render_image_preview", { path, width })
        .then(buf => {
          const url = URL.createObjectURL(new Blob([buf], { type: "image/jpeg" }));
          imgUrlCache.set(key, url);
          return url;
        })
        .catch(e => {
          imgPromiseCache.delete(key);
          throw e;
        }),
    );
  }
  return imgPromiseCache.get(key)!;
}

function ImagePreview({ path, quicklook = false, terms = [], highlight = true }: { path: string; quicklook?: boolean; terms?: string[]; highlight?: boolean }) {
  // Side preview measures its container so a wide image fills the panel and stays
  // crisp on HiDPI, instead of floating at a fixed 800px. Quicklook keeps its
  // larger fixed target. outerRef is the wrap itself; a no-op guard stops a stable
  // size from churning re-renders/re-measures.
  const outerRef = useRef<HTMLDivElement>(null);
  const [measured, setMeasured] = useState(0);
  useLayoutEffect(() => {
    if (quicklook) return;
    const el = outerRef.current;
    if (!el) return;
    const measure = () => {
      const dpr = window.devicePixelRatio || 1;
      const w = Math.min(2400, Math.max(800, Math.round(el.clientWidth * dpr)));
      setMeasured(prev => (prev === w ? prev : w));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [quicklook]);

  const width = quicklook ? IMG_QL_WIDTH : measured;
  const key = imgKey(path, width);
  const [src, setSrc] = useState<string | null>(() => imgUrlCache.get(key) ?? null);
  const [loaded, setLoaded] = useState(() => imgUrlCache.has(key));
  const [error, setError] = useState(false);

  useEffect(() => {
    // Side preview waits for its measurement so the first render targets the real width.
    if (!quicklook && width === 0) return;
    const cached = imgUrlCache.get(key);
    if (cached) { setSrc(cached); setLoaded(true); setError(false); return; }
    let cancelled = false;
    setSrc(null); setLoaded(false); setError(false);
    getImgUrl(path, width)
      .then(url => { if (!cancelled) setSrc(url); })
      .catch(() => { if (!cancelled) setError(true); });
    return () => { cancelled = true; };
  }, [key, path, width, quicklook]);

  // OCR highlight boxes for the matched terms. Empty unless highlighting is on with
  // terms. Debounced ~150ms so arrow-keying through image results doesn't fire an OCR
  // per selection (the backend also coalesces). Cache hits resolve without the delay.
  const termsKey = terms.join(" ");
  const [rects, setRects] = useState<HlRect[]>(() => imgRectsCache.get(imgRectsKey(path, terms)) ?? []);
  useEffect(() => {
    if (!highlight || !terms.length) { setRects([]); return; }
    const cached = imgRectsCache.get(imgRectsKey(path, terms));
    if (cached) { setRects(cached); return; }
    // Drop stale boxes (from the previous image / query) up front so they never
    // paint on the new image during the debounce+fetch - they only appear once the
    // fresh result resolves.
    setRects([]);
    let cancelled = false;
    const timer = setTimeout(() => {
      getImageRects(path, terms)
        .then(r => { if (!cancelled) setRects(r); })
        .catch(e => { console.error("[img] image_match_rects failed:", e); if (!cancelled) setRects([]); });
    }, 150);
    return () => { cancelled = true; clearTimeout(timer); };
    // termsKey stands in for the terms array (stable string identity).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, termsKey, highlight]);

  // The highlight overlay is positioned to the <img>'s rendered box within the wrap
  // (object-fit:contain letterboxes it), so percent boxes land exactly on the image
  // at any panel size. Measured on load and on resize.
  const imgRef = useRef<HTMLImageElement>(null);
  // The box carries the `src` it was measured against, so the overlay renders only
  // when it matches the displayed image. A swap reuses the same <img>, so this avoids
  // both painting boxes over the old geometry and the clear-vs-onLoad race that left
  // boxes missing until a revisit.
  const [imgBox, setImgBox] = useState<{ src: string; l: number; t: number; w: number; h: number } | null>(null);
  const measureImg = useCallback(() => {
    const img = imgRef.current, wrap = outerRef.current;
    if (!img || !wrap || !src) return;
    // offset* (layout box relative to the positioned wrap), NOT getBoundingClientRect:
    // the image runs a brief scale-in reveal animation, and a transformed rect measured
    // mid-animation would leave the overlay permanently offset. offset* ignore transforms.
    const box = { src, l: img.offsetLeft, t: img.offsetTop, w: img.offsetWidth, h: img.offsetHeight };
    setImgBox(prev => (prev && prev.src === box.src && prev.l === box.l && prev.t === box.t && prev.w === box.w && prev.h === box.h ? prev : box));
  }, [src]);
  // Measure once the current src is laid out (covers cached images whose onLoad may
  // fire before the ref settles), and again on any resize.
  useLayoutEffect(() => { if (loaded) measureImg(); }, [src, loaded, measureImg]);
  useEffect(() => {
    const wrap = outerRef.current;
    if (!wrap) return;
    const ro = new ResizeObserver(() => measureImg());
    ro.observe(wrap);
    return () => ro.disconnect();
  }, [measureImg]);

  return (
    <div ref={outerRef} className={`pdf-preview-wrap${!loaded && !error ? " is-loading" : ""}`}>
      {!error && (
        <div
          className="pdf-skeleton"
          style={{ opacity: loaded ? 0 : 1, animation: loaded ? "none" : undefined }}
        />
      )}
      {src && (
        <img
          ref={imgRef}
          src={src}
          alt="Image preview"
          className={loaded ? "img-preview-in" : undefined}
          style={{ opacity: loaded ? undefined : 0 }}
          onLoad={() => { setLoaded(true); measureImg(); }}
        />
      )}
      {src && imgBox && imgBox.src === src && (
        <div
          className="img-text-layer-host"
          style={{ position: "absolute", left: imgBox.l, top: imgBox.t, width: imgBox.w, height: imgBox.h, pointerEvents: "none" }}
        >
          <OcrTextLayer path={path} />
        </div>
      )}
      {src && imgBox && imgBox.src === src && (
        <PdfHighlightLayer
          rects={rects}
          style={{ left: imgBox.l, top: imgBox.t, width: imgBox.w, height: imgBox.h, right: "auto", bottom: "auto" }}
        />
      )}
      {error && <span className="pdf-preview-msg">Preview unavailable</span>}
    </div>
  );
}

// ── svg preview ───────────────────────────────────────────────────────────────

// The asset protocol scope is restricted to icon dirs, so we can't use
// convertFileSrc for arbitrary paths. Instead, read the SVG markup via the
// existing read_text_preview command and create a blob URL from it.
const svgBlobCache = new Map<string, string>();

function SvgPreview({ path }: { path: string }) {
  const [src, setSrc] = useState<string | null>(() => svgBlobCache.get(path) ?? null);
  const [loaded, setLoaded] = useState(() => svgBlobCache.has(path));
  const [error, setError] = useState(false);

  useEffect(() => {
    const cached = svgBlobCache.get(path);
    if (cached) { setSrc(cached); setLoaded(true); setError(false); return; }
    let cancelled = false;
    setSrc(null); setLoaded(false); setError(false);
    invoke<string>("read_text_preview", { path })
      .then(markup => {
        if (cancelled) return;
        const blob = new Blob([markup], { type: "image/svg+xml" });
        const url = URL.createObjectURL(blob);
        svgBlobCache.set(path, url);
        setSrc(url);
      })
      .catch(() => { if (!cancelled) setError(true); });
    return () => { cancelled = true; };
  }, [path]);

  return (
    <div className={`pdf-preview-wrap${!loaded && !error ? " is-loading" : ""}`}>
      {!error && (
        <div
          className="pdf-skeleton"
          style={{ opacity: loaded ? 0 : 1, animation: loaded ? "none" : undefined }}
        />
      )}
      {src && (
        <img
          src={src}
          alt="SVG preview"
          className={loaded ? "pdf-img-revealed" : undefined}
          style={{ opacity: loaded ? undefined : 0 }}
          onLoad={() => setLoaded(true)}
          onError={() => setError(true)}
        />
      )}
      {error && <span className="pdf-preview-msg">Preview unavailable</span>}
    </div>
  );
}

// ── csv / tsv preview ─────────────────────────────────────────────────────────

// Minimal RFC-4180-ish parser: handles quoted fields with embedded delimiters
// and doubled "" escapes. Good enough for a preview over the (possibly line-
// truncated) text returned by read_text_preview.
function parseDelimited(text: string, delim: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let inQuotes = false;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (inQuotes) {
      if (ch === '"') {
        if (text[i + 1] === '"') { field += '"'; i++; }
        else inQuotes = false;
      } else field += ch;
    } else if (ch === '"') {
      inQuotes = true;
    } else if (ch === delim) {
      row.push(field); field = "";
    } else if (ch === "\n") {
      row.push(field); field = "";
      rows.push(row); row = [];
    } else if (ch !== "\r") {
      field += ch;
    }
  }
  if (field.length > 0 || row.length > 0) { row.push(field); rows.push(row); }
  return rows;
}

// Shared table renderer for CSV/TSV and spreadsheet previews. Highlights matched
// terms per cell and scrolls the first matching cell into view.
function DataTable({ rows, terms }: { rows: string[][]; terms: string[] }) {
  const firstMatch = useRef<HTMLTableCellElement | null>(null);
  // Query-key set for sync matching; populated once the cell + query words are keyed.
  const [qkeys, setQkeys] = useState<Set<string>>(() => new Set());

  useEffect(() => {
    if (!terms.length) {
      setQkeys(new Set());
      return;
    }
    let cancelled = false;
    // Key the query plus every cell word in one batch, then expose the query-key set
    // so the sync render (cellMatches / highlightText) can match via the cache.
    const words: string[] = [];
    for (const r of rows) for (const c of r) for (const t of tokenize(c)) words.push(t.word);
    (async () => {
      await ensureKeys([...terms, ...words]);
      if (!cancelled) setQkeys(await loadQueryKeys(terms));
    })();
    return () => {
      cancelled = true;
    };
  }, [rows, terms]);

  useEffect(() => {
    firstMatch.current?.scrollIntoView({ block: "center" });
  }, [rows, qkeys]);

  let found = false;
  // Returns a ref callback for the first term-matching cell, undefined otherwise.
  const refForCell = (text: string) => {
    if (found || !cellMatches(text, qkeys)) return undefined;
    found = true;
    return (el: HTMLTableCellElement | null) => { firstMatch.current = el; };
  };

  const cellClass = (text: string) =>
    cellMatches(text, qkeys) ? "preview-hl-cell" : undefined;

  const [header, ...body] = rows;
  return (
    <div className="text-preview-wrap" data-selectable>
      <table className="csv-preview">
        <thead>
          <tr>
            {header.map((cell, i) => (
              <th key={i} ref={refForCell(cell)} className={cellClass(cell)}>{highlightText(cell, qkeys)}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {body.map((r, ri) => (
            <tr key={ri}>
              {r.map((cell, ci) => (
                <td key={ci} ref={refForCell(cell)} className={cellClass(cell)}>{highlightText(cell, qkeys)}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function CsvPreview({ path, delim, terms }: { path: string; delim: string; terms: string[] }) {
  const [rows, setRows] = useState<string[][] | null>(null);

  useEffect(() => {
    let cancelled = false;
    setRows(null);
    invoke<string>("read_text_preview", { path })
      .then(text => { if (!cancelled) setRows(parseDelimited(text, delim).slice(0, 100)); })
      .catch(() => { if (!cancelled) setRows([]); });
    return () => { cancelled = true; };
  }, [path, delim]);

  if (rows === null || rows.length === 0) return <div className="text-preview-wrap" />;
  return <DataTable rows={rows} terms={terms} />;
}

// ── folder preview ──────────────────────────────────────────────────────────

interface FolderEntry { name: string; is_dir: boolean; size?: number; }

// Backend `list_folder` caps the listing at this many entries (preview.rs).
const FOLDER_LIST_CAP = 200;

function FolderRow({ e }: { e: FolderEntry }) {
  const colored = useContext(ColoredIconsContext);
  const isDot = e.name.startsWith(".");
  const cat = fileCategory(e.name);
  return (
    <div className={`folder-entry${isDot ? " dotfile" : ""}${e.is_dir ? " is-dir" : ""}`}>
      <span className="folder-entry-lead">
        {e.is_dir ? (
          <span className="folder-entry-glyph" data-cat={colored ? "folder" : undefined}><FolderFilledIcon size={13} /></span>
        ) : colored ? (
          <span className="folder-entry-glyph" data-cat={cat}><CategoryGlyph cat={cat} size={13} /></span>
        ) : (
          <span className="folder-entry-glyph file"><FileGlyphIcon size={13} /></span>
        )}
      </span>
      <span className="folder-entry-name">{e.name}</span>
      {e.is_dir ? (
        <span className="folder-entry-chevron"><ChevronRightIcon size={12} /></span>
      ) : e.size != null ? (
        <span className="folder-entry-size">{formatBytes(e.size)}</span>
      ) : (
        <span className="folder-entry-size" />
      )}
    </div>
  );
}

function FolderContents({ entries }: { entries: FolderEntry[] }) {
  // Backend sorts folders-first; drop a single divider at the folder→file boundary.
  const firstFile = entries.findIndex(e => !e.is_dir);
  const dividerAt = firstFile > 0 ? firstFile : -1; // only when both groups exist
  const capped = entries.length >= FOLDER_LIST_CAP;
  return (
    <div className="folder-contents">
      {entries.map((e, i) => (
        <Fragment key={`${e.name}-${i}`}>
          {i === dividerAt && <div className="folder-group-divider" />}
          <FolderRow e={e} />
        </Fragment>
      ))}
      {capped && <div className="folder-cap-note">Showing first {FOLDER_LIST_CAP} entries</div>}
    </div>
  );
}

function FolderPreview({ result, onLaunch, quicklook }: { result: SearchResult; onLaunch: () => void; quicklook: boolean }) {
  const filePath = result.subtitle ? `${result.subtitle}/${result.title}` : result.title;
  const [entries, setEntries] = useState<FolderEntry[] | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setEntries(null);
    invoke<FolderEntry[]>("list_folder", { path: filePath })
      .then(e => { if (!cancelled) setEntries(e); })
      .catch(() => { if (!cancelled) setEntries([]); });
    return () => { cancelled = true; };
  }, [filePath]);

  const handleCopy = () => {
    navigator.clipboard.writeText(filePath);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  const capped = (entries?.length ?? 0) >= FOLDER_LIST_CAP;
  const summary = entries === null ? "" : folderSummary(entries, capped);

  return (
    <div className={`file-preview folder-preview${quicklook ? " file-preview-ql" : ""}`}>
      {!quicklook && (
        <div className="file-preview-head">
          <div className="file-preview-icon-wrap"><FolderFilledIcon size={22} /></div>
          <div className="file-preview-head-text">
            <div className="file-preview-title">{result.title}</div>
            <div className="file-preview-tag">{summary ? `Folder · ${summary}` : "Folder"}</div>
          </div>
          <div className="file-preview-actions">
            <button className={`file-btn-icon${copied ? " copied" : ""}`} onClick={handleCopy} title="Copy path" tabIndex={-1}>
              {copied ? <CheckIcon /> : <CopyIcon />}
            </button>
            <button className="btn-primary" onClick={onLaunch} tabIndex={-1}>
              Open <span className="btn-kbd"><EnterIcon /></span>
            </button>
          </div>
        </div>
      )}

      {entries === null ? (
        <div className="folder-contents folder-contents-loading">
          {Array.from({ length: 7 }).map((_, i) => (
            <div className="folder-skeleton-row" key={i}>
              <span className="folder-skeleton-lead" />
              <span className="folder-skeleton-name" style={{ width: `${40 + ((i * 17) % 45)}%` }} />
            </div>
          ))}
        </div>
      ) : entries.length === 0 ? (
        <div className="folder-empty">
          <span className="folder-empty-glyph"><FolderOpenIcon /></span>
          <span className="folder-empty-label">Empty folder</span>
        </div>
      ) : (
        <FolderContents entries={entries} />
      )}
    </div>
  );
}

// ── markdown preview ──────────────────────────────────────────────────────────

function MarkdownPreview({ path, terms }: { path: string; terms: string[] }) {
  const [source, setSource] = useState<string | null>(null);
  const baseDir = path.slice(0, path.lastIndexOf("/")) || "/";
  const termsKey = terms.join(" ");

  useEffect(() => {
    let cancelled = false;
    setSource(null);
    // Debounce the fetch + (costly) ReactMarkdown render so arrowing quickly
    // through .md files doesn't render every one passed over - same 80ms guard
    // the PDF preview uses.
    const t = setTimeout(() => {
      invoke<string>("read_text_preview", { path, terms })
        // Key the document's words before committing it, so MarkdownView's mark
        // pass runs synchronously and the marks paint with the text (see
        // useTermHighlight). No-op when there are no terms.
        .then(async text => {
          await warmHighlightKeys(text, terms);
          if (!cancelled) setSource(text);
        })
        .catch(() => { if (!cancelled) setSource(""); });
    }, 40);
    return () => { cancelled = true; clearTimeout(t); };
    // termsKey stands in for the terms array (stable string identity) - a fresh
    // [] each parent render must not re-fire this fetch and churn the preview.
  }, [path, termsKey]);

  if (source === null) return <div className="text-preview-wrap" />;

  return (
    <div className="text-preview-wrap" data-selectable>
      <MarkdownView source={source} baseDir={baseDir} terms={terms} />
    </div>
  );
}

// ── text preview ─────────────────────────────────────────────────────────────

// Highlighted-HTML cache keyed path\0lang\0terms, so a revisited file paints
// synchronously instead of blanking to an empty dark wrap while it re-reads +
// re-highlights (the "code preview flash"). Capped like the pdf/img caches.
const textHtmlCache = new Map<string, string>();
const TEXT_HTML_CACHE_CAP = 64;
function textHtmlKey(path: string, lang: string, terms: string[]): string {
  return `${path}\0${lang}\0${terms.join(" ")}`;
}
function storeTextHtml(key: string, html: string) {
  textHtmlCache.set(key, html);
  while (textHtmlCache.size > TEXT_HTML_CACHE_CAP) {
    const oldest = textHtmlCache.keys().next().value as string | undefined;
    if (oldest === undefined) break;
    textHtmlCache.delete(oldest);
  }
}

function TextPreview({ path, lang, terms }: { path: string; lang: string; terms: string[] }) {
  const termsKey = terms.join(" ");
  const key = textHtmlKey(path, lang, terms);
  const [html, setHtml] = useState<string | null>(() => textHtmlCache.get(key) ?? null);
  const ref = useTermHighlight<HTMLPreElement>(terms, html);
  useEffect(() => {
    const cached = textHtmlCache.get(key);
    // Cache hit: swap synchronously, no fetch, no flash.
    if (cached !== undefined) { setHtml(cached); return; }
    let cancelled = false;
    // Keep the previous file's code up until the new one resolves (matches the
    // PDF preview's "hold the old page" pattern) rather than blanking to the
    // empty wrap — the stale frame is imperceptible for local reads and avoids
    // the dark-box flash. First-ever mount has nothing to show, so html stays null.
    // Pass terms so the backend returns a window centered on the first match.
    //
    // Debounced like the md/pdf previews: the highlight below runs synchronously
    // on the main thread and commits multi-KB of innerHTML, so a file merely
    // passed over must not pay for it.
    const t = setTimeout(() => {
      invoke<string>("read_text_preview", { path, terms })
        .then(async text => {
          if (cancelled) return;
          // Key the file's words before committing the html, so the mark pass in
          // useTermHighlight runs synchronously and the marks land in the same
          // paint as the code. No-op when there are no terms.
          await warmHighlightKeys(text, terms);
          if (cancelled) return;
          setTimeout(() => {
            if (cancelled) return;
            const out = hljs.highlight(text, { language: lang, ignoreIllegals: true }).value;
            storeTextHtml(key, out);
            setHtml(out);
          }, 0);
        })
        .catch(() => { if (!cancelled) setHtml(""); });
    }, 40);
    return () => { cancelled = true; clearTimeout(t); };
    // termsKey stands in for the terms array (stable string identity).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, path, lang, termsKey]);

  if (html === null) return <div className="text-preview-wrap" />;

  return (
    <div className="text-preview-wrap" data-selectable>
      <pre
        ref={ref}
        className="text-preview-code hljs"
        dangerouslySetInnerHTML={{ __html: html }}
        key={`${html.length}${terms.join("")}`}
      />
    </div>
  );
}

// ── file / folder ─────────────────────────────────────────────────────────────

interface Props {
  result: SearchResult;
  onLaunch: () => void;
  onReveal?: () => void;
  /** Matched content-search terms to highlight (empty for non-content searches). */
  terms?: string[];
  /** Whether matched-term highlighting is enabled (Ctrl+H toggle; PDF overlay). */
  highlight?: boolean;
  /** Rendered in the full-card Quicklook overlay - enables the large scrollable PDF reader. */
  quicklook?: boolean;
}

export default function FilePreview({ result, onLaunch, onReveal, terms = [], highlight = true, quicklook = false }: Props) {
  const isFolder = result.kind === "folder";
  if (isFolder) return <FolderPreview result={result} onLaunch={onLaunch} quicklook={quicklook} />;
  const kind = fileKind(result.title, false);
  const tag = [kind, result.file_size != null ? formatBytes(result.file_size) : null]
    .filter(Boolean)
    .join(" · ");
  const filePath = result.subtitle ? `${result.subtitle}/${result.title}` : result.title;
  const isPdf = kind === "PDF Document";
  const isImage = isImagePreviewable(result.title);
  const isSvgFile = isSvg(result.title);
  const isCsvFile = isCsv(result.title);
  const office = officeShape(result.title);
  const textLang = !isImage && !isSvgFile && !isCsvFile && !office
    ? textPreviewLang(result.title)
    : null;
  // No renderer matched (archive, video, audio, unknown binary). Show an explicit
  // placeholder instead of a blank body. Quicklook is gated off these upstream, so
  // this only ever appears in the side panel.
  const hasPreview = isPdf || isImage || isSvgFile || isCsvFile || !!office || !!textLang;

  const [copied, setCopied] = useState(false);

  // Content-match section for a paged preview - a PDF page, or an office sheet /
  // slide - fetched lazily: the backend no longer computes it for every search
  // result (that ran a per-file rescan on each keystroke), so we resolve it here
  // only for the file actually being previewed. `null` while pending - we hold the
  // reader's mount until it resolves so it seeds straight to the match section (no
  // section-0 flash then jump). Empty `terms` (non-content file search) skips the
  // fetch and opens at section 0.
  const termsKey = terms.join(" ");
  // A doc-shape office file is one section, so its match section is always 0 and
  // asking for it would only delay the mount by an IPC round trip. Its in-document
  // jump is `OfficeDoc.bestMarkId`, which rides along with the render.
  const paged = isPdf || office === "sheet" || office === "slide";
  const [matchSection, setMatchSection] = useState<number | null>(() =>
    paged && terms.length ? contentMatchSectionCache.get(contentMatchKey(filePath, termsKey)) ?? null : 0,
  );
  // Tracks the file the current matchSection belongs to. Reconciled during render, not
  // in the effect below: an effect commits one render with the new `filePath` still
  // paired with the *previous* file's matchSection, and the reader then renders that
  // section of the new file before the corrected one arrives (the wrong-section flash).
  // Blanking only on a file change lets a same-file term refetch (i.e. typing) keep the
  // section mounted instead of unmounting/remounting the reader between keystrokes.
  const matchSectionPathRef = useRef<string | null>(filePath);
  if (matchSectionPathRef.current !== filePath) {
    matchSectionPathRef.current = filePath;
    // Cache hit (revisited file): keep the reader mounted on the right section.
    // Otherwise `null` holds the mount until the fetch below resolves.
    setMatchSection(
      paged && terms.length ? contentMatchSectionCache.get(contentMatchKey(filePath, termsKey)) ?? null : 0,
    );
  }
  useEffect(() => {
    if (!paged || !terms.length) { setMatchSection(0); return; }
    // Cache hit (revisited file, or a term change on the same file): no fetch.
    const cached = contentMatchSectionCache.get(contentMatchKey(filePath, termsKey));
    if (cached != null) { setMatchSection(cached); return; }
    let cancelled = false;
    invoke<number | null>("content_match_section", { path: filePath, query: termsKey })
      // Same section number -> return prev so the reader's props are referentially
      // unchanged and it doesn't re-render.
      .then(p => {
        contentMatchSectionCache.set(contentMatchKey(filePath, termsKey), p ?? 0);
        if (!cancelled) setMatchSection(prev => (prev === (p ?? 0) ? prev : (p ?? 0)));
      })
      .catch(e => { console.error("[preview] content_match_section failed:", e); if (!cancelled) setMatchSection(0); });
    return () => { cancelled = true; };
    // termsKey stands in for the terms array (stable string identity).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filePath, paged, termsKey]);

  const handleCopy = () => {
    navigator.clipboard.writeText(filePath);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  const handleReveal = () => {
    invoke('reveal_file', { path: filePath }).catch(err => console.error("[preview] reveal_file failed:", err));
    onReveal?.();
  };

  // Header glyph mirrors the result-list icon (ResultIcon): category glyph +
  // tint when colored icons are on, plain document glyph otherwise. "other"
  // carries no data-cat so the wrap keeps its default accent.
  const colored = useContext(ColoredIconsContext);
  const headCat = colored ? fileCategory(result.title) : "other";
  const icon = colored
    ? <CategoryGlyph cat={headCat} size={22} />
    : <FileGlyphIcon size={22} />;

  return (
    <div className={`file-preview${quicklook ? " file-preview-ql" : ""}`}>
      {!quicklook && (
      <div className="file-preview-head">
        <div className="file-preview-icon-wrap" data-cat={headCat === "other" ? undefined : headCat}>{icon}</div>
        <div className="file-preview-head-text">
          <div className="file-preview-title">{result.title}</div>
          <div className="file-preview-tag">{tag}</div>
          {result.snippet && <div className="file-preview-path">{filePath}</div>}
        </div>
        <div className="file-preview-actions">
          <button className={`file-btn-icon${copied ? ' copied' : ''}`} onClick={handleCopy} title="Copy path" tabIndex={-1}>
            {copied ? <CheckIcon /> : <CopyIcon />}
          </button>
          <button className="file-btn-icon" onClick={handleReveal} title="Reveal in folder" tabIndex={-1}>
            <FolderOpenIcon />
          </button>
          <button className="btn-primary" onClick={onLaunch} tabIndex={-1}>
            Open <span className="btn-kbd"><EnterIcon /></span>
          </button>
        </div>
      </div>
      )}

      {isPdf && matchSection != null && <PdfPreview path={filePath} page={matchSection} terms={terms} highlight={highlight} quicklook={quicklook} />}
      {isImage && <ImagePreview path={filePath} terms={terms} highlight={highlight} quicklook={quicklook} />}
      {isSvgFile && <SvgPreview path={filePath} />}
      {isCsvFile && <CsvPreview path={filePath} delim={result.title.toLowerCase().endsWith(".tsv") ? "\t" : ","} terms={terms} />}
      {office && matchSection != null && (
        <OfficePreview
          path={filePath}
          filename={result.title}
          terms={terms}
          highlight={highlight}
          // A section the search matched, or 0 when there is nothing to match. A
          // sheet or slide document opens there; a doc-shape one has a single
          // section and ignores it (the in-document jump is `bestMarkId`).
          initialSection={matchSection}
        />
      )}
      {textLang === "markdown" && <MarkdownPreview path={filePath} terms={terms} />}
      {textLang && textLang !== "markdown" && <TextPreview path={filePath} lang={textLang} terms={terms} />}
      {!hasPreview && (
        <div className="file-preview-none">
          <span className="file-preview-none-glyph"><FileGlyphIcon size={28} /></span>
          <span className="file-preview-none-label">No preview</span>
          <span className="file-preview-none-kind">{kind}</span>
        </div>
      )}

      {!quicklook && (result.modified || result.created) && (
      <div className="file-preview-meta">
        {result.modified && (
          <div className="file-preview-meta-cell">
            <span className="file-preview-meta-key">Modified</span>
            <span className="file-preview-meta-val">{formatDate(result.modified)}</span>
          </div>
        )}
        {result.created && (
          <div className="file-preview-meta-cell">
            <span className="file-preview-meta-key">Created</span>
            <span className="file-preview-meta-val">{formatDate(result.created)}</span>
          </div>
        )}
      </div>
      )}
    </div>
  );
}
