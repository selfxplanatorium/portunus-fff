export interface DirEntry {
  path: string;
  /** Keep the index live via inotify (default true). */
  watch?: boolean;
}

export interface ContentDirEntry {
  path: string;
  depth: number;
  extensions: string[] | null;
}

/** Pre-index estimate for one directory (from the `estimate_dir_index` command). */
export interface DirEstimate {
  total_files: number;
  pdf_files: number;
  image_files: number;
  est_secs_min: number;
  est_secs_max: number;
}

export interface Config {
  general: {
    max_results: number;
    onboarding_completed: boolean;
    layer_shell: boolean;
    /** Icon theme for app icons; null/absent auto-detects from the desktop settings. */
    icon_theme?: string | null;
    /** Check GitHub for a newer release. Notify only — never downloads or installs. */
    check_for_updates: boolean;
    /** Hours between automatic release checks. */
    update_check_interval_hours: number;
  };
  providers: {
    apps: boolean;
    files: boolean;
    calc: boolean;
  };
  calc: {
    currency: boolean;
    rate_max_age_hours: number;
  };
  dict: {
    enabled: boolean;
    fill_sparse: boolean;
    correct_misspellings: boolean;
    copy_definition: boolean;
    fill_threshold: number;
    fill_max: number;
  };
  clipboard: {
    paste_mode: "auto" | "copy";
    max_entries: number;
    ocr_images: boolean;
  };
  files: {
    dirs: DirEntry[];
    show_dotfiles: boolean;
    colored_icons: boolean;
    /** Directory names hidden from results (exact whole-component match). */
    ignore: string[];
  };
  search: {
    min_quality: number;
  };
  ranking: RankingConfig;
  debug: {
    log_scores: boolean;
    log_watcher: boolean;
    log_pdf: boolean;
  };
  frecency: {
    enabled: boolean;
    half_life_days: number;
  };
  content: {
    enabled: boolean;
    dirs: ContentDirEntry[];
    extensions: string[];
    max_file_bytes: number;
    ocr_images: boolean;
    ocr_pdf_fallback: boolean;
    ocr_language: string;
    threads: number;
    ocr_highlight: boolean;
    ocr_highlight_cache: boolean;
  };
  appearance: {
    theme: string;
    font_size: number;
    animate_results: "off" | "slide" | "flip";
    show_metadata: boolean;
    slide_selection: boolean;
    grain: number;
    accent_bleed: "off" | "subtle" | "bold";
  };
  keybinds: KeybindsConfig;
  /** Per-extension state keyed by name. Absent = disabled. */
  extensions: Record<string, ExtensionConfigEntry>;
  marketplace: {
    /** Marketplace index URL; non-default values relax https-only for testing. */
    index_url: string;
  };
}

export interface ExtensionConfigEntry {
  enabled: boolean;
  /** Values for the extension's declared settings schema. */
  settings: Record<string, unknown>;
}

/** One or more canonical chord strings ("ctrl+shift+q"). `""`/`[]` = cleared. */
export type ChordList = string | string[];

/** `[keybinds]` config - mirrors `KeybindsConfig` in config.rs. Keys absent =
 *  default; present with an empty value = chord disabled; present = override. */
export interface KeybindsConfig {
  /** Built-in launcher chords, keyed by bare name ("quick-look", "pin"). */
  builtin: Record<string, ChordList>;
  /** Command-catalog bindings keyed by command id; fire anywhere in the launcher. */
  commands: Record<string, ChordList>;
  /** Result-action bindings keyed by action id ("file:copy-path",
   *  "ext:ytm:queue_last"); matched only against the selected result. */
  actions: Record<string, ChordList>;
}

/** `[ranking]` config - mirrors `RankingConfig` in config.rs. */
export interface RankingConfig {
  /** Category priority, first = highest. Keys: calc, app, command, extension, file, dict. */
  category_order: string[];
  /** 0 = pure match quality, 100 = pure launch history. */
  match_vs_history: number;
  /** Per-category weight 0-100, 50 neutral, 0 hides from root search. */
  category_weights: Record<string, number>;
  /** Title match boosts, 0-100 each (1 point = 100k score, 1 band = 1M). */
  match_boost: { exact: number; prefix: number; word_start: number };
  /** Per-extension weight by name, same semantics as category_weights. */
  extension_weights: Record<string, number>;
}

