import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { listen } from "@tauri-apps/api/event";
import type { OfficeDoc, OfficeShape } from "../../types";
import { officeShape } from "../../utils";
import {
  buildOfficeSrcdoc,
  clampOfficeZoom,
  randomToken,
  OFFICE_ZOOM_MIN,
  OFFICE_ZOOM_STEP,
} from "../../srcdoc";
import { hostFocus } from "../../focus";
import { selection } from "../../selection/controller";
import { probeFrame } from "../../selection/geometry";
import OfficeFrame from "./OfficeFrame";
import type { OfficeFrameHandle } from "./OfficeFrame";
import type { FrameSelState, HostMessage } from "./protocol";
import { getOfficeDoc, officeKey, peekOfficeDoc } from "./officeCache";
import { officeScroll } from "./scrollRegistry";
import { useOfficeHtml } from "./useOfficeHtml";

/**
 * Whether this file gets the rendered-HTML office preview — i.e. whether it draws
 * its own match marks, honours Ctrl+H, and flips sections with Ctrl+←/→.
 *
 * Every extension `officeShape` names is rendered by the Rust renderer, so today
 * this is exactly "is it an office file". It stays a named function because the
 * footer hint bar and the action panel have to agree with the preview about which
 * files those chords do something for: a format added to `officeShape` ahead of its
 * renderer has to make this return false again, and there is one place to do that.
 */
export function officeRendersHtml(filename: string): boolean {
  return officeShape(filename) !== null;
}

/** A fitted slide keeps a hair of letterbox rather than touching the panel edge. */
const SLIDE_FIT = 0.98;

/**
 * Slack left when fitting a page to the frame's width, in wrap px. Nothing scrolls
 * in the frame any more, so this is no longer a scrollbar allowance - it is the
 * breathing room that keeps a fitted page's shadow off the frame's edge.
 */
const DOC_FIT_SLACK = 14;

/**
 * Zoom floor for a document whose fit factor is `fit`.
 *
 * A slide is a fixed canvas: fitting 1280px of it into a 340px side panel is a
 * factor of ~0.26, well under the shared OFFICE_ZOOM_MIN, and a floor above the
 * fitted zoom would make the default state unreachable. Half the fit leaves room
 * to zoom out a step or two from there.
 */
function zoomFloor(fit: number): number {
  return fit > 0 ? Math.min(OFFICE_ZOOM_MIN, fit * 0.5) : OFFICE_ZOOM_MIN;
}

/**
 * The authored size a document's opening zoom is fitted to, and how.
 *
 *  - `contain` (slide): a fixed canvas, fitted on both axes so the whole of it is
 *    on screen - upscaling included, since a small canvas in a big window should
 *    fill it.
 *  - `width` (doc): a page of known width and arbitrary height, fitted on the one
 *    axis that has a meaning, and capped at 1 (see computeFit).
 */
interface FitTarget {
  mode: "contain" | "width";
  w: number;
  /** 0 for `width`, which never looks at the height. */
  h: number;
}

/** A sheet answers null: it grows in both directions and opens at 100%. */
function fitTarget(variant: OfficeShape, doc: OfficeDoc | null): FitTarget | null {
  if (variant === "slide" && doc?.natural) {
    return { mode: "contain", w: doc.natural[0], h: doc.natural[1] };
  }
  if (variant === "doc" && doc?.page) return { mode: "width", w: doc.page[0], h: 0 };
  return null;
}

/** How long a re-render may take before the skeleton covers the stale document. */
const SKELETON_DELAY_MS = 140;

/** How long the zoom readout stays up after the last step. */
const ZOOM_BADGE_MS = 900;

// ── theme epoch ──────────────────────────────────────────────────────────────

// buildOfficeSrcdoc bakes the theme's custom-property *values* into the document
// (an iframe cannot inherit them), and the srcdoc is memoized - so a theme change
// is invisible until the document is rebuilt. This store bumps on the two events
// that change those values and folds into the srcdoc memo's deps, reloading both
// buffers exactly once per theme change.
let themeEpoch = 0;
const themeListeners = new Set<() => void>();
function bumpTheme(delayMs: number) {
  // Deferred, because both events are *notifications*: App.tsx's own listener is
  // what writes the new values onto the host root (and for theme-css-changed it
  // first re-fetches the injected Matugen CSS). Reading getComputedStyle in the
  // same tick would bake the outgoing palette.
  setTimeout(() => {
    themeEpoch++;
    themeListeners.forEach(l => l());
  }, delayMs);
}
void listen("appearance-changed", () => bumpTheme(0));
void listen("theme-css-changed", () => bumpTheme(150));
const subscribeTheme = (cb: () => void) => {
  themeListeners.add(cb);
  return () => void themeListeners.delete(cb);
};

