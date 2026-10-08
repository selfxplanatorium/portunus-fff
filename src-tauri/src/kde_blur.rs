//! KWin blur behind the visible launcher card only (`[appearance] blur` on KDE
//! Plasma Wayland).
//!
//! The launcher window is a transparent 960x640 surface, and most of it is
//! empty: a whole-surface blur would frost the entire rectangle. The frontend
//! reports the card's rounded outline (`set_blur_region`) and this module
//! forwards it as a `wl_region` through whichever blur protocol KWin offers:
//!
//! - `ext_background_effect_manager_v1` (staging ext-background-effect-v1),
//!   preferred. KWin 6.7 effects such as Better Blur DX implement blur only
//!   through it, and with the stock Blur effect disabled KWin no longer
//!   advertises the Plasma protocol at all. Used only while the compositor
//!   reports the `blur` capability.
//! - `org_kde_kwin_blur_manager` (Plasma's blur.xml), the fallback for older
//!   Plasma and the stock Blur effect.
//!
//! Wire-up: GDK already owns the Wayland connection, so this borrows its
//! `wl_display` and the window's `wl_surface` (gdkwayland-sys) and wraps them
//! with wayland-client's libwayland backend (`client_system`, the only backend
//! that can adopt foreign objects). Everything lives on a private event queue,
//! so GDK's own dispatching never sees our objects, and every call runs on the
//! GTK main thread, where GDK is not reading the socket concurrently.
//!
//! Both protocols tie their per-surface object to a wl_surface that GDK
//! destroys on every hide, and ext-background-effect makes any request on an
//! object whose surface is gone a fatal protocol error (it would take GDK's
//! whole connection down). So the per-surface object is destroyed on GTK's
//! `unmap`, which runs before GDK tears the surface down, and recreated on the
//! next show.
//!
//! No-op unless the display is a GdkWaylandDisplay and one of the protocols is
//! advertised; Hyprland and others stop at the probe (see
//! `layer_shell::apply_compositor_blur` for Hyprland).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use gtk::glib::translate::{from_glib, ToGlibPtr};
use gtk::prelude::*;
use tauri::{AppHandle, Manager};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry, wl_surface::WlSurface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, Capability, ExtBackgroundEffectManagerV1,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

/// A rect in window-relative logical pixels (CSS px at zoom 1 == surface-local
/// wl coordinates; the buffer scale is applied by the compositor).
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct BlurRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

static APP: OnceLock<AppHandle> = OnceLock::new();
/// `[appearance] blur`. Off = the region is cleared (the per-surface object stays).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Latched when the display is not Wayland (X11): nothing here can ever work.
/// A Wayland compositor without a blur protocol is retried once per show
/// instead (`Wayland::probed_this_show`), since effects can be toggled live.
static NOT_WAYLAND: AtomicBool = AtomicBool::new(false);
/// One main-thread apply in flight at a time; it reads the latest rects.
static PENDING: AtomicBool = AtomicBool::new(false);
static RECTS: Mutex<Vec<BlurRect>> = Mutex::new(Vec::new());
/// Last probe outcome written to the log (0 = none yet), so the journal gets
/// one line per change rather than one per show.
static LOGGED_PROBE: AtomicU8 = AtomicU8::new(0);

thread_local! {
    // GTK main thread only.
    static WAYLAND: RefCell<Option<Wayland>> = const { RefCell::new(None) };
}

/// Remember the app handle; call once in setup, before any `set_*`.
pub fn init(app: &AppHandle) {
    let _ = APP.set(app.clone());
}

/// Apply or remove the blur for the last reported region.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    schedule();
}

/// New card outline from the frontend. Empty = blur nothing.
pub fn set_rects(rects: Vec<BlurRect>) {
    *crate::util::lock(&RECTS) = rects;
    schedule();
}

fn schedule() {
    if NOT_WAYLAND.load(Ordering::Relaxed) || PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(app) = APP.get() else {
        PENDING.store(false, Ordering::Release);
        return;
    };
    let handle = app.clone();
    if app.run_on_main_thread(move || apply(&handle)).is_err() {
        PENDING.store(false, Ordering::Release);
    }
}

