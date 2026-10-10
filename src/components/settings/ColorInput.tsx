import { useEffect, useState } from "react";
import { parseColor, toHex, withAlpha } from "../../themeTokens";

interface Props {
  /** Current CSS color (any syntax the browser accepts). */
  value: string;
  onChange: (v: string) => void;
  /** Accessible name. */
  label?: string;
  /** Show a reset button (the value differs from its default). */
  modified?: boolean;
  onReset?: () => void;
}

/**
 * Color control: a swatch that opens the native picker plus a free-form field
 * for any CSS color. The picker only speaks opaque hex, so a translucent value
 * keeps its alpha when re-picked; type `rgba(…)` to change the alpha itself.
 */
export default function ColorInput({ value, onChange, label, modified, onReset }: Props) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);

  const parsed = parseColor(value);
  const invalid = draft.trim() !== value && parseColor(draft) === null;

  const commit = () => {
    const v = draft.trim();
    if (v === value) return;
    if (parseColor(v)) onChange(v);
    else setDraft(value);
  };

  return (
    <div className="settings-color">
      <label className="settings-color-swatch" title="Pick a color">
        <span style={{ background: value }} />
        <input
          type="color"
          aria-label={label}
          value={parsed ? toHex(parsed) : "#000000"}
          onChange={e => onChange(withAlpha(e.target.value, parsed?.a ?? 1))}
        />
      </label>
      <input
        type="text"
        className={`settings-text-input mono${invalid ? " invalid" : ""}`}
        value={draft}
        aria-label={label && `${label} value`}
        spellCheck={false}
        onChange={e => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={e => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") { e.stopPropagation(); setDraft(value); }
        }}
      />
      <button
        type="button"
        className="settings-color-reset"
        title="Reset to the theme's color"
        aria-label={label && `Reset ${label}`}
        disabled={!modified}
        onClick={onReset}
      >
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
          <path d="M2.5 8a5.5 5.5 0 1 0 1.6-3.9" />
          <polyline points="2.5,2 2.5,5 5.5,5" />
        </svg>
      </button>
    </div>
  );
}
