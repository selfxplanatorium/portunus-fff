import { useState, useEffect, useRef, KeyboardEvent } from "react";
import { Config, DirEntry } from "../../types";
import DirRow from "./DirRow";
import TagEditor from "./TagEditor";
import Toggle from "./Toggle";
import SectionHeader from "./SectionHeader";
import SettingsGroup from "./SettingsGroup";
import SettingsField from "./SettingsField";

interface Props {
  config: Config;
  onChange: (c: Config) => void;
}

export default function FilesSection({ config, onChange }: Props) {
  const [draft, setDraft] = useState<DirEntry | null>(null);
  const draftInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (draft !== null) draftInputRef.current?.focus();
  }, [draft !== null]);

  const setFiles = (patch: Partial<Config["files"]>) =>
    onChange({ ...config, files: { ...config.files, ...patch } });
  const setDirs = (dirs: DirEntry[]) => setFiles({ dirs });

  const updateDir = (i: number, patch: Partial<DirEntry>) =>
    setDirs(config.files.dirs.map((d, idx) => idx === i ? { ...d, ...patch } : d));

  const removeDir = (i: number) =>
    setDirs(config.files.dirs.filter((_, idx) => idx !== i));

  const commitDraft = () => {
    if (!draft || draft.path.trim() === "") return;
    setDirs([...config.files.dirs, draft]);
    setDraft(null);
  };

  const onDraftKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") { e.preventDefault(); commitDraft(); }
    if (e.key === "Escape") { e.preventDefault(); setDraft(null); }
  };

  const enabled = config.providers.files;

  return (
    <div className="settings-section">
      <SectionHeader
        title="Files"
        desc="Which directories are searched by name."
        master={{ checked: enabled, onChange: v => onChange({ ...config, providers: { ...config.providers, files: v } }), label: "Enable file search" }}
      />

      <div className={!enabled ? "settings-disabled" : undefined} aria-hidden={!enabled}>
        <SettingsGroup
          title="Indexed directories"
          desc={<>Each directory is crawled to full depth by the fff indexer. <code>~</code> is your home directory; <code>/</code> is the whole disk minus <code>/proc</code>, <code>/sys</code>, <code>/dev</code>, <code>/run</code> and <code>/tmp</code>. The toggle keeps a directory live via inotify; unwatched ones refresh on startup and on <code>portunus --reindex</code>.</>}
        >
          <div className="settings-dir-list">
            {config.files.dirs.map((dir, i) => (
              <DirRow
                key={i}
                path={dir.path}
                watch={dir.watch ?? true}
                onPathChange={path => updateDir(i, { path })}
                onWatchChange={watch => updateDir(i, { watch })}
                onRemove={() => removeDir(i)}
              />
            ))}

            {draft !== null && (
              <DirRow
                draft
                inputRef={draftInputRef}
                path={draft.path}
                watch={draft.watch ?? true}
                onPathChange={path => setDraft({ ...draft, path })}
                onWatchChange={watch => setDraft({ ...draft, watch })}
                onRemove={() => setDraft(null)}
                onKeyDown={onDraftKey}
                onCommit={commitDraft}
                onDiscard={() => setDraft(null)}
              />
            )}

            {draft === null && (
              <button className="settings-dir-add" onClick={() => setDraft({ path: "", watch: true })}>
                <span style={{ fontSize: 14, lineHeight: 1 }}>+</span> Add directory
              </button>
            )}
          </div>
          <SettingsField
            stacked
            name="Ignored directory names"
            desc="Hidden from results wherever they appear, matched against a whole path component. The indexer already skips gitignored trees and common build and cache directories; add names here for anything else."
          >
            <TagEditor
              values={config.files.ignore}
              onChange={ignore => setFiles({ ignore })}
              placeholder="add name…"
              normalize={raw => raw.replace(/\/+$/, "")}
            />
          </SettingsField>
        </SettingsGroup>

        <SettingsGroup title="Display">
          <SettingsField
            name="Show dotfiles"
            desc="Include files and directories whose name or path starts with a dot (e.g. .config, .bashrc). Hidden by default."
          >
            <Toggle label="Show dotfiles" checked={config.files.show_dotfiles} onChange={v => setFiles({ show_dotfiles: v })} />
          </SettingsField>

          <SettingsField
            name="Colored file icons"
            desc="Tint file and folder icons by type, with a distinct glyph per category. When off, all files use a plain monochrome document icon."
          >
            <Toggle label="Colored file icons" checked={config.files.colored_icons} onChange={v => setFiles({ colored_icons: v })} />
          </SettingsField>
        </SettingsGroup>
      </div>
    </div>
  );
}
