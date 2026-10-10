import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import rawCss from "../../themes.css?raw";
import type { CustomTheme } from "../../types";
import { CUSTOM_PREFIX } from "../../themeTokens";

export interface ThemeDef {
  id: string;
  label: string;
  swatches: string[];
  /** Every `--token: value` the theme declares (names without `--`). */
  tokens: Record<string, string>;
}

/** All custom-property declarations in a CSS block body. */
export function parseTokens(body: string): Record<string, string> {
  const out: Record<string, string> = {};
  const re = /--([a-z0-9-]+):\s*([^;\n]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(body)) !== null) out[m[1]] = m[2].trim();
  return out;
}

const SWATCH_VARS = ["bg-card", "bg-bar", "accent", "fg", "fg-mute"];
const swatchesOf = (tokens: Record<string, string>) => SWATCH_VARS.map(n => tokens[n] ?? "#888");

/** Parse themes.css at build time into selectable theme definitions with swatches. */
export function buildThemes(): ThemeDef[] {
  const labelRe = /\/\*\s*──\s+([A-Z][a-zA-Z\s-]+?)(?:\s*\([^)]*\))?\s*──/g;
  const blockRe = /:root\[data-theme="([^"]+)"\]\s*\{([^}]+)\}/g;

  const labels: string[] = [];
  let m: RegExpExecArray | null;
  while ((m = labelRe.exec(rawCss)) !== null) {
    labels.push(m[1].trim());
  }

  const themes: ThemeDef[] = [];
  let i = 0;
  while ((m = blockRe.exec(rawCss)) !== null) {
    const [, id, body] = m;
    const tokens = parseTokens(body);
    themes.push({ id, label: labels[i++] ?? id, swatches: swatchesOf(tokens), tokens });
  }
  return themes;
}

const MATUGEN_PLACEHOLDER = ["#3a3a3a", "#2a2a2a", "#888", "#ddd", "#999"];

export const THEMES: ThemeDef[] = [
  ...buildThemes(),
  // Synthetic entry: matugen colors come from an external file at runtime, so it
  // isn't parsed from themes.css. Selecting it sets data-theme="matugen".
  { id: "matugen", label: "Matugen", swatches: MATUGEN_PLACEHOLDER, tokens: {} },
];

/** Matugen's colors live in an external file generated at runtime, so they
 *  are read from that CSS rather than from computed styles (which only reflect
 *  the *active* theme). Empty until loaded or when the file is absent. */
export function useMatugenTokens(): Record<string, string> {
  const [tokens, setTokens] = useState<Record<string, string>>({});
  useEffect(() => {
    invoke<string | null>("get_custom_theme_css")
      .then(css => { if (css) setTokens(parseTokens(css)); })
      .catch(() => {});
  }, []);
  return tokens;
}

/** Token values of a built-in theme (matugen's from `matugen`). */
export function baseTokens(id: string, matugen: Record<string, string>): Record<string, string> {
  if (id === "matugen") return matugen;
  return THEMES.find(t => t.id === id)?.tokens ?? THEMES[0].tokens;
}

const STYLES = `
.theme-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  gap: 12px;
}

.theme-card {
  position: relative;
  padding: 14px 15px 13px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
  background: var(--kbd-bg);
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
  user-select: none;
  text-align: left;
}

.theme-card:hover {
  border-color: var(--fg-dim);
  background: var(--bg-row-hov);
}

.theme-card.selected {
  border-color: var(--accent);
  background: var(--bg-row-hov);
}

.theme-card-label {
  font-size: var(--fs-desc, 12px);
  font-weight: 600;
  color: var(--fg);
  margin-bottom: 9px;
  letter-spacing: -0.01em;
  line-height: 1;
}

.theme-card-swatches {
  display: flex;
  gap: 4px;
  align-items: center;
}

.theme-swatch {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  flex-shrink: 0;
  box-shadow: inset 0 0 0 1px rgba(255,255,255,0.08);
}

.theme-card.custom .theme-card-label::after {
  content: "custom";
  margin-left: 6px;
  font: 500 var(--fs-micro, 10px)/1 var(--font-mono);
  color: var(--fg-dim);
  letter-spacing: 0;
}

.theme-card-check {
  position: absolute;
  top: 7px;
  right: 7px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: var(--accent);
  display: flex;
  align-items: center;
  justify-content: center;
  opacity: 0;
  transform: scale(0.6);
  transition: opacity 0.15s, transform 0.15s;
}

.theme-card.selected .theme-card-check {
  opacity: 1;
  transform: scale(1);
}
`;

if (typeof document !== "undefined") {
  const id = "theme-grid-styles";
  if (!document.getElementById(id)) {
    const el = document.createElement("style");
    el.id = id;
    el.textContent = STYLES;
    document.head.appendChild(el);
  }
}

interface Props {
  /** Currently-selected theme id (`custom:<id>` for a user preset). */
  value: string;
  onSelect: (id: string) => void;
  /** User presets, listed after the built-in themes. */
  custom?: CustomTheme[];
}

/** Shared theme picker grid with color swatches: built-ins, then user presets. */
export default function ThemeGrid({ value, onSelect, custom = [] }: Props) {
  const matugen = useMatugenTokens();
  const matugenSwatches = swatchesOf(matugen);
  const hasMatugen = SWATCH_VARS.every(n => matugen[n]);

  const cards = [
    ...THEMES.map(t => ({
      id: t.id,
      label: t.label,
      custom: false,
      swatches: t.id === "matugen" && hasMatugen ? matugenSwatches : t.swatches,
    })),
    ...custom.map(p => ({
      id: CUSTOM_PREFIX + p.id,
      label: p.name || p.id,
      custom: true,
      swatches: swatchesOf({ ...baseTokens(p.base, matugen), ...p.colors }),
    })),
  ];

  return (
    <div className="theme-grid">
      {cards.map((t) => {
        const swatches = t.swatches;
        return (
        <button
          key={t.id}
          type="button"
          className={`theme-card${t.custom ? " custom" : ""}${value === t.id ? " selected" : ""}`}
          onClick={() => onSelect(t.id)}
        >
          <div className="theme-card-label">{t.label}</div>
          <div className="theme-card-swatches">
            {swatches.map((color, i) => (
              <span key={i} className="theme-swatch" style={{ background: color }} />
            ))}
          </div>
          <span className="theme-card-check" aria-hidden>
            <svg width="8" height="8" viewBox="0 0 10 10" fill="none">
              <polyline points="1.5,5 4,7.5 8.5,2.5" stroke="var(--text-on-accent)" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"/>
            </svg>
          </span>
        </button>
        );
      })}
    </div>
  );
}