/** Per-result score composition from `search_explain` (playground only). */
export interface ScoreBreakdown {
  base: number;
  match_bonus: number;
  frecency_bonus: number;
  pin_bonus: number;
  /** Positive magnitude, already subtracted from `score`. */
  penalty: number;
}

/** Response of the `search_explain` command. */
export interface ExplainResponse {
  results: SearchResult[];
  /** Extensions that would have run; the playground never executes wasm. */
  skipped_extensions: string[];
}

/** One pin, from `list_pins` - `query` is stored normalized. */
export interface PinEntry {
  query: string;
  result_id: string;
  kind: string;
  /** Display snapshot taken at pin time (survives result deletion). */
  title: string;
  subtitle: string | null;
  created_ms: number;
  /** Best-effort: the pinned result no longer resolves. */
  stale: boolean;
}

/** One user-facing action on an extension result. */
export interface ExtAction {
  id: string;
  label: string;
  hint?: string;
  /** Running this action opens a form - don't hide the launcher optimistically. */
  opens_form?: boolean;
  /** Host-validated canonical default chord the extension shipped
   *  ("ctrl+q"); user-overridable via [keybinds.actions]. */
  shortcut?: string;
}

/** One remembered extension action, from `list_extension_actions` - the
 *  bounded "seen actions" catalog the Keybinds settings section lists. */
export interface SeenExtAction {
  id: string;
  label: string;
  hint?: string;
  shortcut?: string;
}

/** Wire DTO an extension returned for a result; round-tripped on activate/preview. */
export interface ExtensionResult {
  id: string;
  title: string;
  subtitle?: string;
  relevance: number;
  /** First action is the default on Enter; the rest via the action picker. */
  actions?: ExtAction[];
  /** Unvalidated - render `SearchResult.icon_data_uri` instead. */
  icon?: { mime: string; data_base64: string };
  /** Small chip shown right-aligned on the result row. */
  badge?: string;
}

/** Declarative preview content returned by an extension's `preview` export. */
export type PreviewContent =
  | { type: "markdown"; content: string }
  | { type: "metadata"; items: { label: string; value: string }[] }
  | { type: "image"; mime: string; data_base64: string }
  | { type: "list"; items: { title: string; subtitle?: string; tag?: string; mono?: boolean }[] }
  | { type: "sections"; items: { heading?: string; rows: string[][] }[] }
  | { type: "code"; lang: string; content: string }
  | { type: "html"; content: string };

export interface ExtensionPermissions {
  network: string[];
  kv: boolean;
  clipboard: boolean;
  open_url: boolean;
  /** May inject synthetic paste keystrokes into other applications. */
  paste: boolean;
  /** Allowlist of OS commands the extension may spawn. Non-empty ⇒ the
   *  extension can run programs outside the wasm sandbox (sandbox-breaking). */
  spawn: string[];
  /** May exchange messages with an unsandboxed companion process over the
   *  portunus socket (message bus). */
  bus: boolean;
  /** Declares at least one `type = "secret"` setting (stored in the keyring). */
  has_secrets: boolean;
}

/** One input field of a form an extension's `activate` returned. */
export interface FormFieldDto {
  key: string;
  label: string;
  /** "text" | "textarea" | "password" | "select" | "checkbox" | "number"; unknown renders as text. */
  type: string;
  required?: boolean;
  placeholder?: string;
  default?: unknown;
  options?: { value: string; label: string }[];
}

/** Form an extension asked the launcher to render (ShowForm effect). */
export interface FormDto {
  title: string;
  fields: FormFieldDto[];
  submitAction: string;
  submitLabel: string | null;
}

export type ToastLevel = "info" | "success" | "error";

export interface ToastDto {
  message: string;
  level: ToastLevel;
}

/** A scope frame to push (PushScope effect). `command` = null drills deeper
 *  into the current command; a name/id enters another command's scope. `data`
 *  is the opaque per-frame blob forwarded to the scoped search. */
