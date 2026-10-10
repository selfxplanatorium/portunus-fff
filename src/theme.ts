import { invoke } from "@tauri-apps/api/core";
import type { Config } from "./types";
import { colorDeclarations, resolveTheme } from "./themeTokens";

const MATUGEN_THEME = "matugen";
const MATUGEN_STYLE_ID = "matugen-theme";
const CUSTOM_STYLE_ID = "custom-colors";

/** Fetch the external matugen.css from the backend and inject it as a <style>
 *  element. The CSS is scoped to `:root[data-theme="matugen"]`, so it only takes
 *  effect when that theme is active and is harmless to leave in the document.
 *  Missing/empty file → no rules → vars fall back to App.css :root defaults. */
export async function injectMatugenTheme() {
  const css = (await invoke<string | null>("get_custom_theme_css")) ?? "";
  let el = document.getElementById(MATUGEN_STYLE_ID) as HTMLStyleElement | null;
  if (!el) {
    el = document.createElement("style");
    el.id = MATUGEN_STYLE_ID;
    document.head.appendChild(el);
  }
  el.textContent = css;
}

// Fallback stacks behind a configured family. Kept in step with the
// --font-ui / --font-mono defaults on :root in App.css.
const UI_STACK = '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif';
const MONO_STACK = '"JetBrains Mono", "Fira Code", monospace';

function setFont(root: HTMLElement, prop: string, family: string | undefined, stack: string) {
  const name = (family ?? "").replace(/["\\]/g, "").trim();
  if (name) root.style.setProperty(prop, `"${name}", ${stack}`);
  else root.style.removeProperty(prop);
}

/** Theme tokens that paint the launcher's background panels. Translucency
 *  fades exactly these, so text, icons, rows and borders keep full contrast. */
const SURFACE_TOKENS = ["--bg-card", "--bg-preview", "--bg-sidebar", "--bg-bar", "--bg-deep", "--bg-footer"];

/** Override each surface token inline with a translucent copy of the active
 *  theme's value. The theme value is read back with the inline override
 *  removed, so the mix never references itself. Launcher window only: the
 *  settings window is an opaque, decorated surface. */
function applySurfaceOpacity(root: HTMLElement, opacity: number | undefined) {
  if (root.classList.contains("settings-win")) return;
  for (const t of SURFACE_TOKENS) root.style.removeProperty(t);
  const alpha = Math.min(1, Math.max(0.3, opacity ?? 1));
  if (alpha >= 1) return;
  const computed = getComputedStyle(root);
  for (const t of SURFACE_TOKENS) {
    const v = computed.getPropertyValue(t).trim();
    if (v) root.style.setProperty(t, `color-mix(in srgb, ${v} ${Math.round(alpha * 100)}%, transparent)`);
  }
}

/** Paint preset colors + unsaved edits over the base theme. The rule is
 *  gated on `data-custom-colors` and outranks `:root[data-theme="…"]` on
 *  specificity, so stylesheet order doesn't matter. Kept out of inline style
 *  because the opacity pass rewrites the surface tokens inline. */
function applyCustomColors(root: HTMLElement, colors: Record<string, string>) {
  const decls = colorDeclarations(colors);
  let el = document.getElementById(CUSTOM_STYLE_ID) as HTMLStyleElement | null;
  if (!decls) {
    root.removeAttribute("data-custom-colors");
    el?.remove();
    return;
  }
  if (!el) {
    el = document.createElement("style");
    el.id = CUSTOM_STYLE_ID;
    document.head.appendChild(el);
  }
  el.textContent = `:root[data-theme][data-custom-colors] {\n${decls}\n}`;
  root.setAttribute("data-custom-colors", "");
}

export function applyTheme(appearance: Config["appearance"]) {
  const root = document.documentElement;
  const { base, preset, overrides } = resolveTheme(appearance);
  root.setAttribute("data-theme", base);
  applyCustomColors(root, { ...preset, ...overrides });
  setFont(root, "--font-ui", appearance.font_family, UI_STACK);
  setFont(root, "--font-mono", appearance.mono_font_family, MONO_STACK);
  // The whole UI scales via root zoom. Publish the factor and its reciprocal:
  // an <iframe> inside a zoomed document gets a layout viewport that does not
  // match its painted box in WebKitGTK, so the extension HTML preview cancels
  // the zoom on the frame element and re-applies it inside (ExtensionPreview).
  const zoom = appearance.font_size / 13;
  root.style.zoom = String(zoom);
  root.style.setProperty("--ui-zoom", String(zoom));
  root.style.setProperty("--ui-zoom-inv", String(1 / zoom));
  root.dataset.animateResults = String(appearance.animate_results ?? "slide");
  root.dataset.showMetadata = String(appearance.show_metadata ?? true);
  root.dataset.slideSelection = String(appearance.slide_selection ?? true);
  root.dataset.accentBleed = String(appearance.accent_bleed ?? "subtle");
  root.dataset.windowMotion = appearance.window_animations === false ? "off" : "on";
  root.style.setProperty("--grain-opacity", String(appearance.grain ?? 0.07));
  // matugen's token values arrive with its stylesheet, so fade after injecting.
  if (base === MATUGEN_THEME) {
    void injectMatugenTheme().then(() => applySurfaceOpacity(root, appearance.opacity));
  } else {
    applySurfaceOpacity(root, appearance.opacity);
  }
}
