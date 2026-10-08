<div align="center">

<img src="portunus-icon.svg" alt="portunus-fff" width="120" />

# portunus-fff

**A keyboard-first application launcher and search tool for Wayland, with whole-disk file search powered by [fff](https://github.com/dmtrKovalenko/fff).**

A fork of [SzilBalazs/portunus](https://github.com/SzilBalazs/portunus).

Find and launch apps, jump to files, do quick math, look up a word, dig through
your clipboard history, or search the text inside your documents. One box, no mouse.

[![Upstream](https://img.shields.io/badge/fork%20of-SzilBalazs%2Fportunus-555?style=flat-square)](https://github.com/SzilBalazs/portunus)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square)](LICENSE.txt)
[![Wayland](https://img.shields.io/badge/Wayland-native-1793D1?style=flat-square)](#compositor-setup)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%20%2B%20Rust-FFC131?style=flat-square)](https://tauri.app)

[What's different](#whats-different-in-this-fork) · [Install](#install) · [Usage](#usage) · [Configuration](#configuration) · [Themes](#themes) · [Extensions](#extensions) · [Building](#building-from-source)

<img src=".github/assets/hero.png" alt="portunus-fff launcher" width="720" />

</div>

It stays out of your way. The window is hidden until you hit your keybind, and it
vanishes again the second you launch something or press Escape.

## What's different in this fork

portunus-fff tracks upstream Portunus and changes three things: the file search
engine, the appearance options, and a handful of UI fixes. The binary, CLI and
config file are unchanged (`portunus`, `~/.config/portunus/config.toml`), so it
is a drop-in replacement.

### File search on fff

The upstream file provider walked a few configured folders two levels deep and
fuzzy-matched file names. This fork replaces it with
[fff](https://github.com/dmtrKovalenko/fff), a file-search engine built for
large trees:

- **Whole disk by default.** The default config indexes `~` (kept live) and
  `/` (every top-level directory except `/proc`, `/sys`, `/dev`, `/run`,
  `/tmp`), crawled to full depth. There is no depth limit to tune.
- **Typo tolerant.** `meetng notes` finds `meeting-notes.md`; `invocie` finds
  `invoice.pdf`. Matching scores the whole path, with a bonus when the hit lands
  in the file name.
- **Live index.** Watched folders stay current through per-directory inotify
  watches instead of re-walking a subtree on every change. Huge, rarely-changing
  roots can opt out (`watch = false`) and refresh on startup, on config change
  and on `portunus --reindex`.
- **Smarter skipping.** The walk honors `.gitignore`/`.ignore` files and skips
  common build and cache trees (`node_modules`, virtualenvs, `target/debug`, …).

Measured on a synthetic 1.5M-file tree against the upstream engine with its
depth limit removed (release builds, 4-core machine):

| | upstream engine | fff |
|---|---|---|
| Memory | ~490 MB | ~275–460 MB |
| Building the index | 15 s | 5–9 s |
| Typo tolerance | no | yes |
| Per keystroke | 100–155 ms | 50–280 ms |

At this scale fff is lighter and indexes faster, but per-keystroke search is
slower on most queries, because it scores full paths with typo tolerance
instead of bare file names. On a typical home folder (tens of thousands of
files), both answer in roughly 10 ms or less.

### Appearance

New options under **Settings → Appearance**:

- **Opacity**: fade the launcher's background panels (30%–solid). Text, icons
  and the selection stay solid.
- **Blur behind**: on Hyprland the blur rule is applied at runtime via
  `hyprctl`; for other compositors, Settings shows the rule to add yourself.
- **Fonts**: pick the interface and monospace fonts from your installed fonts.

### Motion

The launcher opens as a bare search bar and grows into the results window as
you type, then folds back when you clear the query. The bar drops in on every
show, with the search icon and text trailing a few frames behind; results,
preview and footer fade in after the card starts to grow and leave before it
shrinks. **Settings → Appearance → Window animations** turns it all off, and
reduced-motion system settings are respected.

### Cleaner UI

- No colored accent bar on selected and hovered rows.
- No product name and version in the launcher footer; just the settings icon.
- Key hints are optically centered for any font, so `ctrl`/`shift`/`alt` line up
  with the `↵` keys.
- Accent-colored buttons (e.g. **Open**) no longer render transparent when the
  first result you select is a file.

## Features

- 🔍 **Fuzzy app & file search**: apps (`.desktop` entries) plus every file and folder on your disk, indexed and kept live by [fff](https://github.com/dmtrKovalenko/fff) (typo-tolerant), ranked by how often you actually open them
- 🧮 **Inline calculator**: math (`log2(10^8)`), unit conversion (`5km to mi`), currency (`100 usd to eur`), date math (`days until dec 25`), and timezones (`3pm est in cet`)
- 📖 **Dictionary lookup**: `define serendipity`, `dict serendipity`, or `dictionary serendipity` (needs the `dict` client; queries dict.org unless you run a local `dictd`)
- 📋 **Clipboard history**: full-text search back through your `cliphist` entries (Wayland)
- 📄 **Content search**: hit `Tab` to search the text inside PDFs, office docs, and images. OCR handles scanned PDFs and screenshots too
- 👁 **Preview panel**: images, PDFs, text files, folder listings, clipboard contents
- 🧩 **Extensions**: sandboxed wasm modules add new search providers, commands, and previews; browse and install them by typing `marketplace` in the launcher
- ⚡ **No spinners**: the Rust backend indexes on a background thread, so results show up as you type

<table>
  <tr>
    <td width="50%"><img src=".github/assets/dict.png" alt="Dictionary lookup" /><br/><sub><b>Dictionary lookup</b></sub></td>
    <td width="50%"><img src=".github/assets/content.png" alt="Content search" /><br/><sub><b>Content search (Tab)</b></sub></td>
  </tr>
  <tr>
    <td width="50%"><img src=".github/assets/preview.png" alt="Preview panel" /><br/><sub><b>Preview panel</b></sub></td>
    <td width="50%"><img src=".github/assets/clipboard.png" alt="Clipboard history" /><br/><sub><b>Clipboard history</b></sub></td>
  </tr>
</table>

## Install

Download a package from this fork's [Releases page](https://github.com/selfxplanatorium/portunus/releases)
(the upstream releases do not include the fff engine). All of them are **x86_64 only**.
On other architectures use the Nix flake or build from source.

### Arch Linux

The release ships a ready-to-build `PKGBUILD` for `portunus-bin`, which installs
the prebuilt `.deb` above. Its `sha256sum` is filled in by CI at release time, so
there is nothing to edit:

```bash
curl -fLO https://github.com/selfxplanatorium/portunus/releases/latest/download/PKGBUILD
makepkg -si
```

Read the `PKGBUILD` before building, as you would for anything from the AUR.

### Debian / Ubuntu (`.deb`)

```bash
sudo apt install ./portunus_*_amd64.deb
```

The `.deb` links your system's own WebKitGTK, so the launcher never carries a
stale bundled WebView. This is the lowest-latency option. It still bundles
libpdfium, the poppler tools, and the English tesseract data under
`/usr/lib/portunus`, so PDF preview, content search, and OCR need nothing extra.

### AppImage (portable)

```bash
chmod +x portunus_*_amd64.AppImage
./portunus_*_amd64.AppImage
```

Self-contained and runs anywhere, but it is built on Ubuntu 24.04, so it needs
**glibc 2.39 or newer** (Ubuntu 24.04+, Debian 13+, Arch, Fedora 40+). On older
distros the AppImage will not start; install the `.deb` instead.

<details>
<summary><b>Optional runtime dependencies</b></summary>

<br/>

Every package above bundles what PDF preview, content search, and OCR need
(libpdfium, the poppler tools, and the English tesseract data), so those work
with no extra setup. Two features rely on system tools that are not bundled:

| Package | Feature | Arch | Ubuntu/Debian |
|---|---|---|---|
| `cliphist` + `wl-clipboard` | Clipboard history | `sudo pacman -S cliphist wl-clipboard` | `sudo apt install cliphist wl-clipboard` |
| `wtype` | Smart paste (auto Ctrl+V) | `sudo pacman -S wtype` | `sudo apt install wtype` |
| `dict` client | Dictionary definitions | `sudo pacman -S dictd` | `sudo apt install dict` |

Portunus shells out to the `dict` **client**, not to a server. Debian and Ubuntu
split the two, so install `dict` there; `dictd` is the server and does not ship
the `dict` binary at all. Arch's `dictd` package contains both. Either way you
get a `dict.conf` that tries `localhost` first and falls back to `dict.org`, so
definitions work immediately but go over the network. For offline lookups, run a
local `dictd` carrying the WordNet (`wn`) database.

**Settings → Providers** lists every optional dependency and whether Portunus can
currently find it; the first-launch wizard shows the same check.

If you build from source instead of installing a package, you also need the PDF and
OCR tools on your system: `poppler` (or `poppler-utils`), a `pdfium`
build such as `pdfium-bin`, and tesseract with the language data you want
(`tesseract` + `tesseract-data-eng`).

</details>

### Nix (flake)

```bash
nix run github:selfxplanatorium/portunus
```

The `portunus.cachix.org` cache below holds upstream builds, not this fork's, so
expect Nix to build portunus-fff from source. Nix applies a flake's own
substituter list only for trusted users, so unless you are one, add the cache to
your configuration. Without it Nix quietly builds the whole app from source:

```nix
# NixOS: configuration.nix
nix.settings = {
  substituters = [ "https://portunus.cachix.org" ];
  trusted-public-keys = [
    "portunus.cachix.org-1:byhkNv2iSgx4QQmrwgmtzYHFY+ztYe8+3vcAStcDemI="
  ];
};
```

For a one-off run, `--accept-flake-config` does the same job:

```bash
nix run --accept-flake-config github:selfxplanatorium/portunus
```

The wrapper puts libpdfium, the poppler tools, cliphist, wl-clipboard, wtype, the
`dict` client and the tesseract data on the package's own path, so PDF preview,
clipboard history, content search and OCR work out of the box.

Dictionary lookups need one extra step. The `dict` client reads its server list
from `/etc/dict.conf`, which a plain `nix run` never creates, and it has no
built-in default. With no config it exits with `'dict.conf' doesn't specify any
dict server`. Point it at the public server:

```bash
echo 'server dict.org' >> ~/.dictrc
```

On NixOS you can get fully offline lookups instead by running the server locally.
WordNet is already in the default database set:

```nix
services.dictd.enable = true;
```

## Compositor setup

Portunus runs hidden at startup. Bind `portunus --toggle` to a key to reveal it; press it again (or launch/Escape) to hide it.

> [!WARNING]
> Clipboard features need Wayland.

### Hyprland

```conf
# ~/.config/hypr/hyprland.conf
exec-once = /path/to/portunus

bind = CTRL, SPACE, exec, /path/to/portunus --toggle
bind = SUPER, V, exec, /path/to/portunus --clipboard
```

## Usage

| Query | Result |
|---|---|
| `firefox` | Fuzzy-match apps and files |
| `define serendipity` | Dictionary definition |
| `log2(10^8)` | Calculator |
| `5km to mi`, `100 usd to eur` | Unit & currency conversion |
| `now + 3 weeks`, `time in tokyo` | Date math & timezones |
| `clipboard search term` | Browse clipboard history |
| `Tab` then `invoice 2024` | Search file contents |

## Configuration

On first launch Portunus writes a default config to `~/.config/portunus/config.toml`. Config changes are hot-reloaded without a restart.

### Themes

Pick a theme in **Settings → Appearance**. Eight dark themes ship built-in, plus a Matugen theme that pulls its colors from your wallpaper.

#### Matugen (Material You from your wallpaper)

The **Matugen** theme pulls its colors from an external file, so [matugen](https://github.com/InioX/matugen) can recolor Portunus to match your wallpaper. Copy [`templates/portunus.css`](templates/portunus.css) into your matugen config and wire it up:

```toml
# ~/.config/matugen/config.toml
[templates.portunus]
input_path  = "~/.config/matugen/portunus.css"   # copy of templates/portunus.css
output_path = "~/.config/portunus/matugen.css"
post_hook   = "portunus --reload-theme"
```

Run `matugen image <wallpaper>` (add `--mode light` for a light scheme), then select **Matugen** in Settings → Appearance. Every subsequent matugen run recolors the launcher live via the `post_hook`. If `~/.config/portunus/matugen.css` is missing, the theme falls back to default colors.

## Extensions

Portunus can be extended with sandboxed WebAssembly **extensions** that add
new search providers, launcher commands, previews, and background refreshers.
Type `marketplace` in the launcher to browse and install them. Each install
shows the permissions it asks for before you confirm. **Settings → Extensions**
manages what you already have: update checks, sideloading a local `.portext`, and
rescans.

Reference extensions and the marketplace index live in a dedicated repo:
**[SzilBalazs/portunus-extensions](https://github.com/SzilBalazs/portunus-extensions)**.
To write your own, see [EXTENSIONS.md](EXTENSIONS.md) and run `portunus ext new`.

## Building from source

<details>
<summary><b>Dependencies</b></summary>

<br/>

| Dependency | Notes |
|---|---|
| Rust stable | via `rustup` |
| Bun | package manager + JS runtime |
| `libwebkit2gtk-4.1-dev` | Tauri WebView |
| `libssl-dev` | |
| `libtesseract-dev` + `libleptonica-dev` | OCR is always built in, so these are required |

</details>

```bash
# Build
bun tauri build

# Type-check only
cargo check --manifest-path src-tauri/Cargo.toml
bun x tsc --noEmit
```

With Nix, the repo's flake provides all of the above: `nix develop` enters a
dev shell with the full toolchain, and `nix build` produces the package in
`result/` (see `flake.nix` and `packaging/nix/`).

## CLI flags

```
portunus [FLAG]

  --show              Show the launcher window (signals a running instance)
  --close             Close the launcher window (signals a running instance)
  --toggle            Toggle the launcher window (signals a running instance)
  --clipboard         Show the launcher pre-filled with "clipboard"
  --reindex           Rebuild the content search index
  --reload-config     Reload config from file without restarting
  --reload-extensions Re-discover and reload WASM extensions (picks up rebuilt wasm)
  --reload-theme      Re-read the external matugen.css theme (matugen post_hook)
  --version, -V       Print version and exit
  --help, -h          Show this help message
```

## License

[Apache-2.0](LICENSE.txt)
