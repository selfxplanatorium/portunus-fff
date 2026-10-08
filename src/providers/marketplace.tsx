import { useEffect, useState, useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import { registerProvider, type LaunchContext, type PreviewProps } from "./registry";
import { useTauriListener } from "../hooks/useTauriListener";
import { formatBytes } from "../utils";
import { interpretersIn } from "../spawn";
import { NetIcon, StoreIcon, LinkIcon, ClipIcon, PasteIcon, KeyIcon } from "../components/settings/extensions/permGlyphs";
import type { MarketplaceResult } from "../types";

// Launch routing + preview for marketplace catalog rows. The preview panel is
// the consent surface: it lists every permission the index entry declares, so
// Enter installs a fresh, non-sandbox-breaking extension immediately. Two cases
// arm a confirming second Enter instead of installing outright: a fresh install
// of a `spawn` (sandbox-breaking) extension, and ANY update that grows the
// consented permission set - so a new network host / secret / paste / spawn
// grant gets a deliberate confirmation, not just a passive label.

// ── module state (shared by the launch handler and the preview) ──────────────
const installing = new Set<string>();
let armed: string | null = null;
let stamp = 0;
const listeners = new Set<() => void>();
const bump = () => {
  stamp++;
  listeners.forEach(l => l());
};
const subscribe = (cb: () => void) => {
  listeners.add(cb);
  return () => void listeners.delete(cb);
};
const useMarketStamp = () => useSyncExternalStore(subscribe, () => stamp);

/** Enter (or the action panel's Install row) lands here. */
function startInstall(m: MarketplaceResult, ctx: LaunchContext) {
  if (installing.has(m.name)) return;
  // Arm a confirming second Enter for a fresh sandbox-breaking install, or any
  // update that grows the consented permissions (spawn or otherwise).
  const freshSpawn = m.state === "not_installed" && m.permissions.spawn.length > 0;
  const growsOnUpdate = m.state === "update" && m.permissions_grew;
  const needsConfirm = freshSpawn || growsOnUpdate;
  if (needsConfirm && armed !== m.name) {
    armed = m.name;
    bump();
    return;
  }
  const wasUpdate = m.state === "update";
  installing.add(m.name);
  armed = null;
  bump();
  invoke("marketplace_install", { name: m.name })
    .then(() => {
      ctx.pushToast(`${m.name} ${wasUpdate ? "updated" : "installed"} — ready to use`, "success");
      ctx.requery();
    })
    .catch(e => ctx.pushToast(String(e), "error"))
    .finally(() => {
      installing.delete(m.name);
      bump();
    });
}

registerProvider({
  kinds: ["marketplace", "marketplace-msg"],
  Preview: MarketplacePreview,
  bindableActions: [
    { id: "market:uninstall", title: "Uninstall Extension", hint: "Marketplace" },
  ],
  handleLaunch: (result, ctx) => {
    if (result.kind === "marketplace-msg") return true; // inert info row
    if (result.kind !== "marketplace" || !result.market) return false;
    const m = result.market;
    if (installing.has(m.name)) return true;
    if (m.dev_conflict) {
      ctx.pushToast(`${m.name} is dev-linked — unlink it before installing`, "info");
      return true;
    }
    if (m.state === "installed") {
      // Already installed — Enter jumps to its card in Settings.
      invoke("open_settings_window", { section: `extensions:${m.name}` })
        .catch(e => ctx.pushToast(String(e), "error"));
      return true;
    }
    if (m.state === "incompatible") {
      ctx.pushToast(`${m.name} requires a newer Portunus`, "info");
      return true;
    }
    startInstall(m, ctx);
    return true;
  },

  actions: (result, ctx) => {
    if (result.kind !== "marketplace" || !result.market) return [];
    const m = result.market;
    const out = [];
    if (!m.dev_conflict && (m.state === "not_installed" || m.state === "update")) {
      out.push({
        id: "market:install",
        title: m.state === "update" ? `Update to v${m.version}` : "Install",
        section: "result" as const,
        shortcut: { key: "enter" },
        displayOnly: true, // plain Enter already routes through handleLaunch
        run: () => startInstall(m, ctx),
      });
    }
    if (m.state === "installed" || m.state === "update") {
      out.push({
        id: "market:settings",
        title: "Open in Settings",
        section: "result" as const,
        // Opaque `extensions:<name>` section string: Settings selects the
        // Extensions pane and pre-filters to this card. Backend just forwards
        // the string, so no Rust change.
        run: () => {
          invoke("open_settings_window", { section: `extensions:${m.name}` })
            .catch(e => ctx.pushToast(String(e), "error"));
        },
      });
    }
    if (!m.dev_conflict && (m.state === "installed" || m.state === "update")) {
      out.push({
        id: "market:uninstall",
        title: "Uninstall",
        section: "result" as const,
        run: () => {
          invoke("uninstall_extension", { name: m.name })
            .then(() => {
              ctx.pushToast(`${m.name} uninstalled`, "success");
              ctx.requery();
            })
            .catch(e => ctx.pushToast(String(e), "error"));
        },
      });
    }
    return out;
  },
});

// ── preview panel ─────────────────────────────────────────────────────────────

const STATE_BADGE: Record<string, { label: string; tone: string }> = {
  installed: { label: "Installed", tone: "ok" },
  update: { label: "Update available", tone: "update" },
  incompatible: { label: "Needs newer Portunus", tone: "warn" },
};

interface Pill {
  icon: React.ReactNode;
  label: string;
}

/** Benign grants shown as glyphed pills. Sandbox-relaxing grants (any-host
 *  network, spawn, bus) are deliberately excluded — they get the loud callout
 *  below, never a quiet pill. */
function permissionPills(m: MarketplaceResult): Pill[] {
  const p = m.permissions;
  const pills: Pill[] = [];
  for (const host of p.network.filter(h => h !== "*")) {
    pills.push({ icon: <NetIcon />, label: host });
  }
  if (p.kv) pills.push({ icon: <StoreIcon />, label: "Storage" });
  if (p.open_url) pills.push({ icon: <LinkIcon />, label: "Open links" });
  if (p.clipboard) pills.push({ icon: <ClipIcon />, label: "Clipboard" });
  if (p.paste) pills.push({ icon: <PasteIcon />, label: "Paste" });
  if (p.has_secrets) pills.push({ icon: <KeyIcon />, label: "Keyring" });
  return pills;
}

/** Rounded icon tile from the index entry's inline icon, with a store-glyph
 *  fallback when an extension ships none. */
function IconTile({ src }: { src?: string | null }) {
  if (src) return <img className="market-icon" src={src} alt="" draggable={false} />;
  return (
    <div className="market-icon market-icon-fallback">
      <StoreIcon />
    </div>
  );
}

const WarnGlyph = () => (
  <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
    <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
    <line x1="12" y1="9" x2="12" y2="13" />
    <line x1="12" y1="17" x2="12.01" y2="17" />
  </svg>
);

/** One sandbox-relaxing grant, rendered as the loud callout — icon rail, bold
 *  title, muted body. Armed state (pending confirm-Enter) shifts the accent. */
function DangerBox({ armed, title, children }: { armed: boolean; title: string; children: React.ReactNode }) {
  return (
    <div className={`market-danger${armed ? " armed" : ""}`}>
      <span className="market-danger-ico"><WarnGlyph /></span>
      <div className="market-danger-body">
        <span className="market-danger-title">{title}</span>
        <span className="market-danger-text">{children}</span>
      </div>
    </div>
  );
}

function MarketplacePreview({ result }: PreviewProps) {
  useMarketStamp();
  if (result.kind === "marketplace-msg") {
    return (
      <div className="market-preview">
        <div className="market-empty">
          <div className="market-empty-title">{result.title}</div>
          <div className="market-empty-desc">{result.subtitle}</div>
        </div>
      </div>
    );
  }
  const m = result.market;
  if (!m) return null;
  return <EntryView m={m} icon={result.icon_data_uri} />;
}

function EntryView({ m, icon }: { m: MarketplaceResult; icon?: string | null }) {
  // Disarm when the selection leaves this row or the marketplace scope, so a
  // stale arm can't turn a later single Enter into an unconfirmed install.
  useEffect(
    () => () => {
      if (armed === m.name) {
        armed = null;
        bump();
      }
    },
    [m.name],
  );

  const busy = installing.has(m.name);
  const isArmed = armed === m.name;
  const badge = STATE_BADGE[m.state];
  const pills = permissionPills(m);
  const spawn = m.permissions.spawn;
  const netAny = m.permissions.network.includes("*");
  const bus = m.permissions.bus;
  const interpreters = interpretersIn(spawn);
  const hasDanger = spawn.length > 0 || netAny || bus;

  let hint: string | null;
  if (busy) hint = null;
  else if (m.dev_conflict) hint = "Dev-linked — unlink to install from the marketplace";
  else if (isArmed) hint = "Press Enter again to confirm";
  else if (m.state === "not_installed") hint = "↵ Install";
  else if (m.state === "update") hint = "↵ Update";
  else if (m.state === "installed") hint = "↵ Open in Settings";
  else hint = null;

  return (
    <div className="market-preview">
      <div className="market-header">
        <IconTile src={icon} />
        <div className="market-head-body">
          <div className="market-title-row">
            <span className="market-name">{m.name}</span>
            {m.state === "update" ? (
              <span className="market-version">
                v{m.installed_version} <span className="market-version-arrow">→</span> v{m.version}
              </span>
            ) : (
              <span className="market-version">v{m.version}</span>
            )}
            {badge && <span className={`market-badge ${badge.tone}`}>{badge.label}</span>}
          </div>
          {m.description && <div className="market-desc">{m.description}</div>}
          <div className="market-meta-row">
            <span>Marketplace</span>
            {m.homepage && <span className="market-meta-home">{prettyHost(m.homepage)}</span>}
            {m.size_bytes > 0 && <span>{formatBytes(m.size_bytes)}</span>}
            {m.author && <span>by {m.author}</span>}
          </div>
        </div>
      </div>

      <div className="market-perms">
        <div className="market-perms-label">
          {m.state === "update" && m.permissions_grew
            ? "Permissions — this update requests new ones"
            : "Permissions"}
        </div>
        {pills.length === 0 && !hasDanger ? (
          <div className="market-perm-none">No permissions — runs fully sandboxed</div>
        ) : (
          <>
            {pills.length > 0 && (
              <div className="market-pills">
                {pills.map(p => (
                  <span className="market-pill" key={p.label}>
                    <span className="market-pill-ico">{p.icon}</span>
                    {p.label}
                  </span>
                ))}
              </div>
            )}
            {netAny && (
              <DangerBox armed={isArmed} title="Reaches any host on the network">
                Connects to any server, not a fixed allowlist.
              </DangerBox>
            )}
            {spawn.length > 0 && (
              <DangerBox armed={isArmed} title="Runs programs outside the sandbox">
                <span className="market-danger-cmds">{spawn.join(", ")}</span>
                {interpreters.length > 0 && (
                  <>
                    {" — "}
                    {interpreters.join(", ")}{" "}
                    {interpreters.length === 1
                      ? "is a command interpreter and can"
                      : "are command interpreters and can"}{" "}
                    run <strong>any</strong> program, granting effectively unrestricted access.
                  </>
                )}
              </DangerBox>
            )}
            {bus && (
              <DangerBox armed={isArmed} title="Talks to a companion app">
                Exchanges messages with a separately-installed program outside the sandbox.
              </DangerBox>
            )}
          </>
        )}
      </div>

      {m.keywords.length > 0 && (
        <div className="market-keywords">
          {m.keywords.map(k => <code key={k}>{k}</code>)}
        </div>
      )}

      {busy ? (
        <InstallProgress name={m.name} />
      ) : (
        <div className="market-footer">
          {hint && <span className={`market-hint${isArmed ? " armed" : ""}`}>{hint}</span>}
        </div>
      )}
    </div>
  );
}

/** Bare host for the meta line — "github.com" from "https://github.com/…". */
function prettyHost(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url.replace(/^https?:\/\//, "").replace(/\/.*$/, "");
  }
}

function InstallProgress({ name }: { name: string }) {
  // Re-render on every throttled progress event while this install runs.
  const [progress, setProgress] = useState<{ fetched: number; total: number | null } | null>(null);
  useTauriListener<{ fetched: number; total: number | null }>(
    "ext-install-progress",
    p => setProgress(p),
  );
  const pct =
    progress && progress.total ? Math.min(100, (progress.fetched / progress.total) * 100) : null;
  return (
    <div className="market-progress">
      <div className="market-progress-label">
        Installing {name}
        {progress && (
          <span>
            {" "}
            — {formatBytes(progress.fetched)}
            {progress.total ? ` / ${formatBytes(progress.total)}` : ""}
          </span>
        )}
      </div>
      <div className="market-progress-bar">
        <div
          className={`market-progress-fill${pct === null ? " indeterminate" : ""}`}
          style={pct !== null ? { width: `${pct}%` } : undefined}
        />
      </div>
    </div>
  );
}

const STYLES = `
.market-preview {
  padding: 18px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
.market-preview::-webkit-scrollbar { width: 4px; }
.market-preview::-webkit-scrollbar-thumb { background: var(--bg-input); border-radius: 2px; }
.market-preview::-webkit-scrollbar-track { background: transparent; }

.market-header {
  display: flex;
  gap: 13px;
  align-items: flex-start;
}
.market-icon {
  width: 46px;
  height: 46px;
  border-radius: 12px;
  flex-shrink: 0;
  object-fit: cover;
  background: var(--bg-input);
}
.market-icon-fallback {
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--fg-mute);
}
.market-icon-fallback svg { width: 22px; height: 22px; }
.market-head-body {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 5px;
}
.market-title-row {
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
  flex-wrap: wrap;
}
.market-name {
  font-size: 20px;
  font-weight: 700;
  color: var(--fg);
  letter-spacing: -0.02em;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.market-version {
  font: 400 11px/1 var(--font-mono);
  color: var(--fg-dim);
  white-space: nowrap;
}
.market-version-arrow { color: var(--accent); }
.market-meta-row {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 5px 8px;
  font-size: 11px;
  color: var(--fg-mute);
}
.market-meta-row > span:not(:last-child)::after {
  content: "·";
  margin-left: 8px;
  color: var(--fg-dim);
}
.market-meta-home { color: var(--accent); }
.market-badge {
  font-size: 9.5px;
  font-weight: 600;
  letter-spacing: 0.03em;
  text-transform: uppercase;
  padding: 2px 7px;
  border-radius: 99px;
  background: var(--accent-soft);
  color: var(--accent);
  align-self: center;
}
.market-badge.warn {
  background: var(--danger-bg-dim);
  color: var(--danger-fg);
}

.market-desc {
  font-size: 12.5px;
  line-height: 1.5;
  color: var(--fg-desc);
}

.market-perms {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.market-perms-label {
  font-size: 10px;
  text-transform: uppercase;
  letter-spacing: 0.07em;
  color: var(--fg-mute);
}
.market-perm-none {
  font-size: 12px;
  color: var(--fg-dim);
}
.market-pills {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.market-pill {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 3px 9px 3px 7px;
  border-radius: 99px;
  border: 1px solid var(--line);
  background: var(--bg-card);
  font-size: 11.5px;
  color: var(--fg-desc);
  max-width: 100%;
}
.market-pill-ico {
  display: inline-flex;
  color: var(--fg-mute);
}
.market-pill-ico svg { width: 13px; height: 13px; }

.market-danger {
  display: flex;
  gap: 9px;
  padding: 9px 11px;
  border-radius: 8px;
  border: 1px solid var(--danger-bg);
  background: var(--danger-bg-dim);
}
.market-danger.armed { border-color: var(--accent); }
.market-danger-ico {
  display: inline-flex;
  flex-shrink: 0;
  margin-top: 1px;
  color: var(--danger-fg);
}
.market-danger-body {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 0;
}
.market-danger-title {
  font-size: 12px;
  font-weight: 600;
  color: var(--danger-fg);
}
.market-danger-text {
  font-size: 11.5px;
  line-height: 1.45;
  color: var(--danger-fg);
  opacity: 0.82;
  word-break: break-word;
}
.market-danger-text strong { opacity: 1; font-weight: 600; }
.market-danger-cmds {
  font: 400 11px/1.4 var(--font-mono);
}

.market-keywords {
  display: flex;
  flex-wrap: wrap;
  gap: 5px;
}
.market-keywords code {
  font: 400 10.5px/1 var(--font-mono);
  padding: 3px 6px;
  border-radius: 5px;
  background: var(--bg-input);
  color: var(--fg-mute);
}

.market-footer {
  margin-top: auto;
  padding-top: 10px;
  display: flex;
  align-items: baseline;
  gap: 12px;
  border-top: 1px solid var(--line);
}
.market-hint {
  font-size: 11.5px;
  font-weight: 600;
  color: var(--accent);
}
.market-hint.armed { color: var(--accent); }

.market-progress {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding-top: 8px;
}
.market-progress-label { font-size: 11px; color: var(--fg-mute); }
.market-progress-bar {
  height: 3px;
  border-radius: 2px;
  background: var(--bg-input);
  overflow: hidden;
}
.market-progress-fill {
  height: 100%;
  background: var(--accent);
  border-radius: 2px;
  transition: width 0.15s ease;
}
.market-progress-fill.indeterminate {
  width: 40%;
  animation: market-indeterminate 1.1s ease-in-out infinite;
}
@keyframes market-indeterminate {
  0% { margin-left: -40%; }
  100% { margin-left: 100%; }
}

.market-empty {
  margin: auto;
  text-align: center;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.market-empty-title { font-size: 14px; font-weight: 600; color: var(--fg); }
.market-empty-desc { font-size: 12px; color: var(--fg-mute); }
`;

if (typeof document !== "undefined") {
  const id = "market-preview-styles";
  if (!document.getElementById(id)) {
    const el = document.createElement("style");
    el.id = id;
    el.textContent = STYLES;
    document.head.appendChild(el);
  }
}
