// The pan/zoom view shared by the two document readers.
//
// A reader positions its content with one CSS transform - `translate(tx,ty)
// scale(z)` - and never with a scroll. That is not a style preference: a scroll
// offset and a style change are committed by different parts of WebKit (scrolling
// is owned by the scrolling thread), so a zoom that moves both can be presented
// half-applied, showing the new scale against the old offset for a frame. One
// property cannot be half-applied. This is the canonical statement of that rule;
// the places that live with its consequences point back here.
//
// The engine owns the *policy* as well as the arithmetic - what a wheel tick
// means, what Ctrl+0 means, where the zoom stops - because the two readers had
// already drifted apart on the second of those while sharing the first. A reader
// keeps only what is genuinely its own: how to turn a pointer event into viewport
// coordinates, and what its content is.
//
// Everything lives inside `createViewEngine` on purpose. The office reader runs
// inside a sandboxed iframe that can import nothing, so it gets this code by
// `String(createViewEngine)` (see `officeBootstrap`): the factory is stringified
// into the srcdoc and called there. A reference out of the factory - a module
// constant, another import - would survive as a *minified* identifier that does
// not exist inside the frame, so the closure has to be complete.

/** Content offset and scale. `tx`/`ty` are the content's top-left in the viewport. */
export interface View {
  z: number;
  tx: number;
  ty: number;
}

/**
 * What the view is clamped against: the content at z = 1, and the box it sits in.
 */
export interface Geom {
  cw: number;
  ch: number;
  vw: number;
  vh: number;
}

/** A wheel tick, in viewport px, with the deltas already in the view's own space. */
export interface WheelInput {
  ctrl: boolean;
  dx: number;
  dy: number;
}

/** Per-reader zoom policy. A slide fitted into a 340px panel needs a floor far
 *  below a sheet's, so `min` is per *document* for the office reader. */
export interface ViewBounds {
  min: number;
  max: number;
  /** Multiplier per keystroke. */
  step: number;
  /** Multiplier per wheel tick; finer than `step` where a reader wants it. */
  wheelStep: number;
  /** Quantum for the factor, as a reciprocal (100 = 2dp). Omitted, none. */
  snap?: number;
}

export interface ViewEngine {
  /**
   * Hold the content inside its viewport: each offset stays within its valid
   * range, which collapses to a single value once an axis fits. With
   * `recenterFit` a fitting axis is centred instead - used for defaults, resizes
   * and page flips. Interactive zoom and pan leave it off, so the anchored point
   * is honored even when the content fits (otherwise a fitting page would just
   * centre-zoom and ignore the cursor).
   */
  clampView(v: View, g: Geom, recenterFit?: boolean): View;
  /**
   * Zoom toward a fixed point by holding the content coordinate under it
   * invariant: the translate moves with the scale so `(anchor - translate) /
   * scale` does not change. A null anchor means the viewport's centre.
   */
  zoomAt(v: View, nz: number, ax: number | null, ay: number | null, g: Geom): View;
  /** Move the content by a pixel delta - wheel scroll, drag - and clamp. */
  panBy(v: View, dx: number, dy: number, g: Geom): View;
  /** A wheel tick: ctrl zooms toward the pointer, anything else pans. */
  wheel(v: View, e: WheelInput, ax: number, ay: number, g: Geom): View;
  /** Ctrl +/-/0. Null for a key this does not own, so the caller falls through. */
  keyZoom(v: View, key: string, g: Geom): View | null;
  /** Actual size, at the top, centred if it fits. What Ctrl+0 means. */
  reset(g: Geom): View;
  /** The view that puts a content point in the middle of the viewport. */
  centerOn(z: number, cx: number, cy: number, g: Geom): View;
  /** Does the content overflow its viewport - i.e. is there anywhere to pan to? */
  overflows(v: View, g: Geom): boolean;
}

export function createViewEngine(b: ViewBounds): ViewEngine {
  var bound = function (z: number): number {
    var n = Math.min(b.max, Math.max(b.min, z));
    return b.snap ? Math.round(n * b.snap) / b.snap : n;
  };
  var clampView = function (v: View, g: Geom, recenterFit?: boolean): View {
    var axis = function (val: number, vpLen: number, scaled: number): number {
      if (recenterFit && scaled <= vpLen) return (vpLen - scaled) / 2;
      var lo = Math.min(0, vpLen - scaled);
      var hi = Math.max(0, vpLen - scaled);
      return Math.min(hi, Math.max(lo, val));
    };
    return {
      z: v.z,
      tx: axis(v.tx, g.vw, g.cw * v.z),
      ty: axis(v.ty, g.vh, g.ch * v.z),
    };
  };
  var zoomAt = function (v: View, nz: number, ax: number | null, ay: number | null, g: Geom): View {
    var z = bound(nz);
    if (z === v.z) return v;
    var x = ax == null ? g.vw / 2 : ax;
    var y = ay == null ? g.vh / 2 : ay;
    // Content coordinate under the anchor, held fixed across the change of scale.
    var hx = (x - v.tx) / v.z;
    var hy = (y - v.ty) / v.z;
    return clampView({ z: z, tx: x - hx * z, ty: y - hy * z }, g);
  };
  var panBy = function (v: View, dx: number, dy: number, g: Geom): View {
    return clampView({ z: v.z, tx: v.tx + dx, ty: v.ty + dy }, g);
  };
  var reset = function (g: Geom): View {
    return clampView({ z: bound(1), tx: 0, ty: 0 }, g, true);
  };
  return {
    clampView: clampView,
    zoomAt: zoomAt,
    panBy: panBy,
    reset: reset,
    wheel: function (v: View, e: WheelInput, ax: number, ay: number, g: Geom): View {
      if (!e.ctrl) return panBy(v, -e.dx, -e.dy, g);
      return zoomAt(v, v.z * (e.dy < 0 ? b.wheelStep : 1 / b.wheelStep), ax, ay, g);
    },
    keyZoom: function (v: View, key: string, g: Geom): View | null {
      if (key === '=' || key === '+') return zoomAt(v, v.z * b.step, null, null, g);
      if (key === '-' || key === '_') return zoomAt(v, v.z / b.step, null, null, g);
      if (key === '0') return reset(g);
      return null;
    },
    centerOn: function (z: number, cx: number, cy: number, g: Geom): View {
      var nz = bound(z);
      return clampView({ z: nz, tx: g.vw / 2 - cx * nz, ty: g.vh / 2 - cy * nz }, g, true);
    },
    overflows: function (v: View, g: Geom): boolean {
      return g.cw * v.z > g.vw + 1 || g.ch * v.z > g.vh + 1;
    },
  };
}