export interface PushScopeDto {
  command: string | null;
  data: string | null;
  chip: string | null;
  placeholder: string | null;
  query: string | null;
}

/** Response of the `extension_activate` command; side effects already ran. */
export interface ActivateResponse {
  hide: boolean;
  form: FormDto | null;
  toasts: ToastDto[];
  refreshResults: boolean;
  setQuery: string | null;
  /** Reset the selection highlight to the first result (SelectFirst effect). */
  selectFirst: boolean;
  /** Push a new scope frame onto the launcher's stack (drill-in menuing). */
  pushScope?: PushScopeDto | null;
  /** Pop one scope frame off the stack (native "back"). */
  popScope: boolean;
}

/** One `[[settings]]` entry from an extension's manifest. */
export interface ExtensionSettingSpec {
  key: string;
  /** "string" | "bool" | "number" | "select" | "secret" */
  type: string;
  label: string;
  description: string;
  default: unknown;
  options: string[];
  min: number | null;
  max: number | null;
  step: number | null;
  placeholder: string;
}

/** One command an extension declares (manifest [[commands]]). */
export interface ExtensionCommandInfo {
  name: string;
  title: string;
  mode: ModeKind;
  /** Search synonyms folded into the command's fuzzy match. */
  keywords: string[];
  /** Command that runs live in root search on every keystroke. */
  always: boolean;
}

/** One installed extension, as reported by the `list_extensions` command. */
export interface ExtensionInfo {
  name: string;
  version: string;
  description: string;
  author: string;
  homepage: string;
  permissions: ExtensionPermissions | null;
  enabled: boolean;
  loaded: boolean;
  error: string | null;
  benched: boolean;
  /** Set when the manifest declares a [background] refresh interval. */
  background_interval_secs: number | null;
  /** The extension's declared [[commands]]. */
  commands: ExtensionCommandInfo[];
  /** Result kind the extension emits (drives launcher group labels). */
  kind: string | null;
  settings_schema: ExtensionSettingSpec[];
  settings_values: Record<string, unknown>;
  /** Permissions grew past the consented snapshot - blocked until re-approved. */
  needs_reconsent: boolean;
  /** "marketplace" | "url" | "file" | "dev" - install origin, when recorded. */
  origin: string | null;
  origin_url: string | null;
  /** Extensions dir entry is a symlink (`portunus ext dev`). */
  dev: boolean;
  /** Secret setting keys that currently have a stored keyring value. */
  secrets_set: string[];
}

/** Staged install description from `preview_extension_install`. */
export interface InstallPreview {
  name: string;
  version: string;
  description: string;
  author: string;
  homepage: string;
  permissions: ExtensionPermissions;
  keywords: string[];
  sha256: string;
  size_bytes: number;
  replaces: { old_version: string; permissions_grew: boolean } | null;
  staging_token: string;
}

/** One available update from the marketplace index (`marketplace_updates`). */
export interface MarketplaceUpdateInfo {
  name: string;
  installed_version: string;
  index_version: string;
  /** The new version asks for permissions the consented snapshot lacks. */
  permissions_grew: boolean;
  /** The new version's full permission set (from the index entry), for
   *  diffing against the consented snapshot and gating a grown spawn list. */
  permissions: ExtensionPermissions;
}

/** A stable Portunus release the app is willing to point at. Mirrors the Rust
 *  `app_update::ReleaseInfo`; `url` is built in the backend from the tag, never
 *  taken from the API response. */
export interface ReleaseInfo {
  version: string;
  tag: string;
  url: string;
  published_at: string;
}

/** What the About section reads (`app_update_status`). Mirrors the Rust
 *  `app_update::UpdateStatus`. The enable toggle and the interval are not here -
 *  they come from `Config.general`, which is their only source of truth. */
export interface AppUpdateStatus {
  current_version: string;
  latest: ReleaseInfo | null;
  update_available: boolean;
  /** 0 = never checked (fresh install, or checks disabled). */
  checked_at: number;
}

/** Payload of a `kind: "marketplace"` row - the index entry plus install
 *  state; the preview panel renders it as the consent surface. */