fn apply(app: &AppHandle) {
    PENDING.store(false, Ordering::Release);
    let rects = if ENABLED.load(Ordering::Relaxed) {
        crate::util::lock(&RECTS).clone()
    } else {
        Vec::new()
    };
    let Some(window) = app.get_webview_window("main") else { return };
    let Ok(gtk_win) = window.gtk_window() else { return };

    let changed = WAYLAND.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            // Nothing to blur yet: don't connect (or bind globals) just to clear.
            if rects.is_empty() {
                return false;
            }
            let Some(wayland) = Wayland::connect(&gtk_win) else {
                NOT_WAYLAND.store(true, Ordering::Relaxed);
                log_probe(Probe::NotWayland);
                return false;
            };
            *cell = Some(wayland);
            // GDK destroys the wl_surface when the window unmaps; destroy the
            // per-surface object while that surface still exists, and allow a
            // fresh probe on the next show.
            gtk_win.connect_unmap(|_| {
                WAYLAND.with(|cell| {
                    if let Some(wayland) = cell.borrow_mut().as_mut() {
                        wayland.release_surface_blur();
                        wayland.probed_this_show = false;
                    }
                });
            });
        }
        let Some(wayland) = cell.as_mut() else { return false };
        if wayland.backend.is_none() {
            if rects.is_empty() || wayland.probed_this_show {
                return false;
            }
            wayland.probe();
        }
        wayland.set_region(&gtk_win, &rects)
    });

    // Blur state is double-buffered on the wl_surface: make sure a commit
    // follows, not whenever the webview next happens to paint.
    if changed {
        gtk_win.queue_draw();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    BackgroundEffect = 1,
    KwinBlur = 2,
    None = 3,
    NotWayland = 4,
}

fn log_probe(probe: Probe) {
    if LOGGED_PROBE.swap(probe as u8, Ordering::Relaxed) == probe as u8 {
        return;
    }
    let msg = match probe {
        Probe::BackgroundEffect => "using ext_background_effect_manager_v1",
        Probe::KwinBlur => "using org_kde_kwin_blur_manager",
        Probe::None => "no blur protocol advertised (ext_background_effect with blur capability, or org_kde_kwin_blur_manager); will retry on next show",
        Probe::NotWayland => "not a Wayland display; region blur disabled",
    };
    eprintln!("[portunus] blur: {msg}");
}

/// The bound blur protocol.
enum BlurManager {
    BackgroundEffect(ExtBackgroundEffectManagerV1),
    Kwin(OrgKdeKwinBlurManager),
}

/// The per-surface blur object of whichever protocol is bound.
enum SurfaceBlur {
    BackgroundEffect(ExtBackgroundEffectSurfaceV1),
    Kwin(OrgKdeKwinBlur),
}

struct Wayland {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    /// Kept for re-probing: tracks globals as registry events are dispatched.
    globals: GlobalList,
    compositor: WlCompositor,
    backend: Option<BlurManager>,
    /// A probe found nothing since the last unmap; next try is the next show.
    probed_this_show: bool,
    /// The per-surface object and the address of the wl_surface it belongs to.
    /// GDK swaps the wl_surface on every hide/show, so this is per surface.
    surface_blur: Option<(usize, SurfaceBlur)>,
}

