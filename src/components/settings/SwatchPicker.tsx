import { parseColor, toHex } from "../../themeTokens";

interface Swatch {
  name: string;
  color: string;
}

interface Props {
  /** Current color; the matching swatch is marked selected. */
  value: string;
  swatches: Swatch[];
  onChange: (color: string) => void;
  /** Accessible name for the group. */
  label?: string;
}

/** Row of one-click color swatches plus a "custom" swatch that opens the
 *  native picker (and shows the current color when it isn't a suggestion). */
export default function SwatchPicker({ value, swatches, onChange, label }: Props) {
  const parsed = parseColor(value);
  const current = parsed ? toHex(parsed) : "#000000";
  const isSuggested = swatches.some(s => s.color.toLowerCase() === current);

  return (
    <div className="settings-swatches" role="radiogroup" aria-label={label}>
      {swatches.map(s => {
        const selected = s.color.toLowerCase() === current;
        return (
          <button
            key={s.name}
            type="button"
            role="radio"
            aria-checked={selected}
            aria-label={s.name}
            title={s.name}
            className={`settings-swatch${selected ? " selected" : ""}`}
            style={{ background: s.color }}
            onClick={() => onChange(s.color)}
          />
        );
      })}
      <label
        className={`settings-swatch custom${isSuggested ? "" : " selected"}`}
        title="Any color…"
        style={isSuggested ? undefined : { background: current }}
      >
        <input
          type="color"
          aria-label="Any color"
          value={current}
          onChange={e => onChange(e.target.value)}
        />
      </label>
    </div>
  );
}
