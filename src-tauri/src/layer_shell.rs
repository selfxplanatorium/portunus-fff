//! wlr-layer-shell integration (Wayland only).
//!
//! Promotes the main launcher window to a real layer-shell *overlay* surface via
//! `libgtk-layer-shell`, so it sits above everything with no compositor-specific
//! window rules — the correct behavior for a launcher (wofi/rofi-wayland do the same).
//!
//! Tauri 2 on Linux uses GTK3, so this uses the GTK3 `gtk-layer-shell` crate.
//! `init_layer_shell()` must run before the window is mapped; tao may have already
//! mapped it, so we `hide()` (unmap) first. See tao issue #925.

/// Apply layer-shell properties to the main window. Call once, in setup, before
/// the window is shown. No-op off Linux and off Wayland.
#[cfg(target_os = "linux")]
pub fn apply(window: &tauri::WebviewWindow) {
    use gtk::prelude::WidgetExt;
    use gtk_layer_shell::{KeyboardMode, Layer, LayerShell};

    // gtk-layer-shell only works under Wayland; skip on X11.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }

    // GNOME/Mutter and other non-wlroots compositors don't implement
    // zwlr_layer_shell_v1. Without this probe gtk-layer-shell falls back to an
    // XDG surface and every property setter below hits a g_critical.
    if !gtk_layer_shell::is_supported() {
        eprintln!("[portunus] layer_shell: compositor lacks zwlr_layer_shell_v1 - using XDG window");
        return;
    }

    let gtk_win = match window.gtk_window() {
        Ok(w) => w,
        Err(_) => return,
    };

    // init_layer_shell asserts the window is not yet mapped. tao may have mapped
    // it during an early redraw even though it's configured `visible: false`, so
    // unmap it first.
    gtk_win.hide();

    gtk_win.init_layer_shell();
    gtk_win.set_layer(Layer::Overlay);
    gtk_win.set_keyboard_mode(KeyboardMode::Exclusive);
    gtk_win.set_namespace("portunus");
    // No anchors set -> the compositor centers the surface, matching `center: true`.
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_window: &tauri::WebviewWindow) {}

/// Ask the compositor to blur what is behind the launcher's translucent
/// surfaces (`[appearance] blur`). A webview cannot blur the desktop itself -
/// that is compositor work - so this only acts where the compositor can be told
/// at runtime: Hyprland, through `hyprctl keyword` layer rules on the
/// `portunus` layer-shell namespace (layer_shell must be on). Elsewhere the
/// Appearance settings show the rule to add by hand.
///
/// `ignore_alpha 0` keeps the fully transparent margin around the rounded card
/// from being blurred as a rectangle. Hyprland 0.53 rewrote the rule syntax, so
/// the newer form is tried first and the legacy form is the fallback; each is
/// accepted only if hyprctl answers `ok`.
pub fn apply_compositor_blur(enabled: bool) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let state = if enabled { "on" } else { "off" };
    let modern = [
        format!("blur {state},match:namespace ^portunus$"),
        "ignore_alpha 0,match:namespace ^portunus$".to_string(),
    ];
    let legacy: &[String] = if enabled {
        &["blur,^portunus$".to_string(), "ignorealpha 0,^portunus$".to_string()]
    } else {
        // Pre-0.53 rules cannot be withdrawn at runtime.
        &[]
    };
    for rules in [&modern[..], legacy] {
        if !rules.is_empty() && rules.iter().all(|r| hyprctl_layerrule(r)) {
            return;
        }
    }
    eprintln!(
        "[portunus] blur: hyprctl did not accept the layer rule{}",
        if enabled { "" } else { "; reload Hyprland to drop the old one" }
    );
}

fn hyprctl_layerrule(rule: &str) -> bool {
    std::process::Command::new("hyprctl")
        .args(["keyword", "layerrule", rule])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok")
}
