//! KWin blur behind the visible launcher card only (`[appearance] blur` on KDE
//! Plasma Wayland).
//!
//! The launcher window is a transparent 960x640 surface, and most of it is
//! empty: KWin's default blur would frost the whole rectangle. Plasma's
//! `org_kde_kwin_blur` protocol lets a client name the exact region to blur, so
//! the frontend reports the card's rounded outline (`set_blur_region`) and this
//! module forwards it as a `wl_region`.
//!
//! Wire-up: GDK already owns the Wayland connection, so this borrows its
//! `wl_display` and the window's `wl_surface` (gdkwayland-sys) and wraps them
//! with wayland-client's libwayland backend (`client_system`, the only backend
//! that can adopt foreign objects). Everything lives on a private event queue,
//! so GDK's own dispatching never sees our objects, and every call runs on the
//! GTK main thread, where GDK is not reading the socket concurrently.
//!
//! No-op unless the display is a GdkWaylandDisplay *and* the compositor
//! advertises `org_kde_kwin_blur_manager`; Hyprland and others never get past
//! the first probe (see `layer_shell::apply_compositor_blur` for Hyprland).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use gtk::glib::translate::{from_glib, ToGlibPtr};
use gtk::prelude::*;
use tauri::{AppHandle, Manager};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry, wl_surface::WlSurface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
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
/// `[appearance] blur`. Off = the region is emptied (the blur object stays).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Latched after the first probe fails (X11, or no KWin blur manager).
static UNSUPPORTED: AtomicBool = AtomicBool::new(false);
/// One main-thread apply in flight at a time; it reads the latest rects.
static PENDING: AtomicBool = AtomicBool::new(false);
static RECTS: Mutex<Vec<BlurRect>> = Mutex::new(Vec::new());

thread_local! {
    // GTK main thread only.
    static KWIN: RefCell<Option<KwinBlur>> = const { RefCell::new(None) };
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
    if UNSUPPORTED.load(Ordering::Relaxed) || PENDING.swap(true, Ordering::AcqRel) {
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

    let changed = KWIN.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            // Nothing to blur yet: don't probe (or bind globals) just to clear.
            if rects.is_empty() {
                return false;
            }
            match KwinBlur::connect(&gtk_win) {
                Some(kwin) => *cell = Some(kwin),
                None => {
                    UNSUPPORTED.store(true, Ordering::Relaxed);
                    return false;
                }
            }
            // GDK destroys the wl_surface when the window unmaps; release the
            // blur object while that surface still exists.
            gtk_win.connect_unmap(|_| {
                KWIN.with(|cell| {
                    if let Some(kwin) = cell.borrow_mut().as_mut() {
                        kwin.release_blur();
                    }
                });
            });
        }
        cell.as_mut().is_some_and(|kwin| kwin.set_region(&gtk_win, &rects))
    });

    // Blur state is double-buffered on the wl_surface: make sure a commit
    // follows, not whenever the webview next happens to paint.
    if changed {
        gtk_win.queue_draw();
    }
}

struct KwinBlur {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    compositor: WlCompositor,
    manager: OrgKdeKwinBlurManager,
    /// The blur object and the address of the wl_surface it belongs to. GDK
    /// swaps the wl_surface on every hide/show, so this is per surface.
    blur: Option<(usize, OrgKdeKwinBlur)>,
}

impl KwinBlur {
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
        let manager = globals.bind::<OrgKdeKwinBlurManager, _, _>(&qh, 1..=1, ()).ok()?;
        let compositor = globals.bind::<WlCompositor, _, _>(&qh, 1..=4, ()).ok()?;
        Some(Self { conn, queue, qh, compositor, manager, blur: None })
    }

    /// Point the blur at `rects` on the window's current wl_surface. Returns
    /// whether anything was sent (and a surface commit is needed).
    fn set_region(&mut self, gtk_win: &gtk::ApplicationWindow, rects: &[BlurRect]) -> bool {
        let Some(surface_ptr) = current_surface(gtk_win) else {
            self.release_blur();
            return false;
        };
        if self.blur.as_ref().map(|(ptr, _)| *ptr) != Some(surface_ptr as usize) {
            self.release_blur();
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
            let blur = self.manager.create(&surface, &self.qh, ());
            self.blur = Some((surface_ptr as usize, blur));
        }
        let Some((_, blur)) = self.blur.as_ref() else { return false };

        // Empty region = blur nothing. (A null region would mean the whole
        // surface, so "off" must never unset it.)
        let region = self.compositor.create_region(&self.qh, ());
        for (x, y, w, h) in rects.iter().filter_map(to_wl_rect) {
            region.add(x, y, w, h);
        }
        blur.set_region(Some(&region));
        blur.commit();
        region.destroy();
        self.flush();
        true
    }

    fn release_blur(&mut self) {
        if let Some((_, blur)) = self.blur.take() {
            blur.release();
            self.flush();
        }
    }

    fn flush(&mut self) {
        let _ = self.conn.flush();
        // Our objects have no events; drain whatever (delete_id bookkeeping)
        // landed on the private queue.
        let _ = self.queue.dispatch_pending(&mut State);
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

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals added/removed after startup don't matter here.
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlRegion);
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