impl Wayland {
    /// Adopt GDK's connection. None only when the display is not Wayland.
    fn connect(gtk_win: &gtk::ApplicationWindow) -> Option<Self> {
        let gdk_win = gtk_win.window()?;
        let display = gdk_win.display();
        // SAFETY: plain GType lookup / pointer read on a live GdkDisplay.
        let wayland_type: gtk::glib::Type =
            unsafe { from_glib(gdk_wayland_sys::gdk_wayland_display_get_type()) };
        if !display.type_().is_a(wayland_type) {
            return None;
        }
        let display_ptr: *mut gtk::gdk::ffi::GdkDisplay = display.to_glib_none().0;
        // SAFETY: display is a GdkWaylandDisplay (checked above).
        let wl_display = unsafe {
            gdk_wayland_sys::gdk_wayland_display_get_wl_display(display_ptr as *mut _)
        };
        if wl_display.is_null() {
            return None;
        }
        // SAFETY: GDK keeps this wl_display alive for the life of the process;
        // a foreign Backend never disconnects it.
        let backend = unsafe { Backend::from_foreign_display(wl_display as *mut _) };
        let conn = Connection::from_backend(backend);
        // Own queue + own wl_registry: GDK's queue never sees these objects.
        let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let compositor = globals.bind::<WlCompositor, _, _>(&qh, 1..=4, ()).ok()?;
        Some(Self {
            conn,
            queue,
            qh,
            state: State::default(),
            globals,
            compositor,
            backend: None,
            probed_this_show: false,
            surface_blur: None,
        })
    }

    /// Bind the best available blur protocol, if any.
    fn probe(&mut self) {
        self.probed_this_show = true;
        // Pick up globals announced since the last look (an effect enabled live).
        let _ = self.queue.roundtrip(&mut self.state);

        if let Ok(manager) = self.globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&self.qh, 1..=1, ()) {
            // `capabilities` is sent right after the bind.
            self.state.ext_blur_capable = None;
            let _ = self.queue.roundtrip(&mut self.state);
            if self.state.ext_blur_capable == Some(true) {
                self.backend = Some(BlurManager::BackgroundEffect(manager));
                log_probe(Probe::BackgroundEffect);
                return;
            }
            // Advertised without blur (no blur effect loaded): try Plasma's.
            manager.destroy();
        }
        if let Ok(manager) = self.globals.bind::<OrgKdeKwinBlurManager, _, _>(&self.qh, 1..=1, ()) {
            self.backend = Some(BlurManager::Kwin(manager));
            log_probe(Probe::KwinBlur);
            return;
        }
        log_probe(Probe::None);
    }

    /// Point the blur at `rects` on the window's current wl_surface. Returns
    /// whether anything was sent (and a surface commit is needed).
    fn set_region(&mut self, gtk_win: &gtk::ApplicationWindow, rects: &[BlurRect]) -> bool {
        if self.backend.is_none() {
            return false;
        }
        let Some(surface_ptr) = current_surface(gtk_win) else {
            self.release_surface_blur();
            return false;
        };
        if self.surface_blur.as_ref().map(|(ptr, _)| *ptr) != Some(surface_ptr as usize) {
            self.release_surface_blur();
            if rects.is_empty() {
                return false;
            }
            // SAFETY: a live wl_proxy owned by GDK; wrapped as a foreign id, we
            // only pass it as a request argument and never destroy it.
            let id = match unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr.cast()) } {
                Ok(id) => id,
                Err(_) => return false,
            };
            let Ok(surface) = WlSurface::from_id(&self.conn, id) else { return false };
            // Exactly one per surface: a second get_background_effect on the
            // same surface is a protocol error.
            let blur = match self.backend.as_ref() {
                Some(BlurManager::BackgroundEffect(m)) => {
                    SurfaceBlur::BackgroundEffect(m.get_background_effect(&surface, &self.qh, ()))
                }
                Some(BlurManager::Kwin(m)) => SurfaceBlur::Kwin(m.create(&surface, &self.qh, ())),
                None => return false,
            };
            self.surface_blur = Some((surface_ptr as usize, blur));
        }
        let Some((_, blur)) = self.surface_blur.as_ref() else { return false };

        let region = (!rects.is_empty()).then(|| {
            let region = self.compositor.create_region(&self.qh, ());
            for (x, y, w, h) in rects.iter().filter_map(to_wl_rect) {
                region.add(x, y, w, h);
            }
            region
        });
        // Both protocols copy the region, so it can be destroyed right away.
        match blur {
            // NULL removes the effect here.
            SurfaceBlur::BackgroundEffect(effect) => {
                effect.set_blur_region(region.as_ref());
                if let Some(region) = region {
                    region.destroy();
                }
            }
            // Here NULL would mean the whole surface, so clearing sends an
            // empty region instead; the request also needs its own commit.
            SurfaceBlur::Kwin(kwin) => {
                let region = region.unwrap_or_else(|| self.compositor.create_region(&self.qh, ()));
                kwin.set_region(Some(&region));
                kwin.commit();
                region.destroy();
            }
        }
        self.flush();
        true
    }

    fn release_surface_blur(&mut self) {
        if let Some((_, blur)) = self.surface_blur.take() {
            match blur {
                SurfaceBlur::BackgroundEffect(effect) => effect.destroy(),
                SurfaceBlur::Kwin(kwin) => kwin.release(),
            }
            self.flush();
        }
    }

    fn flush(&mut self) {
        let _ = self.conn.flush();
        // Drain our queue: capability changes and delete_id bookkeeping.
        let _ = self.queue.dispatch_pending(&mut self.state);
    }
}

