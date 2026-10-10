import type { Config, CustomTheme } from "./types";

/** `theme` value prefix that selects a user preset from `custom_themes`. */
export const CUSTOM_PREFIX = "custom:";
export const DEFAULT_THEME = "warm-dark";

export interface TokenDef {
  /** Custom-property name without the leading `--`. */
  name: string;
  label: string;
}

export interface TokenGroup {
  title: string;
  tokens: TokenDef[];
}

/** Every color token a theme defines (see themes.css), grouped for the editor. */
export const TOKEN_GROUPS: TokenGroup[] = [
  {
    title: "Surfaces",
    tokens: [
      { name: "bg-card", label: "Window" },
      { name: "bg-bar", label: "Search bar" },
      { name: "bg-preview", label: "Preview pane" },
      { name: "bg-sidebar", label: "Sidebar" },
      { name: "bg-footer", label: "Footer" },
      { name: "bg-deep", label: "Deep background" },
      { name: "bg-input", label: "Inputs" },
      { name: "bg-skeleton", label: "Loading skeleton" },
      { name: "bg-row-hov", label: "Row hover" },
      { name: "bg-row-sel", label: "Row selected" },
    ],
  },
  {
    title: "Text",
    tokens: [
      { name: "fg", label: "Primary text" },
      { name: "fg-mute", label: "Muted text" },
      { name: "fg-desc", label: "Descriptions" },
      { name: "fg-dim", label: "Dim text" },
      { name: "text-on-accent", label: "Text on accent" },
      { name: "quote-fg", label: "Quotes" },
    ],
  },
  {
    title: "Accent",
    tokens: [
      { name: "accent", label: "Accent" },
      { name: "accent-soft", label: "Accent tint" },
      { name: "accent-border", label: "Accent border" },
    ],
  },
  {
    title: "Lines",
    tokens: [
      { name: "line", label: "Dividers" },
      { name: "line-soft", label: "Soft dividers" },
      { name: "border", label: "Borders" },
      { name: "border-raised", label: "Raised borders" },
    ],
  },
  {
    title: "Controls",
    tokens: [
      { name: "kbd-bg", label: "Key hint background" },
      { name: "kbd-fg", label: "Key hint text" },
      { name: "icon-bg", label: "Icon tile" },
      { name: "icon-bg-sel", label: "Icon tile selected" },
      { name: "icon-fg-mute", label: "Muted icon" },
      { name: "toggle-bg", label: "Toggle track" },
      { name: "toggle-thumb", label: "Toggle thumb" },
      { name: "danger-bg", label: "Danger" },
      { name: "danger-bg-dim", label: "Danger dim" },
      { name: "danger-fg", label: "Danger text" },
    ],
  },
  {
    title: "Syntax",
    tokens: [
      { name: "hljs-keyword", label: "Keywords" },
      { name: "hljs-string", label: "Strings" },
      { name: "hljs-number", label: "Numbers" },
      { name: "hljs-comment", label: "Comments" },
      { name: "hljs-function", label: "Functions" },
      { name: "hljs-type", label: "Types" },
      { name: "hljs-builtin", label: "Built-ins" },
      { name: "hljs-attr", label: "Attributes" },
      { name: "hljs-variable", label: "Variables" },
    ],
  },
];

export const TOKEN_NAMES = new Set(TOKEN_GROUPS.flatMap(g => g.tokens.map(t => t.name)));

export function findPreset(appearance: Config["appearance"], theme = appearance.theme): CustomTheme | undefined {
  if (!theme.startsWith(CUSTOM_PREFIX)) return undefined;
  const id = theme.slice(CUSTOM_PREFIX.length);
  return (appearance.custom_themes ?? []).find(t => t.id === id);
}

export interface ResolvedTheme {
  /** Built-in theme whose stylesheet supplies every token not overridden. */
  base: string;
  /** The selected preset's colors (empty for a built-in theme). */
  preset: Record<string, string>;
  /** Unsaved per-token edits on top of the preset. */
  overrides: Record<string, string>;
}

/** Split the active appearance into base theme + preset colors + live edits.
 *  A dangling `custom:` id falls back to the default theme. */
export function resolveTheme(appearance: Config["appearance"]): ResolvedTheme {
  const overrides = appearance.colors ?? {};
  if (appearance.theme.startsWith(CUSTOM_PREFIX)) {
    const p = findPreset(appearance);
    return { base: p?.base || DEFAULT_THEME, preset: p?.colors ?? {}, overrides };
  }
  return { base: appearance.theme || DEFAULT_THEME, preset: {}, overrides };
}

/** A token value is spliced into a stylesheet, so it may not escape its
 *  declaration. Anything the browser doesn't accept as a color is dropped. */
export function isSafeColor(v: string): boolean {
  if (!v || /[;{}<>\\]/.test(v)) return false;
  return typeof CSS === "undefined" || CSS.supports("color", v);
}

/** Merged color overrides as CSS declarations, unknown tokens and unsafe
 *  values filtered out. */
export function colorDeclarations(colors: Record<string, string>): string {
  return Object.entries(colors)
    .filter(([k, v]) => TOKEN_NAMES.has(k) && isSafeColor(v.trim()))
    .map(([k, v]) => `  --${k}: ${v.trim()};`)
    .join("\n");
}

// ── color parsing ────────────────────────────────────────────────────────────

let ctx: CanvasRenderingContext2D | null = null;

/** Parse any CSS color into 0-255 RGB + 0-1 alpha via the canvas normalizer
 *  (it reports `#rrggbb` or `rgba(r, g, b, a)`). `null` for non-colors. */
export function parseColor(v: string): { r: number; g: number; b: number; a: number } | null {
  if (!isSafeColor(v.trim())) return null;
  ctx ??= document.createElement("canvas").getContext("2d");
  if (!ctx) return null;
  ctx.fillStyle = "#000";
  ctx.fillStyle = v.trim();
  const out = String(ctx.fillStyle);
  const hex = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(out);
  if (hex) return { r: parseInt(hex[1], 16), g: parseInt(hex[2], 16), b: parseInt(hex[3], 16), a: 1 };
  const m = /rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)(?:\s*,\s*([\d.]+))?\s*\)/.exec(out);
  if (!m) return null;
  return { r: +m[1], g: +m[2], b: +m[3], a: m[4] === undefined ? 1 : +m[4] };
}

export function toHex({ r, g, b }: { r: number; g: number; b: number }): string {
  return "#" + [r, g, b].map(c => Math.round(c).toString(16).padStart(2, "0")).join("");
}

/** `hex` with `alpha` applied: plain hex when opaque, else `rgba(…)`. */
export function withAlpha(hex: string, alpha: number): string {
  if (alpha >= 1) return hex;
  const c = parseColor(hex);
  if (!c) return hex;
  return `rgba(${c.r}, ${c.g}, ${c.b}, ${+alpha.toFixed(2)})`;
}
