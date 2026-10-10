import { parseColor, toHex } from "./themeTokens";

/** How dark the generated surfaces are. */
export type Darkness = "dark" | "darker" | "black";

export interface QuickParams {
  /** The one color the user picks; becomes `--accent`. */
  accent: string;
  darkness: Darkness;
  /** 0 = neutral grey surfaces .. 1 = surfaces clearly tinted with the accent hue. */
  tint: number;
}

/** One-click starting points for the main color. */
export const SUGGESTED_ACCENTS = [
  { name: "Amber", color: "#d6a370" },
  { name: "Coral", color: "#e07a5f" },
  { name: "Rose", color: "#e0729f" },
  { name: "Violet", color: "#a78bfa" },
  { name: "Indigo", color: "#8a9af0" },
  { name: "Sky", color: "#5fb4e8" },
  { name: "Teal", color: "#4fc1b0" },
  { name: "Green", color: "#8cc56b" },
  { name: "Gold", color: "#d4bd5c" },
  { name: "Steel", color: "#a8b4c4" },
];

// ── OKLCH ↔ sRGB ─────────────────────────────────────────────────────────────
// Perceptual lightness steps keep every hue equally readable; plain HSL makes
// yellow surfaces glare and blue ones murky at the same "lightness".

interface Lch { l: number; c: number; h: number }

const toLinear = (v: number) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
const fromLinear = (v: number) => (v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055);

