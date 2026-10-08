import { ReactNode, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Config } from "../../types";
import ThemeGrid from "./ThemeGrid";
import Toggle from "./Toggle";
import Select from "./Select";
import SectionHeader from "./SectionHeader";
import SettingsGroup from "./SettingsGroup";
import SettingsField from "./SettingsField";
import Slider from "./Slider";
import { DeSetupInfo, DesktopEnv } from "../../types";

interface Props {
  config: Config;
  onChange: (c: Config) => void;
}

// Labels are user-facing; values are the config enum. "Smooth" replaces the raw
// "FLIP" jargon, but the stored value stays "flip" for backward compatibility.
const ANIM_OPTIONS = [
  { label: "Off",    value: "off"   },
  { label: "Slide",  value: "slide" },
  { label: "Smooth", value: "flip"  },
] as const;

function animLabel(v: Config["appearance"]["animate_results"]): string {
  return ANIM_OPTIONS.find(o => o.value === v)?.label ?? "Slide";
}

// Accent bleed: tint the selection + its preview with the color sampled from
// each result's own icon/art, instead of the single theme accent.
const BLEED_OPTIONS = [
  { label: "Off",    value: "off"    },
  { label: "Subtle", value: "subtle" },
  { label: "Bold",   value: "bold"   },
] as const;

function bleedLabel(v: Config["appearance"]["accent_bleed"]): string {
  return BLEED_OPTIONS.find(o => o.value === v)?.label ?? "Subtle";
}

// Stands in for `icon_theme: null`, i.e. follow the GTK/gsettings icon theme.
const ICON_THEME_AUTO = "Auto (desktop setting)";

// Stands in for an empty font family, i.e. the built-in stack.
const FONT_DEFAULT = "Default";

// Blur is compositor work: the webview can't blur the desktop behind its own
// window. Hyprland gets the rule applied at runtime; elsewhere, the snippet.
const BLUR_HINTS: Partial<Record<DesktopEnv, ReactNode>> = {
  hyprland: <>Applied to the <code>portunus</code> layer at runtime via <code>hyprctl</code> (needs layer shell on). To keep it across Hyprland reloads, add a blur and an ignore-alpha <code>layerrule</code> for the <code>portunus</code> namespace to hyprland.conf.</>,
  sway: <>Needs SwayFX: add <code>layer_effects "portunus" blur enable</code> to your sway config. Plain sway cannot blur.</>,
  other: <>Your compositor has to do the blurring. Add a blur rule for the <code>portunus</code> layer-shell namespace in its config, if it supports one.</>,
};

