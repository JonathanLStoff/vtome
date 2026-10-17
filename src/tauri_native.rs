//! What [`TauriVtome`](crate::TauriVtome)'s output windows need from each
//! platform that Tauri does not give: hearing a right-click on a bare window (a
//! window with no webview reports no mouse input through Tauri), and, on
//! macOS, sitting above the menu bar.
//!
//! Everything here runs on the main thread, where the windows were made.

// On a phone no output window is ever made, so only `forget` is reached.
#![cfg_attr(any(target_os = "ios", target_os = "android"), allow(dead_code))]

use std::sync::Arc;

use tauri::Runtime;

use crate::error::{Error, Result};

/// What a right-click on one output runs.
pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

/// Calls `hook` on the main thread each time `window` is right-clicked.
/// `label` is the window's label, for [`forget`].
pub(crate) fn watch_right_clicks<R: Runtime>(window: &tauri::Window<R>, label: &str, hook: Hook) -> Result<()> {
    platform::watch_right_clicks(window, label, hook)
}

/// Stops calling the hook of the window labelled `label`. The window's own
/// handlers go with it when it is destroyed; this is for what outlives it.
pub(crate) fn forget(label: &str) {
    platform::forget(label);
}

/// Raises `window` above the menu bar and the Dock. Only macOS has a window
/// level below the menu bar for always-on-top windows; elsewhere this does
/// nothing.
pub(crate) fn cover_menu_bar<R: Runtime>(window: &tauri::Window<R>) -> Result<()> {
    platform::cover_menu_bar(window)
}

fn native_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::Render {
        reason: format!("{what}: {error}"),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! One application-local event monitor for right mouse downs, installed on
    //! the first output, which looks the event's window up among the outputs'.
    //! An event monitor sees the event before the window does and passes it
    //! on, so nothing else changes.

    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex, Once, PoisonError};

    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSStatusWindowLevel, NSWindow};
    use tauri::Runtime;

    use super::{native_error, Hook};
    use crate::error::Result;

    /// An output's window, by AppKit's number for it, and what its right-click runs.
    struct Watched {
        label: String,
        number: isize,
        hook: Hook,
    }

    static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());

    fn watched() -> std::sync::MutexGuard<'static, Vec<Watched>> {
        WATCHED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The NSWindow behind a Tauri window.
    fn ns_window<R: Runtime>(window: &tauri::Window<R>) -> Result<NonNull<NSWindow>> {
        let pointer = window.ns_window().map_err(|error| native_error("no NSWindow for an output", error))?;
        NonNull::new(pointer.cast::<NSWindow>()).ok_or_else(|| native_error("no NSWindow for an output", "null"))
    }

    pub(super) fn cover_menu_bar<R: Runtime>(window: &tauri::Window<R>) -> Result<()> {
        let ns_window = ns_window(window)?;
        // SAFETY: a live NSWindow Tauri just made, and this is the main thread,
        // where AppKit wants it changed. The status level is just above the
        // main menu's and the Dock's.
        unsafe { ns_window.as_ref() }.setLevel(NSStatusWindowLevel);
        Ok(())
    }

    pub(super) fn watch_right_clicks<R: Runtime>(window: &tauri::Window<R>, label: &str, hook: Hook) -> Result<()> {
        let ns_window = ns_window(window)?;
        // SAFETY: as in `cover_menu_bar`.
        let number = unsafe { ns_window.as_ref() }.windowNumber();
        watched().push(Watched {
            label: label.to_string(),
            number,
            hook,
        });
        install_monitor();
        Ok(())
    }

    pub(super) fn forget(label: &str) {
        watched().retain(|watched| watched.label != label);
    }

    /// Once per process: the monitor lives as long as the application.
    fn install_monitor() {
        static INSTALLED: Once = Once::new();

        INSTALLED.call_once(|| {
            let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
                // SAFETY: AppKit hands the monitor a live event.
                let number = unsafe { event.as_ref() }.windowNumber();
                // Called after the lock is let go: the hook may open windows
                // or menus that send events of their own.
                let hook = watched().iter().find(|watched| watched.number == number).map(|watched| Arc::clone(&watched.hook));
                if let Some(hook) = hook {
                    hook();
                }
                event.as_ptr()
            });

            // SAFETY: the main thread (an output is being made on it), and the
            // block returns the event it was given, which AppKit requires.
            let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::RightMouseDown, &block) };
            // Kept for the life of the process; nothing removes it.
            std::mem::forget(monitor);
        });
    }
}

