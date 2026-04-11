//! macOS-specific window tweaks.
//!
//! When the tab bar is merged into the titlebar area via
//! `with_fullsize_content_view`, macOS still pins the traffic-light
//! buttons (close, minimize, zoom) to their default Y position — near
//! the very top of the window. If our tab bar is taller than the
//! native titlebar, the buttons end up top-aligned rather than
//! centered inside our bar, which looks crowded.
//!
//! This module walks from a winit `Window` down to the underlying
//! `NSWindow` and nudges each traffic-light button's frame so its
//! center sits on the vertical center of our tab bar. The arithmetic
//! is straightforward; everything hairy is isolated in `unsafe`
//! blocks here.

use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow, NSWindowButton};
use objc2_foundation::NSPoint;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tracing::{info, warn};
use winit::window::Window;

/// Move the traffic lights so they sit vertically centered inside a
/// tab bar of `bar_height_logical_px` points. Called once after the
/// window is created, and again on tab bar height changes (e.g. font
/// zoom). Silently no-ops if the underlying `NSWindow` can't be found.
pub fn reposition_traffic_lights(window: &Window, bar_height_logical_px: f32) {
    let ns_window = match get_ns_window(window) {
        Some(w) => w,
        None => {
            warn!("couldn't reach NSWindow — traffic lights will stay top-aligned");
            return;
        }
    };
    let buttons = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ];
    for kind in buttons {
        let Some(button) = ns_window.standardWindowButton(kind) else {
            continue;
        };
        // The button's superview is macOS's titlebar container view,
        // which is typically ~28pt tall — smaller than our tab bar.
        // Our tab bar is drawn from the window's top edge downward
        // (y=0 at top) in wgpu coordinates. AppKit uses bottom-left
        // origin for views; the button's origin.y is measured from the
        // BOTTOM of its superview.
        //
        // We want: button top (in window top-down coords) at
        //     desired_top = (bar_h - button_h) / 2
        // Converting to AppKit parent coords (origin.y measured from
        // the BOTTOM of the parent, which is flush with the titlebar's
        // bottom inside the window):
        //     origin.y = parent_h - button_h - desired_top
        // We assume the parent (titlebar container) is anchored to
        // the top of the window, so parent_bottom_in_window = parent_h.
        let frame = button.frame();
        let button_h = frame.size.height;
        // SAFETY: superview is a standard NSView accessor; no state
        // is mutated and the returned reference is released via arc.
        let (parent_h, parent_y) = unsafe {
            button
                .superview()
                .map(|s| {
                    let f = s.frame();
                    (f.size.height, f.origin.y)
                })
                .unwrap_or((28.0, 0.0))
        };
        let bar_h = bar_height_logical_px as f64;
        let desired_top = ((bar_h - button_h) / 2.0).max(0.0);
        let new_y = (parent_h - button_h - desired_top).max(0.0);
        info!(
            ?kind,
            old_x = frame.origin.x,
            old_y = frame.origin.y,
            button_h,
            parent_h,
            parent_y,
            bar_h,
            desired_top,
            new_y,
            "repositioning traffic light"
        );
        let new_origin = NSPoint {
            x: frame.origin.x,
            y: new_y,
        };
        // SAFETY: setFrameOrigin: is a standard NSView method; the
        // button pointer is valid as long as the window is alive.
        unsafe {
            button.setFrameOrigin(new_origin);
        }
        // Verify that the frame actually moved.
        let after = button.frame();
        info!(
            ?kind,
            after_y = after.origin.y,
            "after reposition"
        );
    }
}

/// Walk from winit's NSView handle up to the containing NSWindow.
fn get_ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit hands out a non-null NSView pointer tied to the
    // window's lifetime. We only borrow it briefly to read `.window`.
    unsafe {
        let view_ptr = h.ns_view.as_ptr() as *mut NSView;
        let view: &NSView = &*view_ptr;
        view.window()
    }
}