export interface MarketplaceResult {
  name: string;
  version: string;
  api: number;
  description: string;
  author: string;
  homepage: string;
  keywords: string[];
  permissions: ExtensionPermissions;
  size_bytes: number;
  state: "not_installed" | "installed" | "update" | "incompatible";
  installed_version: string | null;
  /** Update rows: the new version asks for more than the consented snapshot. */
  permissions_grew: boolean;
  /** A same-name dev symlink exists; marketplace install is blocked. */
  dev_conflict: boolean;
}

export interface ExtensionLogEntry {
  ts_ms: number;
  level: "info" | "error";
  message: string;
}

/** How a command behaves when invoked. */
export type ModeKind = "scope" | "action";

export type CommandSource = { type: "builtin" } | { type: "extension"; name: string };

export type CommandRoute =
  | { type: "builtin"; provider_id: string }
  | { type: "extension"; name: string; command: string }
  /** Frontend swaps in a dedicated component; the backend is not searched. */
  | { type: "ui_takeover" }
  /** Built-in action: activation invokes the named Tauri command with the
   *  optional `args` payload. */
  | { type: "invoke"; command: string; args?: Record<string, unknown> };

/** A searchable launcher command ("Define Word", "Search Issues"). */
export interface CommandDescriptor {
  /** `cmd:<name>` for built-ins, `ext:<name>:cmd:<command>` for extensions. */
  id: string;
  title: string;
  /** Short label for the active-mode chip ("Contents", "Define"). */
  chip: string;
  subtitle?: string;
  source: CommandSource;
  mode_kind: ModeKind;
  /** Search synonyms folded into the command's fuzzy match. */
  keywords: string[];
  /** Input placeholder while the scope is active. */
  placeholder?: string;
  min_query_len: number;
  /** `SearchResult.kind` the command's own results carry. */
  result_kind: string;
  /** Built-in command icon: a named glyph rendered inline (theme-aware). */
  glyph?: string;
  icon_data_uri?: string;
  /** Action command opens a form - don't hide the launcher optimistically. */
  opens_form?: boolean;
  /** Scope results are shown in full, not truncated to max_results (browse
   *  scopes like the marketplace). */
  uncapped?: boolean;
  /** The result set changes underneath the launcher (e.g. a live queue) - a
   *  requery clears this scope's streamed rows first so dropped rows don't
   *  linger. */
  volatile?: boolean;
  /** Manifest-declared default chord (extensions); user-overridable. */
  default_shortcut?: string;
  route: CommandRoute;
}

export interface SearchResult {
  id: string;
  title: string;
  subtitle?: string;
  snippet?: string;
  kind: string;
  score: number;
  exec?: string;
  /** Absolute path of the `.desktop` file behind an app result; passed back to
   *  `launch_app`, which launches it via GIO (`exec` is only the fallback). */
  desktop_file?: string;
  icon_path?: string;
  /** Pre-built `data:` URI for a validated extension-supplied icon. */
  icon_data_uri?: string;
  /** Dominant `#rrggbb` color sampled from the result's icon/art (accent bleed).
   *  Absent for glyph-only results (files/folders) → theme accent fallback. */
  dominant_color?: string;
  file_size?: number;
  created?: number;
  modified?: number;
  /** 0-based PDF page where the content query mainly matched (content provider only). */
  match_page?: number;
  /** Original extension DTO for `ext:` results; passed back on activate/preview. */
  ext?: ExtensionResult;
  /** Extension command that produced this result; passed back on activate/preview. */
  ext_command?: string;
  /** Descriptor behind a `kind: "command"` entry row. */
  command?: CommandDescriptor;
  /** Payload of a `kind: "marketplace"` row (index entry + install state). */
  market?: MarketplaceResult;
  /** True when a pin for the typed query boosted this result to the top.
   *  Optional: frontend-synthesized hint/error rows omit it. */
  pinned?: boolean;
  /** Score composition - present only on `search_explain` results. */
  breakdown?: ScoreBreakdown;
}

/** One extension whose async `query` export is running for the current
 *  keystroke; its batches arrive later as `search-stream` events. */
export interface PendingExt {
  name: string;
  kind: string;
}

/** Response of the `search` command: sync-tier results plus the async tier's
 *  started set. */
