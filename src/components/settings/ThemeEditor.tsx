import { useState } from "react";
import { Config, CustomTheme } from "../../types";
import {
  CUSTOM_PREFIX, DEFAULT_THEME, TOKEN_GROUPS, findPreset, parseColor, resolveTheme, withAlpha, toHex,
} from "../../themeTokens";
import ThemeGrid, { THEMES, baseTokens, useMatugenTokens } from "./ThemeGrid";
import ColorInput from "./ColorInput";
import Modal from "./Modal";
import Select from "./Select";
import SettingsField from "./SettingsField";
import SettingsGroup from "./SettingsGroup";
import TextInput from "./TextInput";

type Appearance = Config["appearance"];

interface Props {
  appearance: Appearance;
  set: (patch: Partial<Appearance>) => void;
}

/** Tokens that are translucent copies of the accent; they follow it when it
 *  changes unless the user gave them a color of their own. */
const ACCENT_TINTS = ["accent-soft", "accent-border"];

function slug(name: string, taken: Set<string>): string {
  const base = name.toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "theme";
  let id = base;
  for (let n = 2; taken.has(id); n++) id = `${base}-${n}`;
  return id;
}

function sameRgb(a: string | undefined, b: string | undefined): boolean {
  const x = a ? parseColor(a) : null;
  const y = b ? parseColor(b) : null;
  return !!x && !!y && x.r === y.r && x.g === y.g && x.b === y.b;
}

const omit = (o: Record<string, string>, keys: string[]) =>
  Object.fromEntries(Object.entries(o).filter(([k]) => !keys.includes(k)));

/**
 * Theme picker + full color editor. On a built-in theme, edits are unsaved
 * overrides (`[appearance] colors`) until saved as a preset; on a preset they
 * write straight into that preset.
 */