export default function AppearanceSection({ config, onChange }: Props) {
  const set = (patch: Partial<Config["appearance"]>) =>
    onChange({ ...config, appearance: { ...config.appearance, ...patch } });
  // Icon theme lives under [general] but belongs next to the other visuals.
  const setGeneral = (patch: Partial<Config["general"]>) =>
    onChange({ ...config, general: { ...config.general, ...patch } });

  const [iconThemes, setIconThemes] = useState<string[]>([]);
  const [fonts, setFonts] = useState<string[]>([]);
  useEffect(() => {
    invoke<string[]>("list_icon_themes").then(setIconThemes).catch(() => setIconThemes([]));
    invoke<string[]>("list_font_families").then(setFonts).catch(() => setFonts([]));
  }, []);
  const [de, setDe] = useState<DesktopEnv | null>(null);
  useEffect(() => {
    invoke<DeSetupInfo>("de_setup_info").then(i => setDe(i.de)).catch(() => setDe(null));
  }, []);

  const { theme, font_size, animate_results, show_metadata, slide_selection, grain, accent_bleed } = config.appearance;
  const opacity = config.appearance.opacity ?? 1;
  const blurHint = BLUR_HINTS[de ?? "other"] ?? BLUR_HINTS.other;

  return (
    <div className="settings-section">
      <SectionHeader title="Appearance" desc="Theme, scale, and launcher visuals." />

      <div className="settings-group-block">
        <div className="settings-group-title">Theme</div>
        <ThemeGrid value={theme} onSelect={id => set({ theme: id })} />
      </div>

      <SettingsGroup title="Display">
        <SettingsField name="Interface scale" desc="Scale the entire launcher UI proportionally.">
          <Slider
            label="Interface scale"
            value={font_size}
            min={11} max={18} step={1}
            format={v => `${v}px`}
            commitOnRelease
            onChange={v => set({ font_size: v })}
          />
        </SettingsField>

        <SettingsField
          name="Result animations"
          desc="How result rows animate: none, a slide-in entrance, or smoothly repositioning retained rows."
        >
          <Select
            options={ANIM_OPTIONS.map(o => ({ label: o.label }))}
            value={animLabel(animate_results)}
            onChange={label => {
              const opt = ANIM_OPTIONS.find(o => o.label === label);
              if (opt) set({ animate_results: opt.value });
            }}
          />
        </SettingsField>

        <SettingsField
          name="Icon theme"
          desc="Icon theme used for app icons. The chosen theme is exhausted before its fallbacks, so results stay visually consistent."
        >
          <Select
            options={[{ label: ICON_THEME_AUTO }, ...iconThemes.map(t => ({ label: t }))]}
            value={config.general.icon_theme || ICON_THEME_AUTO}
            onChange={label =>
              setGeneral({ icon_theme: label === ICON_THEME_AUTO ? null : label })
            }
          />
        </SettingsField>

        <SettingsField name="File metadata" desc="Show the modified/created row in file previews.">
          <Toggle label="File metadata" checked={show_metadata ?? true} onChange={v => set({ show_metadata: v })} />
        </SettingsField>

        <SettingsField name="Sliding selection" desc="Glide the highlight between rows as you navigate.">
          <Toggle label="Sliding selection" checked={slide_selection ?? true} onChange={v => set({ slide_selection: v })} />
        </SettingsField>

        <SettingsField
          name="Accent bleed"
          desc="Tint the selected row and its preview with the color sampled from that result's icon or art, instead of the theme accent."
        >
          <Select
            options={BLEED_OPTIONS.map(o => ({ label: o.label }))}
            value={bleedLabel(accent_bleed)}
            onChange={label => {
              const opt = BLEED_OPTIONS.find(o => o.label === label);
              if (opt) set({ accent_bleed: opt.value });
            }}
          />
        </SettingsField>

        <SettingsField name="Film grain" desc="Faint noise texture over the launcher. 0 turns it off.">
          <Slider
            label="Film grain"
            value={grain ?? 0.07}
            min={0}
            max={0.25}
            step={0.005}
            onChange={v => set({ grain: v })}
            format={v => v === 0 ? "Off" : v.toFixed(3)}
          />
        </SettingsField>
      </SettingsGroup>

      <SettingsGroup title="Window">
        <SettingsField name="Opacity" desc="How see-through the launcher's background is. Text, icons and the selection stay solid.">
          <Slider
            label="Opacity"
            value={opacity}
            min={0.3} max={1} step={0.05}
            format={v => v >= 1 ? "Solid" : `${Math.round(v * 100)}%`}
            onChange={v => set({ opacity: v })}
          />
        </SettingsField>

        <SettingsField name="Blur behind" desc={blurHint}>
          <Toggle label="Blur behind" checked={config.appearance.blur ?? false} onChange={v => set({ blur: v })} />
        </SettingsField>
      </SettingsGroup>

      <SettingsGroup title="Fonts">
        <SettingsField name="Interface font" desc="Used for the search field, results and previews.">
          <Select
            options={[{ label: FONT_DEFAULT }, ...fonts.map(f => ({ label: f }))]}
            value={config.appearance.font_family || FONT_DEFAULT}
            onChange={label => set({ font_family: label === FONT_DEFAULT ? "" : label })}
          />
        </SettingsField>

        <SettingsField name="Monospace font" desc="Used for code, paths and key hints.">
          <Select
            options={[{ label: FONT_DEFAULT }, ...fonts.map(f => ({ label: f }))]}
            value={config.appearance.mono_font_family || FONT_DEFAULT}
            onChange={label => set({ mono_font_family: label === FONT_DEFAULT ? "" : label })}
          />
        </SettingsField>
      </SettingsGroup>
    </div>
  );
}