/// The GDK window's current wl_surface, or None while unmapped.
fn current_surface(gtk_win: &gtk::ApplicationWindow) -> Option<*mut std::ffi::c_void> {
    let gdk_win = gtk_win.window()?;
    let ptr: *mut gtk::gdk::ffi::GdkWindow = gdk_win.to_glib_none().0;
    // SAFETY: only reached after the display was confirmed to be Wayland, so
    // this is a GdkWaylandWindow; NULL while it has no surface.
    let surface = unsafe { gdk_wayland_sys::gdk_wayland_window_get_wl_surface(ptr as *mut _) };
    (!surface.is_null()).then_some(surface as *mut std::ffi::c_void)
}

/// Logical rect -> integer surface rect. Edges are rounded independently so
/// rects that touch in CSS still touch (no hairline gaps between strips).
fn to_wl_rect(r: &BlurRect) -> Option<(i32, i32, i32, i32)> {
    if !(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite()) {
        return None;
    }
    let (x0, y0) = (r.x.round(), r.y.round());
    let (x1, y1) = ((r.x + r.w).round(), (r.y + r.h).round());
    let clamp = |v: f64| v.clamp(-32_768.0, 32_767.0) as i32;
    (x1 > x0 && y1 > y0).then(|| (clamp(x0), clamp(y0), clamp(x1 - x0), clamp(y1 - y0)))
}

#[derive(Default)]
struct State {
    /// Latest `capabilities` from ext_background_effect_manager_v1 (None until
    /// it arrives).
    ext_blur_capable: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // GlobalList tracks additions/removals itself; probe() reads it.
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            let blur = match flags {
                WEnum::Value(caps) => caps.contains(Capability::Blur),
                WEnum::Unknown(raw) => raw & Capability::Blur.bits() != 0,
            };
            // Losing the capability later means the compositor stops applying
            // the effect on its own; nothing to undo here.
            state.ext_blur_capable = Some(blur);
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ExtBackgroundEffectSurfaceV1);
delegate_noop!(State: OrgKdeKwinBlurManager);
delegate_noop!(State: OrgKdeKwinBlur);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_keeps_adjacent_strips_touching() {
        let a = BlurRect { x: 10.4, y: 0.0, w: 50.2, h: 3.3 };
        let b = BlurRect { x: 10.4, y: 3.3, w: 50.2, h: 3.3 };
        let (_, ay, _, ah) = to_wl_rect(&a).unwrap();
        let (_, by, _, _) = to_wl_rect(&b).unwrap();
        assert_eq!(ay + ah, by);
    }

    #[test]
    fn degenerate_and_non_finite_rects_are_dropped() {
        assert!(to_wl_rect(&BlurRect { x: 0.0, y: 0.0, w: 0.2, h: 10.0 }).is_none());
        assert!(to_wl_rect(&BlurRect { x: f64::NAN, y: 0.0, w: 5.0, h: 5.0 }).is_none());
    }
}