export default function ThemeEditor({ appearance, set }: Props) {
  const matugen = useMatugenTokens();
  const presets = appearance.custom_themes ?? [];
  const preset = findPreset(appearance);
  const { base, preset: presetColors, overrides } = resolveTheme(appearance);
  const baseValues = baseTokens(base, matugen);
  const effective = { ...baseValues, ...presetColors, ...overrides };
  const dirty = !preset && Object.keys(overrides).length > 0;

  const [editing, setEditing] = useState(false);
  const [saveOpen, setSaveOpen] = useState(false);
  const [saveName, setSaveName] = useState("");
  const [pendingTheme, setPendingTheme] = useState<string | null>(null);
  const [deleteOpen, setDeleteOpen] = useState(false);

  const updatePreset = (patch: Partial<CustomTheme>, extra: Partial<Appearance> = {}) => {
    if (!preset) return;
    set({
      custom_themes: presets.map(p => (p.id === preset.id ? { ...p, ...patch } : p)),
      ...extra,
    });
  };

  /** Write `changes` into the active edit layer (preset or overrides). */
  const writeColors = (changes: Record<string, string>, removed: string[] = []) => {
    if (preset) {
      // Hand-written `colors` in config.toml would shadow the preset; fold
      // the edited tokens out of them so the change is visible.
      const touched = [...Object.keys(changes), ...removed];
      updatePreset(
        { colors: { ...omit(preset.colors, removed), ...changes } },
        touched.some(k => k in overrides) ? { colors: omit(overrides, touched) } : {},
      );
    } else {
      set({ colors: { ...omit(overrides, removed), ...changes } });
    }
  };

  const setToken = (name: string, value: string) => {
    const changes: Record<string, string> = { [name]: value };
    if (name === "accent") {
      const next = parseColor(value);
      const layer = preset ? preset.colors : overrides;
      for (const t of ACCENT_TINTS) {
        const cur = effective[t];
        const tracking = !(t in layer) || sameRgb(cur, effective.accent);
        const a = cur ? parseColor(cur)?.a ?? 1 : 1;
        if (next && tracking) changes[t] = withAlpha(toHex(next), a < 1 ? a : 0.12);
      }
    }
    writeColors(changes);
  };

  const resetToken = (name: string) => writeColors({}, [name]);

  const selectTheme = (id: string) => {
    if (id === appearance.theme) return;
    if (dirty) setPendingTheme(id);
    else set({ theme: id, colors: {} });
  };

  const openSave = () => {
    setSaveName(preset ? `${preset.name} copy` : `${THEMES.find(t => t.id === base)?.label ?? "My"} custom`);
    setSaveOpen(true);
  };

  const savePreset = () => {
    const name = saveName.trim();
    if (!name) return;
    const id = slug(name, new Set(presets.map(p => p.id)));
    const colors = { ...presetColors, ...overrides };
    set({
      custom_themes: [...presets, { id, name, base, colors }],
      // Saved from the "unsaved changes" prompt: carry on to the theme picked.
      theme: pendingTheme ?? CUSTOM_PREFIX + id,
      colors: {},
    });
    setSaveOpen(false);
    setPendingTheme(null);
  };

  const deletePreset = () => {
    if (!preset) return;
    set({ custom_themes: presets.filter(p => p.id !== preset.id), theme: preset.base || DEFAULT_THEME });
    setDeleteOpen(false);
  };

  const editedCount = Object.keys(preset ? { ...preset.colors, ...overrides } : overrides).length;

  return (
    <>
      <div className="settings-group-block">
        <div className="settings-group-title-row">
          <div className="settings-group-title">Theme</div>
          <div className="settings-btn-row">
            {dirty && (
              <button className="settings-btn-danger" onClick={() => set({ colors: {} })}>
                Discard changes
              </button>
            )}
            <button className="settings-btn-secondary" onClick={openSave}>
              {preset ? "Duplicate" : "Save as preset"}
            </button>
            <button
              className={editing ? "settings-btn-primary" : "settings-btn-secondary"}
              onClick={() => setEditing(e => !e)}
              aria-expanded={editing}
            >
              {editing ? "Done" : "Customize colors"}
            </button>
          </div>
        </div>
        <ThemeGrid value={appearance.theme} onSelect={selectTheme} custom={presets} />
        {dirty && (
          <div className="settings-group-desc theme-dirty-note">
            {editedCount} unsaved color {editedCount === 1 ? "change" : "changes"} on top of this theme. Save them as a preset to keep them when switching.
          </div>
        )}
      </div>

      {editing && preset && (
        <SettingsGroup title="Preset">
          <SettingsField name="Name">
            <TextInput
              label="Preset name"
              value={preset.name}
              width={220}
              onChange={name => updatePreset({ name })}
            />
          </SettingsField>
          <SettingsField name="Based on" desc="Colors you haven't changed come from this theme.">
            <Select
              options={THEMES.map(t => ({ label: t.label }))}
              value={THEMES.find(t => t.id === preset.base)?.label ?? preset.base}
              onChange={label => {
                const t = THEMES.find(x => x.label === label);
                if (t) updatePreset({ base: t.id });
              }}
            />
          </SettingsField>
          <SettingsField name="Delete preset" desc="Switches back to the theme it was based on.">
            <button className="settings-btn-danger" onClick={() => setDeleteOpen(true)}>Delete</button>
          </SettingsField>
        </SettingsGroup>
      )}

      {editing && TOKEN_GROUPS.map(group => (
        <SettingsGroup
          key={group.title}
          title={group.title}
          desc={group.title === "Accent" ? "Changing the accent also recolors its tint and border, unless you set those yourself." : undefined}
        >
          {group.tokens.map(t => {
            const own = t.name in overrides || (!!preset && t.name in preset.colors);
            return (
              <SettingsField key={t.name} name={t.label} desc={<code>--{t.name}</code>}>
                <ColorInput
                  label={t.label}
                  value={effective[t.name] ?? ""}
                  modified={own}
                  onChange={v => setToken(t.name, v)}
                  onReset={() => resetToken(t.name)}
                />
              </SettingsField>
            );
          })}
        </SettingsGroup>
      ))}

      {saveOpen && (
        <Modal
          title="Save as preset"
          onClose={() => setSaveOpen(false)}
          footer={
            <>
              <button className="settings-btn-secondary" onClick={() => setSaveOpen(false)}>Cancel</button>
              <button className="settings-btn-primary" disabled={!saveName.trim()} onClick={savePreset}>Save</button>
            </>
          }
        >
          <p style={{ marginTop: 0 }}>Stores every current color as a new preset in your config.</p>
          <TextInput label="Preset name" value={saveName} onChange={setSaveName} autoFocus onEnter={savePreset} />
        </Modal>
      )}

      {pendingTheme !== null && !saveOpen && (
        <Modal
          title="Unsaved color changes"
          onClose={() => setPendingTheme(null)}
          footer={
            <>
              <button className="settings-btn-secondary" onClick={() => setPendingTheme(null)}>Cancel</button>
              <button
                className="settings-btn-danger"
                onClick={() => { set({ theme: pendingTheme, colors: {} }); setPendingTheme(null); }}
              >
                Discard
              </button>
              <button className="settings-btn-primary" onClick={openSave}>Save as preset</button>
            </>
          }
        >
          Switching themes drops the {editedCount} color {editedCount === 1 ? "change" : "changes"} you made to this one. Save them as a preset first?
        </Modal>
      )}

      {deleteOpen && preset && (
        <Modal
          title="Delete preset"
          onClose={() => setDeleteOpen(false)}
          footer={
            <>
              <button className="settings-btn-secondary" onClick={() => setDeleteOpen(false)}>Cancel</button>
              <button className="settings-btn-danger" onClick={deletePreset}>Delete</button>
            </>
          }
        >
          Delete “{preset.name}”? This can't be undone.
        </Modal>
      )}
    </>
  );
}
