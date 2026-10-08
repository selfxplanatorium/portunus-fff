// KWin blur region for the launcher card (backend: kde_blur.rs, command
// `set_blur_region`). The window is a transparent 960x640 surface; only the
// card is visible, so only the card's outline should be blurred.

export interface BlurRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Strips per rounded corner. 5 keeps the staircase under ~1px of error at the
 *  card's radius while sending 11 rects per update. */
const CORNER_STEPS = 5;

/** A rounded rectangle as stacked axis-aligned rects (a wl_region can only add
 *  rects): one full-width middle band, plus `steps` strips at the top and the
 *  bottom, each inset to where the corner circle sits at the strip's midline. */
export function roundedRectRegion(
  x: number, y: number, w: number, h: number, radius: number, steps = CORNER_STEPS,
): BlurRect[] {
  const r = Math.max(0, Math.min(radius, w / 2, h / 2));
  if (r < 1 || steps < 1) return [{ x, y, w, h }];
  const out: BlurRect[] = [{ x, y: y + r, w, h: h - 2 * r }];
  const band = r / steps;
  for (let i = 0; i < steps; i++) {
    const dy = r - (i + 0.5) * band; // strip midline, measured from the circle center
    const inset = r - Math.sqrt(r * r - dy * dy);
    const sx = x + inset;
    const sw = w - 2 * inset;
    out.push({ x: sx, y: y + i * band, w: sw, h: band });
    out.push({ x: sx, y: y + h - (i + 1) * band, w: sw, h: band });
  }
  return out;
}

/** The card's blur outline in window logical pixels (= wl surface coords).
 *  The UI scale sets CSS `zoom` on the root, and engines differ on whether
 *  getBoundingClientRect reports zoomed or unzoomed units; the root fills the
 *  viewport, so the viewport-to-root ratio converts either way (1 when rects
 *  are already in viewport px). */
export function cardBlurRegion(card: HTMLElement): BlurRect[] {
  const root = document.documentElement.getBoundingClientRect();
  const k = root.width > 0 ? window.innerWidth / root.width : 1;
  const r = card.getBoundingClientRect();
  const radius = parseFloat(getComputedStyle(card).borderTopLeftRadius) || 0;
  return roundedRectRegion(r.left * k, r.top * k, r.width * k, r.height * k, radius * k);
}
