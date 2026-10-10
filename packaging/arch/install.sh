#!/usr/bin/env bash
# One-command install of portunus-fff on Arch Linux (and derivatives):
#
#   curl -fsSL https://raw.githubusercontent.com/selfxplanatorium/portunus-fff/master/packaging/arch/install.sh | bash
#
# Fetches the release's rendered PKGBUILD (portunus-fff-bin, the prebuilt .deb
# repackaged; checksum filled in by CI) and builds + installs it with makepkg,
# so pacman tracks every file and `pacman -R portunus-fff-bin` removes it.
#
# Options (pass after `bash -s --` when piping):
#   --version X.Y.Z   install that release instead of the latest
#   --optional        also install the optional deps (cliphist, wl-clipboard, wtype, dictd)
#   --autostart       start portunus on login (XDG autostart; KDE, GNOME, …)
#   --review          show the PKGBUILD and ask before building
#   --uninstall       remove portunus-fff-bin
#   -y, --yes         don't ask pacman for confirmation
set -euo pipefail

# Everything runs from main(), called on the last line: under `curl | bash`,
# bash reads the script from stdin as it goes, so the whole file must be parsed
# before stdin is pointed at the terminal for pacman's prompts.

REPO="selfxplanatorium/portunus-fff"
PKG="portunus-fff-bin"
OPTIONAL_DEPS=(cliphist wl-clipboard wtype dictd)

version="" optional=0 autostart=0 review=0 uninstall=0 noconfirm=()

msg()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}" 2>/dev/null || true; }

main() {
while [ $# -gt 0 ]; do
  case "$1" in
    --version)   [ $# -ge 2 ] || die "--version needs a value"; version="${2#v}"; shift ;;
    --optional)  optional=1 ;;
    --autostart) autostart=1 ;;
    --review)    review=1 ;;
    --uninstall) uninstall=1 ;;
    -y|--yes)    noconfirm=(--noconfirm) ;;
    -h|--help)   usage; exit 0 ;;
    *)           die "unknown option: $1" ;;
  esac
  shift
done

command -v pacman >/dev/null || die "pacman not found - this installer is for Arch Linux and derivatives"
[ "$(uname -m)" = x86_64 ] || die "only x86_64 packages are published (use the Nix flake or build from source)"
[ "$(id -u)" -ne 0 ] || die "run as your normal user, not root (makepkg refuses root; sudo is used where needed)"
command -v sudo >/dev/null || die "sudo is required"

# Piped through `curl | bash`, stdin is the script: give prompts the terminal.
if [ ! -t 0 ] && { : </dev/tty; } 2>/dev/null; then exec </dev/tty; fi

if [ "$uninstall" = 1 ]; then
  if pacman -Qq "$PKG" >/dev/null 2>&1; then
    msg "Removing $PKG"
    sudo pacman -R "${noconfirm[@]}" "$PKG"
    rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/portunus.desktop"
    msg "Removed. Your config in ~/.config/portunus was left in place."
  else
    msg "$PKG is not installed"
  fi
  exit 0
fi

# makepkg needs fakeroot/binutils (base-devel); the PKGBUILD extracts with ar + bsdtar.
missing=()
for p in base-devel curl libarchive; do
  pacman -Qq "$p" >/dev/null 2>&1 || pacman -Qg "$p" >/dev/null 2>&1 || missing+=("$p")
done
if [ ${#missing[@]} -gt 0 ]; then
  msg "Installing build prerequisites: ${missing[*]}"
  sudo pacman -S --needed "${noconfirm[@]}" "${missing[@]}"
fi

if [ -n "$version" ]; then
  url="https://github.com/$REPO/releases/download/v$version/PKGBUILD"
else
  url="https://github.com/$REPO/releases/latest/download/PKGBUILD"
fi

work=$(mktemp -d -t portunus-install.XXXXXX)
trap 'rm -rf "$work"' EXIT

msg "Fetching PKGBUILD${version:+ for v$version}"
curl -fsSL "$url" -o "$work/PKGBUILD" ||
  die "could not download $url (no published release${version:+ v$version} with a PKGBUILD? see https://github.com/$REPO/releases)"
grep -q "^pkgname=$PKG\$" "$work/PKGBUILD" || die "downloaded file is not the $PKG PKGBUILD"
grep -q '@SHA256@\|@PKGVER@' "$work/PKGBUILD" && die "PKGBUILD is an unrendered template"

newver=$(sed -n 's/^pkgver=//p' "$work/PKGBUILD")
if have=$(pacman -Q "$PKG" 2>/dev/null); then
  msg "Installed: $have -> installing $newver"
else
  msg "Installing $PKG $newver"
fi

if [ "$review" = 1 ]; then
  ${PAGER:-less} "$work/PKGBUILD" || cat "$work/PKGBUILD"
  read -r -p "Build and install? [Y/n] " ans
  case "$ans" in [nN]*) die "aborted" ;; esac
fi

( cd "$work" && makepkg -si --needed "${noconfirm[@]}" )

if [ "$optional" = 1 ]; then
  msg "Installing optional dependencies: ${OPTIONAL_DEPS[*]}"
  sudo pacman -S --needed "${noconfirm[@]}" "${OPTIONAL_DEPS[@]}" || \
    warn "some optional dependencies could not be installed"
fi

if [ "$autostart" = 1 ]; then
  dir="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
  mkdir -p "$dir"
  cat > "$dir/portunus.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Portunus
Comment=Application launcher (starts hidden)
Exec=portunus
X-GNOME-Autostart-enabled=true
NoDisplay=true
DESKTOP
  msg "Autostart enabled: $dir/portunus.desktop"
  msg "(Hyprland/sway ignore XDG autostart: add 'exec-once = portunus' / 'exec portunus' instead.)"
fi

if pgrep -x portunus >/dev/null; then
  msg "Done. Restart the running portunus to use the new version."
elif [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
  (nohup portunus >/dev/null 2>&1 &)
  msg "Done. Portunus is running (hidden until toggled)."
else
  msg "Done. Start it from your session with:  portunus &"
fi
msg "Bind a key to:  portunus --toggle"
}

main "$@"