#[cfg(windows)]
mod platform {
    //! A window subclass on each output that hears `WM_RBUTTONUP`. The hook is
    //! the subclass's reference data, freed when the window is destroyed.

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use tauri::Runtime;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCDESTROY, WM_RBUTTONUP};

    use super::{native_error, Hook};
    use crate::error::Result;

    /// The subclass's id: one per window is all there is.
    const SUBCLASS: usize = 0x7674_6f6d; // "vtom"

    unsafe extern "system" fn subclass(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, id: usize, data: usize) -> LRESULT {
        let hook = data as *const Hook;
        match message {
            // SAFETY: `data` is the `Box<Hook>` given to `SetWindowSubclass`,
            // alive until `WM_NCDESTROY` below.
            WM_RBUTTONUP => (unsafe { &*hook })(),
            WM_NCDESTROY => {
                // SAFETY: the window's last message: the box is dropped once,
                // after the subclass that could reach it is removed.
                unsafe {
                    let _ = RemoveWindowSubclass(hwnd, Some(subclass), id);
                    drop(Box::from_raw(hook.cast_mut()));
                }
            }
            _ => {}
        }
        // SAFETY: forwarding the message, as a subclass must.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    pub(super) fn watch_right_clicks<R: Runtime>(window: &tauri::Window<R>, _label: &str, hook: Hook) -> Result<()> {
        let handle = window.window_handle().map_err(|error| native_error("no native handle for an output", error))?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return Err(native_error("an output window", "not a Win32 window"));
        };
        let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

        let data = Box::into_raw(Box::new(hook));
        // SAFETY: a live window of this thread (the main thread made it); the
        // box is freed by the subclass on `WM_NCDESTROY`, or here if it was
        // never installed.
        let installed = unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, data as usize) };
        if !installed.as_bool() {
            drop(unsafe { Box::from_raw(data) });
            return Err(native_error("an output window", "Windows would not subclass it"));
        }
        Ok(())
    }

    pub(super) fn forget(_label: &str) {}

    pub(super) fn cover_menu_bar<R: Runtime>(_window: &tauri::Window<R>) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! A GTK button-press handler on each output; it goes with the window.

    use gtk::prelude::*;
    use tauri::Runtime;

    use super::{native_error, Hook};
    use crate::error::Result;

    /// GDK's number for the secondary (right) button.
    const SECONDARY: u32 = 3;

    pub(super) fn watch_right_clicks<R: Runtime>(window: &tauri::Window<R>, _label: &str, hook: Hook) -> Result<()> {
        let gtk_window = window.gtk_window().map_err(|error| native_error("no GTK window for an output", error))?;
        gtk_window.add_events(gtk::gdk::EventMask::BUTTON_PRESS_MASK);
        gtk_window.connect_button_press_event(move |_, event| {
            if event.button() == SECONDARY && event.event_type() == gtk::gdk::EventType::ButtonPress {
                hook();
            }
            gtk::glib::Propagation::Proceed
        });
        Ok(())
    }

    pub(super) fn forget(_label: &str) {}

    pub(super) fn cover_menu_bar<R: Runtime>(_window: &tauri::Window<R>) -> Result<()> {
        Ok(())
    }
}

/// Anywhere else (the BSDs, mobile): no right-clicks, nothing to cover.
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use tauri::Runtime;

    use super::Hook;
    use crate::error::Result;

    pub(super) fn watch_right_clicks<R: Runtime>(_window: &tauri::Window<R>, _label: &str, _hook: Hook) -> Result<()> {
        Ok(())
    }

    pub(super) fn forget(_label: &str) {}

    pub(super) fn cover_menu_bar<R: Runtime>(_window: &tauri::Window<R>) -> Result<()> {
        Ok(())
    }
}