export interface SearchResponse {
  query_id: number;
  results: SearchResult[];
  pending: PendingExt[];
}

/** `search-stream` event payload - one streamed batch from an async query. */
export interface StreamPayload {
  query_id: number;
  ext: string;
  results: SearchResult[];
  done: boolean;
  error?: string;
}

/** `extension-preview-chunk` event payload - a streamed preview update that
 *  replaces the rendered content wholesale. */
export interface PreviewChunk {
  request_id: number;
  content: PreviewContent;
}

/** One clipboard history entry, as returned by the `clipboard_list` command. */
export interface ClipboardEntry {
  id: string;
  /** "text" | "image" */
  kind: string;
  /** First-line snippet (text) or the cliphist binary-data label (image). */
  preview: string;
  /** "text" | "url" | "color" | "json" | "image" */
  content_type: string;
  /** Normalized CSS color when content_type === "color". */
  color: string | null;
  byte_size: number | null;
  dimensions: [number, number] | null;
  format: string | null;
  /** Image entries: cached OCR'd text, searchable. Null until OCR'd. */
  ocr_text?: string | null;
}

export interface ClipboardCapabilities {
  /** Enter pastes into the focused window (wtype on Wayland) vs copy-only. */
  smart_paste: boolean;
}

export interface DepStatus {
  id: string;
  label: string;
  feature: string;
  available: boolean;
  install_hint: string;
}

/** Detected desktop environment (de_setup.rs). */
export type DesktopEnv = "hyprland" | "gnome" | "kde" | "sway" | "niri" | "river" | "other";

/** Raw facts for the onboarding DE-setup step; snippets are composed frontend-side. */
export interface DeSetupInfo {
  de: DesktopEnv;
  /** Absolute binary path (or bare "portunus") for snippets and autostart Exec=. */
  exec_path: string;
}

/** Normalized [x, y, w, h], 0..1, top-left origin (same space as the PDF/OCR
 *  highlight rects). Mirrors the backend's `[f32; 4]`. */
export type NormRect = [number, number, number, number];

/** One word of a PDF page text layer, from `pdf_text_layer` (preview.rs). */
export interface PdfTextWord {
  text: string;
  rect: NormRect;
}

export interface PdfTextLine {
  rect: NormRect;
  words: PdfTextWord[];
}

export interface PdfTextLayerData {
  /** Page size in PDF points (aspect/font sizing; rects are normalized). */
  page_w: number;
  page_h: number;
  /** The extraction hit its char/word caps; the layer covers a prefix. */
  truncated: boolean;
  lines: PdfTextLine[];
}

/** Which renderer produced an office document, and therefore how the frontend
 *  presents it: a flowing page column, a scrolling sheet, or a fixed-geometry
 *  slide. Mirrors the backend `office::Shape`. */
export type OfficeShape = "doc" | "sheet" | "slide";

/** One rendered section of an office document, from `render_office_doc`
 *  (preview.rs). Mirrors `office::OfficeDoc` (serialized camelCase). */
export interface OfficeDoc {
  /** Body fragment, escaped by construction, carrying its own `<style>` block.
   *  Self-contained apart from the theme custom properties the host injects. */
  html: string;
  shape: OfficeShape;
  /** Sheet names / slide titles. Length is the section count. */
  sections: string[];
  /** Which section `html` covers. */
  section: number;
  /** Slide canvas size in CSS px at 96dpi; null for docs and sheets. */
  natural: [number, number] | null;
  /** docx page width and padding in CSS px; null for sheets and slides. */
  page: [number, number, number] | null;
  /** Id of the mark to scroll to. Marks carry stable `pm-N` ids and the best
   *  cluster is only known once the section is rendered, so the winner is
   *  reported here rather than as a fixed id in the markup. */
  bestMarkId: string | null;
  truncated: boolean;
  /** Degradation notes for a muted footer - charts without a raster fallback,
   *  clipped rows, missing parts. Surfaced rather than hidden. */
  notes: string[];
}

/** One OCR'd word, from `image_text_layer` / `clipboard_image_text_layer`.
 *  Original case; group by `line` to reconstruct reading order. */
export interface OcrWord {
  text: string;
  rect: NormRect;
  line: number;
}