function rgbToOklch(r8: number, g8: number, b8: number): Lch {
  const r = toLinear(r8 / 255), g = toLinear(g8 / 255), b = toLinear(b8 / 255);
  const l = Math.cbrt(0.4122214708 * r + 0.5363335296 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  const L = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
  const A = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const B = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
  const h = (Math.atan2(B, A) * 180) / Math.PI;
  return { l: L, c: Math.hypot(A, B), h: h < 0 ? h + 360 : h };
}

/** Linear sRGB for an OKLCH color (may fall outside 0..1). */
function oklchToLinear({ l: L, c, h }: Lch): [number, number, number] {
  const A = c * Math.cos((h * Math.PI) / 180);
  const B = c * Math.sin((h * Math.PI) / 180);
  const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
}

const inGamut = (rgb: number[]) => rgb.every(v => v >= -1e-4 && v <= 1 + 1e-4);

/** OKLCH → `#rrggbb`, lowering chroma until the color fits sRGB. */
function lch(l: number, c: number, h: number): string {
  let lo = 0, hi = Math.max(0, c);
  const color = { l: Math.min(1, Math.max(0, l)), c: hi, h };
  if (!inGamut(oklchToLinear(color))) {
    for (let i = 0; i < 20; i++) {
      color.c = (lo + hi) / 2;
      if (inGamut(oklchToLinear(color))) lo = color.c; else hi = color.c;
    }
    color.c = lo;
  }
  const [r, g, b] = oklchToLinear(color).map(v => Math.round(fromLinear(Math.min(1, Math.max(0, v))) * 255));
  return toHex({ r, g, b });
}

function rgba(hex: string, a: number): string {
  const c = parseColor(hex)!;
  return `rgba(${c.r}, ${c.g}, ${c.b}, ${a})`;
}

/** sRGB mix of two hex colors, `t` of `b` into `a`. */
function mix(a: string, b: string, t: number): string {
  const x = parseColor(a)!, y = parseColor(b)!;
  return toHex({ r: x.r + (y.r - x.r) * t, g: x.g + (y.g - x.g) * t, b: x.b + (y.b - x.b) * t });
}

// ── palette ──────────────────────────────────────────────────────────────────

/** Lightness of the main window surface per darkness level; every other
 *  surface is an offset from it, mirroring the built-in themes' steps. */
const CARD_L: Record<Darkness, number> = { dark: 0.235, darker: 0.2, black: 0.155 };

/** Every theme token, derived from one main color. */
export function generatePalette({ accent, darkness, tint }: QuickParams): Record<string, string> {
  const rgb = parseColor(accent) ?? { r: 214, g: 163, b: 112, a: 1 };
  const acc = rgbToOklch(rgb.r, rgb.g, rgb.b);
  const accentHex = toHex(rgb);
  const h = acc.h;
  const t = Math.min(1, Math.max(0, tint));
  // Surface chroma: a whisper of the accent hue, never more than the accent has.
  const bgC = t * Math.min(0.045, acc.c * 0.5);
  const fgC = t * Math.min(0.02, acc.c * 0.2);
  const L = CARD_L[darkness];
  const deepStep = darkness === "black" ? 0.06 : 0.05;
  const bg = (dl: number) => lch(L + dl, bgC, h);

  const card = bg(0);
  // Text on the accent: whichever end of the scale reads against it.
  const onAccent = acc.l > 0.62 ? lch(0.2, Math.min(0.04, acc.c * 0.3), h) : lch(0.98, 0.01, h);
  const syntax = (dh: number, l = 0.78, c = 0.11) => lch(l, Math.max(c * 0.6, Math.min(c, acc.c || c)), (h + dh) % 360);

  return {
    "bg-card": card,
    "bg-preview": bg(-0.03),
    "bg-sidebar": bg(-0.02),
    "bg-bar": bg(0.02),
    "bg-deep": bg(-deepStep),
    "bg-footer": bg(-0.012),
    "bg-input": bg(0.035),
    "bg-skeleton": bg(0.04),
    "bg-row-hov": bg(0.03),
    "bg-row-sel": mix(card, accentHex, 0.2),

    "line": bg(0.04),
    "line-soft": bg(0.02),
    "border": bg(0.07),
    "border-raised": bg(0.11),

    "fg": lch(0.91, fgC, h),
    "fg-mute": lch(0.66, fgC, h),
    "fg-desc": lch(0.6, fgC, h),
    "fg-dim": lch(0.47, fgC, h),

    "accent": accentHex,
    "accent-soft": rgba(accentHex, 0.12),
    "accent-border": rgba(accentHex, 0.45),

    "kbd-bg": bg(0.035),
    "kbd-fg": lch(0.76, fgC, h),

    "icon-bg": bg(0.045),
    "icon-bg-sel": mix(bg(0.045), accentHex, 0.22),
    "icon-fg-mute": lch(0.64, fgC, h),

    "text-on-accent": onAccent,

    "danger-bg": lch(0.37, 0.1, 25),
    "danger-bg-dim": lch(0.27, 0.06, 25),
    "danger-fg": lch(0.88, 0.05, 25),

    "toggle-bg": bg(0.06),
    "toggle-thumb": lch(0.55, fgC, h),

    "quote-fg": lch(0.68, acc.c * 0.7, h),

    "hljs-keyword": accentHex,
    "hljs-string": syntax(120),
    "hljs-number": syntax(60, 0.8, 0.1),
    "hljs-comment": lch(0.5, fgC, h),
    "hljs-function": syntax(240),
    "hljs-type": syntax(180, 0.78, 0.09),
    "hljs-builtin": syntax(30, 0.72, 0.12),
    "hljs-attr": lch(0.8, Math.min(0.06, acc.c * 0.4), h),
    "hljs-variable": lch(0.84, fgC, h),
  };
}

/** Best guess at the quick settings that produced the current colors, so the
 *  controls open where the user left them (nothing extra is stored). */
export function estimateParams(colors: Record<string, string>): QuickParams {
  const accent = colors.accent ?? "#d6a370";
  const a = parseColor(accent);
  const c = parseColor(colors["bg-card"] ?? "");
  const acc = a ? rgbToOklch(a.r, a.g, a.b) : { l: 0.7, c: 0.1, h: 60 };
  const card = c ? rgbToOklch(c.r, c.g, c.b) : { l: CARD_L.dark, c: 0, h: 0 };
  const levels = Object.entries(CARD_L) as [Darkness, number][];
  const darkness = levels.reduce((best, cur) =>
    Math.abs(cur[1] - card.l) < Math.abs(best[1] - card.l) ? cur : best)[0];
  const maxC = Math.min(0.045, acc.c * 0.5);
  const tint = maxC > 0.002 ? Math.min(1, card.c / maxC) : 0;
  return { accent: a ? toHex(a) : accent, darkness, tint: Math.round(tint * 20) / 20 };
}
