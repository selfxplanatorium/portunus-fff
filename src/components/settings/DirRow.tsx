import { KeyboardEvent, RefObject } from "react";
import Toggle from "./Toggle";

const TrashIcon = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14H6L5 6"/><path d="M10 11v6"/><path d="M14 11v6"/><path d="M9 6V4h6v2"/>
  </svg>
);

const CheckIcon = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
    <polyline points="20 6 9 17 4 12"/>
  </svg>
);

const XIcon = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
    <line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>
  </svg>
);

/** The middle column: a crawl-depth stepper (content dirs) or a live-watch
 *  toggle (file dirs, which fff crawls to full depth). */
type DirControl =
  | { depth: number; onDepthChange: (depth: number) => void; watch?: never; onWatchChange?: never }
  | { watch: boolean; onWatchChange: (watch: boolean) => void; depth?: never; onDepthChange?: never };

type BaseProps = DirControl & {
  path: string;
  onPathChange: (path: string) => void;
  onRemove: () => void;
};

type ExistingRowProps = BaseProps & {
  draft?: false;
};

type DraftRowProps = BaseProps & {
  draft: true;
  inputRef: RefObject<HTMLInputElement | null>;
  onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => void;
  onCommit: () => void;
  onDiscard: () => void;
};

type Props = ExistingRowProps | DraftRowProps;

export default function DirRow(props: Props) {
  const { path, onPathChange, onRemove } = props;
  const isDraft = props.draft === true;

  return (
    <div className={`settings-dir-row${isDraft ? " settings-dir-row--draft" : ""}`}>
      <input
        ref={isDraft ? (props as DraftRowProps).inputRef : undefined}
        className="settings-dir-path"
        value={path}
        placeholder="~/path/to/dir"
        onChange={e => onPathChange(e.target.value)}
        onKeyDown={isDraft ? (props as DraftRowProps).onKeyDown : undefined}
      />
      {props.onDepthChange ? (
        <div className="settings-dir-depth">
          <button className="settings-dir-depth-btn" onClick={() => props.onDepthChange(Math.max(1, props.depth - 1))}>−</button>
          <span className="settings-dir-depth-val" title="Search depth">{props.depth}</span>
          <button className="settings-dir-depth-btn" onClick={() => props.onDepthChange(Math.min(10, props.depth + 1))}>+</button>
        </div>
      ) : (
        <div className="settings-dir-watch" title="Watch for changes (one inotify watch per directory)">
          <Toggle label="Watch for changes" checked={props.watch} onChange={props.onWatchChange} />
        </div>
      )}
      {isDraft ? (
        <>
          <button
            className="settings-dir-confirm"
            onClick={(props as DraftRowProps).onCommit}
            disabled={path.trim() === ""}
            title="Confirm"
          >
            <CheckIcon />
          </button>
          <button className="settings-dir-remove" onClick={(props as DraftRowProps).onDiscard} title="Discard">
            <XIcon />
          </button>
        </>
      ) : (
        <button className="settings-dir-remove" onClick={onRemove} title="Remove">
          <TrashIcon />
        </button>
      )}
    </div>
  );
}