/** Keys the frame's selection engine needs forwarded: it is never focused, so it
 *  cannot receive them itself (see the focus-custody note in `officeBootstrap`). */
const SEL_KEYS = new Set(["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"]);

// ── rendered-HTML preview ────────────────────────────────────────────────────

interface Props {
  path: string;
  filename: string;
  terms: string[];
  highlight: boolean;
  /** Section to open on - the one the content index says matched `terms`, or 0.
   *  `null` is "the backend's own default" (for a workbook, the first non-hidden
   *  sheet), which is what a preview opened without a search query wants. */
  initialSection?: number | null;
}

// There is no shape prop and no fallback renderer: every office format the app
// recognises is rendered by the Rust renderer, and the variant the frame is
// scaffolded for comes from the *rendered* document (`doc.shape`) rather than from
// the extension - the renderer is the authority on what it produced.
export default function OfficePreview({
  path,
  filename,
  terms,
  highlight,
  initialSection,
}: Props) {
  const epoch = useSyncExternalStore(subscribeTheme, () => themeEpoch);
  // `null` means "whichever section the backend considers the default" (for a
  // workbook, the first non-hidden sheet). The tab strip sets it to an index, and
  // `initialSection` seeds it with the section the search matched - which is why
  // the first painted frame is already the right sheet or slide rather than a flip
  // away from it.
  const [section, setSection] = useState<number | null>(initialSection ?? null);
  // Reset on a file change. PreviewPanel keys by `result.kind` and every file
  // result shares one kind, so this component instance is reused across results -
  // without this, sheet 3 of the last workbook would be requested for the next
  // file, which very likely does not have one. Assigned during render, not in an
  // effect, for the reason spelled out in useOfficeHtml: an effect commits the
  // new path paired with the old section first.
  const pathRef = useRef(path);
  const userZoomed = useRef(false);
  // Declared up here only because the path-change reset below runs during render
  // and has to be able to clear them; the fit machinery itself lives further down.
  const [fit, setFit] = useState(0);
  const fitRef = useRef(0);
  const fitToRef = useRef<FitTarget | null>(null);
  if (pathRef.current !== path) {
    pathRef.current = path;
    setSection(initialSection ?? null);
    // A new file opens at its own natural zoom - for a slide or a page, its own
    // fit, which is a property of *this* canvas / paper size and has to be
    // measured again.
    userZoomed.current = false;
    fitRef.current = 0;
    setFit(0);
  }
  const { doc, error, loading } = useOfficeHtml(path, section, terms);
  // The renderer is the authority on what it produced, so the variant comes from
  // the document rather than from the extension.
  const variant = doc?.shape ?? "sheet";

  const frameRef = useRef<OfficeFrameHandle>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  // Read inside the srcdoc memo without joining its deps: the highlight state is
  // baked in for the first paint, then driven by message (see below) so Ctrl+H
  // never reloads the document.
  const hlRef = useRef(highlight);
  hlRef.current = highlight;

  // Zoom lives in a ref for the same reason: it is baked into the next document
  // the memo builds (so a sheet switch does not snap back to 100%) but changing
  // it must never rebuild the current one - that is a message. `zoomLabel` is the
  // transient readout and is the only part React re-renders on.
  const zoomRef = useRef(1);
  const [zoomLabel, setZoomLabel] = useState<number | null>(null);
  const badgeTimer = useRef<number | undefined>(undefined);
  const showZoomBadge = useCallback((z: number) => {
    setZoomLabel(z);
    window.clearTimeout(badgeTimer.current);
    badgeTimer.current = window.setTimeout(() => setZoomLabel(null), ZOOM_BADGE_MS);
  }, []);
  useEffect(() => () => window.clearTimeout(badgeTimer.current), []);

  // ── default fit ────────────────────────────────────────────────────────────
  // Two of the three variants have an authored size, so "100%" is not a sensible
  // opening zoom for them: a 1280px slide canvas in a 340px panel would show one
  // corner of it, and an 816px page in the same panel would show the left third of
  // every line. The host measures the wrap against that size and opens at the fit
  // factor instead, loosening the frame's zoom floor to match (see zoomFloor). A
  // sheet has no authored size to fit and opens at 100%.
  //
  // Held in state *and* a ref: the state drives the re-fit effect, the ref lets the
  // srcdoc memo read the current value without joining its deps - a resize must
  // never rebuild the document, which is a frame navigation.
  fitToRef.current = fitTarget(variant, doc);
  // A zero from state means "not measured yet", not "does not fit", so it must not
  // clobber a measurement the srcdoc memo already took for this document.
  if (fit > 0) fitRef.current = fit;
  if (!fitToRef.current) fitRef.current = 0;

  // Deliberately synchronous and callable during render: the *first* document of a
  // deck has to be built with its fit already known. Waiting for the layout effect
  // means the document is baked at 100%, paints one frame of a slide four times too
  // big, and only then gets a `zoom` message - which is the flash.
  const computeFit = useCallback(() => {
    const el = wrapRef.current;
    const to = fitToRef.current;
    if (!el || !to || to.w <= 0) return 0;
    const w = el.clientWidth;
    const h = el.clientHeight;
    if (w <= 0 || h <= 0) return 0; // not laid out yet; the observer will call back
    // A page is fitted by *width* alone, and never above 1. Its height is whatever
    // the document runs to, so a contain-fit would shrink a ten-page render to a
    // sliver; and a wide QuickLook window should show the paper at 100% rather than
    // blow a 816px page up to fill it.
    if (to.mode === "width") {
      return Math.min(1, Math.round(((w - DOC_FIT_SLACK) / to.w) * 100) / 100);
    }
    // A slide is a fixed canvas: both axes, and a hair of letterbox left over.
    if (to.h <= 0) return 0;
    return Math.round(Math.min(w / to.w, h / to.h) * SLIDE_FIT * 100) / 100;
  }, []);

  const measureFit = useCallback(() => {
    const z = computeFit();
    if (z <= 0 && fitToRef.current) return; // mid-layout, not "no fit"
    setFit(prev => (Math.abs(prev - z) < 0.005 ? prev : z));
  }, [computeFit]);

  useLayoutEffect(() => {
    measureFit();
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(measureFit);
    ro.observe(el);
    return () => ro.disconnect();
  }, [measureFit, doc]);

  const built = useMemo(() => {
    if (!doc) return null;
    const token = randomToken();
    // Baked in rather than sent after `ready`: a slide flip that opened at 100% and
    // then snapped to fit would flash the canvas at four times its size, and a page
    // would flash a third of a line's width. Measured here rather than read from
    // `fit` because the state is one render behind on the first document of a file -
    // the wrap is already laid out (the skeleton was in it), so the measurement is
    // available now.
    //
    // `fitToRef` being null (a sheet) is what keeps fitRef at 0 through the render
    // above, so both branches below fall through to the carried zoom for one.
    if (fitToRef.current && fitRef.current <= 0) fitRef.current = computeFit();
    const floor = zoomFloor(fitRef.current);
    const open =
      !userZoomed.current && fitRef.current > 0
        ? fitRef.current
        : clampOfficeZoom(zoomRef.current, floor);
    zoomRef.current = open;
    return {
      token,
      srcdoc: buildOfficeSrcdoc(doc.html, doc.shape, {
        token,
        bestMarkId: doc.bestMarkId,
        hlOff: !hlRef.current,
        natural: doc.natural,
        page: doc.page,
        zoom: open,
        zoomMin: floor,
      }),
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [doc, epoch]);

  // Bumped on every `ready`, so the highlight state is re-asserted on the buffer
  // that just came up (it was built with whatever was current at build time, which
  // may already be stale by the time it finishes loading).
  const [readyTick, setReadyTick] = useState(0);
  const paintedRef = useRef(false);

  useEffect(() => {
    frameRef.current?.send({ type: "hl", on: highlight });
  }, [highlight, readyTick]);

  // Let QuickLook scroll us: an office document's scroller is in another document,
  // so there is no DOM viewport for it to find. Stable identity, because the
  // registry arbitrates side-panel vs Quicklook by which handler is on top.
  const send = useCallback((msg: HostMessage) => frameRef.current?.send(msg) ?? false, []);
  useEffect(() => officeScroll.register(send), [send]);

  // ── selection ──────────────────────────────────────────────────────────────
  // The frame runs its own copy of the selection engine (frameSelection.ts) and
  // reports the result; the host adopts it so the copy and search-selection
  // chords, the footer bar and the popover behave as they do for any other
  // preview. See `adoptExternal` in selection/controller.ts.
  const probeRef = useRef<HTMLElement>(null);
  const selRef = useRef<FrameSelState | null>(null);
  // Bumped only when the selected *text* changes, so the popover survives the
  // stream of anchor restatements a scroll produces (see ExternalPopover.key).
  const selEpoch = useRef(0);

  const onSel = useCallback((s: FrameSelState) => {
    if (selRef.current?.text !== s.text) selEpoch.current++;
    selRef.current = s;
    // The popover is host DOM, so the frame's viewport coordinates have to be
    // mapped into the wrap's. The scale is derived from the frame's painted box
    // against the viewport width it reported, which stays right however the
    // launcher's UI zoom and the reader's zoom happen to be plumbed.
    let popover = null;
    const wrap = wrapRef.current;
    const fr = frameRef.current?.rect();
    // The probe reports both where the wrap's positioning origin paints and how
    // many painted pixels one authored CSS pixel there covers - the launcher runs
    // the whole UI under a root `zoom`, so those are not the same number, and a
    // popover placed in painted pixels drifts by a percentage of its distance from
    // the origin. `k` is the other half: frame-viewport pixels to painted ones,
    // measured against the viewport width the frame reported, so it holds however
    // the frame's own zoom happens to be plumbed.
    const pf = probeFrame(probeRef.current);
    const show = s.text !== "" && !s.dragging && !s.keyboard && s.anchor;
    if (show && wrap && fr && pf && s.vw > 0 && s.vh > 0) {
      const kx = fr.width / s.vw;
      const ky = fr.height / s.vh;
      const [ax, ay, aw, ah] = s.anchor!;
      popover = {
        host: wrap,
        key: selEpoch.current,
        anchor: {
          x: (fr.left + ax * kx - pf.x) / pf.sx,
          y: (fr.top + ay * ky - pf.y) / pf.sy,
          w: (aw * kx) / pf.sx,
          h: (ah * ky) / pf.sy,
        },
        viewport: {
          top: (fr.top - pf.y) / pf.sy,
          bottom: (fr.bottom - pf.y) / pf.sy,
          left: (fr.left - pf.x) / pf.sx,
          right: (fr.right - pf.x) / pf.sx,
        },
      };
    }
    selection.adoptExternal({
      text: s.text,
      keyboard: s.keyboard,
      clear: () => void send({ type: "selClear" }),
      popover,
    });
  }, [send]);

  // A new document has nothing selected in it, and the frame reports nothing on
  // load - so the release has to happen here. Unmount included: the preview can
  // go away with a selection live in it.
  // Guarded on actually owning one, so the side panel unmounting under an open
  // Quicklook cannot drop the overlay's selection.
  const releaseSel = useCallback(() => {
    const s = selRef.current;
    selRef.current = null;
    if (s && (s.text !== "" || s.keyboard)) selection.adoptExternal(null);
  }, []);
  useEffect(() => releaseSel, [releaseSel]);

  const setZoom = useCallback((z: number) => {
    const next = clampOfficeZoom(z, zoomFloor(fitRef.current));
    // From here on the zoom is the user's: a panel resize re-fits only until they
    // take it over, or the reader would undo their zoom on every layout change.
    userZoomed.current = true;
    zoomRef.current = next;
    send({ type: "zoom", factor: next, anchor: "center" });
    showZoomBadge(next);
  }, [send, showZoomBadge]);

  // Ctrl+0. Where there is a fit, "actual size" is *it* rather than 100%: nobody
  // asks to see a sixth of a slide, or a page cropped to the panel's width. A
  // sheet has no fit and so resets to 1.
  const resetZoom = useCallback(() => {
    userZoomed.current = false;
    const z = fitRef.current > 0 ? fitRef.current : 1;
    zoomRef.current = z;
    send({ type: "zoom", factor: z, anchor: "center" });
    showZoomBadge(z);
  }, [send, showZoomBadge]);

  // Re-fit on a resize (and on a buffer that came up carrying a stale baked zoom),
  // until the user takes the zoom over. Silent: an automatic fit is not a gesture,
  // so it raises no badge.
  //
  // `zoomRef` is only advanced once the message actually went out: before the first
  // `ready` there is no frame to receive it, and recording the zoom anyway would
  // make this effect a no-op on the retry that `readyTick` triggers - which is how
  // the reader ends up stuck at the baked zoom.
  useEffect(() => {
    if (fit <= 0 || userZoomed.current) return;
    if (Math.abs(zoomRef.current - fit) < 0.005) return;
    if (!send({ type: "zoom", factor: fit })) return;
    zoomRef.current = fit;
  }, [fit, readyTick, send]);

  // ── sections ───────────────────────────────────────────────────────────────
  // The section list comes from the rendered document, so while a switch is in
  // flight it is the *previous* document's - which is the same file, so the strip
  // stays correct. The highlight follows the requested index rather than
  // `doc.section`, so a click responds on the click and not one render later.
  const sections = doc?.sections ?? [];
  const count = sections.length;
  const active = Math.min(section ?? doc?.section ?? 0, Math.max(count - 1, 0));
  const gotoSection = useCallback(
    (i: number) => setSection(Math.min(Math.max(i, 0), Math.max(count - 1, 0))),
    [count],
  );

  // Keep the selected tab reachable in a workbook with more sheets than fit.
  const activeTabRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    activeTabRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active]);

  // Ctrl +/-/0 zooms; Ctrl+arrows and Ctrl+Home/End move between sections.
  //
  // The frame runs the same zoom handler for the case where focus slipped inside
  // it, and ctrl+wheel is handled there outright (the wheel event is delivered to
  // the frame, never to the host) - both report back through `onZoomed`, which is
  // what keeps this ref in step.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey || e.metaKey || !document.hasFocus()) return;
      // Only the topmost mount acts: the side panel stays mounted underneath an
      // open Quicklook, and both install this listener.
      if (!officeScroll.isTop(send)) return;
      // Keyboard caret mode owns the movement keys outright - including Ctrl+arrows,
      // which mean word-move there, exactly as they do in the host engine, and so
      // never reach the section flip below. Escape is deliberately not forwarded:
      // App.tsx already dismisses a live selection before anything else, and for an
      // adopted one that routes back into the frame as `selClear`.
      if (selRef.current?.keyboard && SEL_KEYS.has(e.key)) {
        send({ type: "selKey", key: e.key, shift: e.shiftKey, ctrl: e.ctrlKey });
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      if (!e.ctrlKey) return;
      // In keyboard select mode Ctrl+arrows mean word-move, as they do for the PDF
      // reader's page flip.
      const navigable = count > 1 && !selection.isKeyboardMode();
      if (e.key === "=" || e.key === "+") setZoom(zoomRef.current * OFFICE_ZOOM_STEP);
      else if (e.key === "-" || e.key === "_") setZoom(zoomRef.current / OFFICE_ZOOM_STEP);
      else if (e.key === "0") resetZoom();
      else if (!navigable) return;
      else if (e.key === "ArrowLeft") gotoSection(active - 1);
      else if (e.key === "ArrowRight") gotoSection(active + 1);
      else if (e.key === "Home") gotoSection(0);
      else if (e.key === "End") gotoSection(count - 1);
      else return;
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [send, setZoom, resetZoom, gotoSection, active, count]);

  // Slides are stepped through one at a time, so the neighbours are almost always
  // what comes next; render them ahead so Ctrl+→ flips without a skeleton. Bounded
  // to ±1, cache-deduped and best-effort. Sheets are deliberately left out: a
  // workbook's sheets are picked rather than stepped, and each can be megabytes.
  const termsRef = useRef(terms);
  termsRef.current = terms;
  const termsKey = terms.join(" ");
  useEffect(() => {
    if (variant !== "slide" || count < 2) return;
    const t = setTimeout(() => {
      for (const i of [active + 1, active - 1]) {
        if (i < 0 || i >= count) continue;
        if (peekOfficeDoc(officeKey(path, i, termsRef.current))) continue;
        void getOfficeDoc(path, i, termsRef.current).catch(() => {});
      }
    }, 200);
    return () => clearTimeout(t);
    // termsKey stands in for the terms array (stable string identity).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [variant, path, active, count, termsKey]);

  // Skeleton is an *overlay* on the still-painting stale frame, never a reason to
  // blank it (`srcDoc=null` would be two extra navigations, i.e. two flashes).
  // Delayed, so a cached or fast render shows no loading state at all - except on
  // the first ever mount, which has nothing to hold and would otherwise be a void.
  const [showSkeleton, setShowSkeleton] = useState(true);
  useEffect(() => {
    // Nothing has ever painted here - even a cache hit still has to navigate a
    // frame, so hold the skeleton until `ready` rather than flash the empty wrap.
    if (!paintedRef.current) {
      setShowSkeleton(true);
      return;
    }
    // A document is already on screen. Drop the skeleton as soon as the render
    // resolves: the stale frame keeps painting until the back buffer flips in, so
    // there is nothing left to mask.
    if (!loading) {
      setShowSkeleton(false);
      return;
    }
    const t = setTimeout(() => setShowSkeleton(true), SKELETON_DELAY_MS);
    return () => clearTimeout(t);
  }, [loading]);

  const notes = doc?.notes ?? [];
  const footer =
    doc && (notes.length > 0 || doc.truncated) ? (
      <div className="office-notes">
        {doc.truncated && (
          <div className="office-note">This document is larger than the preview shows.</div>
        )}
        {notes.map((n, i) => (
          <div className="office-note" key={i}>{n}</div>
        ))}
      </div>
    ) : null;

  // Nothing rendered and nothing to hold: show the failure instead of an empty
  // frame stretched over the panel.
  if (error && !doc) {
    return (
      <div className="office-preview">
        <div className="office-msg">{error}</div>
      </div>
    );
  }

  return (
    <div className="office-preview">
      <div className="office-frame-wrap" ref={wrapRef}>
        {/* Scale probe for the popover this wrap hosts - see the mapping in onSel. */}
        <i className="sel-probe" ref={probeRef} />
        {built && (
          <OfficeFrame
            ref={frameRef}
            srcdoc={built.srcdoc}
            token={built.token}
            title={`${filename} preview`}
            onReady={() => {
              paintedRef.current = true;
              setShowSkeleton(false);
              setReadyTick(t => t + 1);
              releaseSel();
            }}
            onSel={onSel}
            onZoomed={z => {
              if (z === zoomRef.current) return;
              zoomRef.current = z;
              showZoomBadge(z);
            }}
            onRefocus={() => hostFocus.restore()}
          />
        )}
        {zoomLabel !== null && (
          <div className="office-zoom-badge">{Math.round(zoomLabel * 100)}%</div>
        )}
        {/* A deck gets a position readout rather than a tab strip: slides are
            stepped through, and thirty tabs of "Slide 17" would say nothing. The
            rendered slide's own title is the tooltip. */}
        {variant === "slide" && count > 1 && (
          <span className="office-slide-label" title={sections[active] || undefined}>
            {active + 1} / {count}
          </span>
        )}
        {showSkeleton && <div className="office-skeleton" />}
      </div>
      {variant !== "slide" && count > 1 && (
        // Below the document, where a spreadsheet's sheet tabs live. `tabIndex={-1}`
        // and no focus outline because the launcher keeps focus on the search input
        // - cancelling the mousedown does not cancel the click, so these still work.
        <div className="office-tabs" role="tablist" aria-label="Sheets">
          {sections.map((name, i) => (
            <button
              key={i}
              ref={i === active ? activeTabRef : undefined}
              className={`office-tab${i === active ? " is-active" : ""}`}
              role="tab"
              aria-selected={i === active}
              tabIndex={-1}
              title={name || `Sheet ${i + 1}`}
              onClick={() => gotoSection(i)}
            >
              {name || `Sheet ${i + 1}`}
            </button>
          ))}
        </div>
      )}
      {footer}
    </div>
  );
}
