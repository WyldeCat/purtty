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

/// Default macOS traffic-light x positions (close, minimize, zoom).
const DEFAULT_X: [f64; 3] = [9.0, 32.0, 55.0];

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
    // Resize the titlebar container (the buttons' parent view) to
    // match our tab bar height. Without this, macOS fixes the
    // container at ~32pt and clamps the buttons — they can't center
    // in a bar taller than parent_h + button_h.
    let bar_h_f = bar_height_logical_px as f64;
    if let Some(first_btn) = ns_window.standardWindowButton(NSWindowButton::CloseButton) {
        unsafe {
            if let Some(parent) = first_btn.superview() {
                let mut pf = parent.frame();
                let top = pf.origin.y + pf.size.height;
                pf.size.height = bar_h_f;
                pf.origin.y = top - bar_h_f;
                parent.setFrame(pf);
                parent.setNeedsDisplay(true);
            }
        }
    }

    let buttons = [
        (NSWindowButton::CloseButton, DEFAULT_X[0]),
        (NSWindowButton::MiniaturizeButton, DEFAULT_X[1]),
        (NSWindowButton::ZoomButton, DEFAULT_X[2]),
    ];
    for (kind, original_x) in buttons {
        let Some(button) = ns_window.standardWindowButton(kind) else {
            continue;
        };
        let frame = button.frame();
        let button_h = frame.size.height;
        let parent_h = unsafe {
            button
                .superview()
                .map(|s| s.frame().size.height)
                .unwrap_or(bar_h_f)
        };
        let bar_h = bar_height_logical_px as f64;
        let desired_top = ((bar_h - button_h) / 2.0).max(0.0);
        let new_y = (parent_h - button_h - desired_top).max(0.0);
        // Per-frame repositioning — only log at trace to avoid spam.
        tracing::trace!(
            ?kind,
            old_y = frame.origin.y,
            new_y,
            bar_h,
            "repositioning traffic light"
        );
        // x stays at the macOS default — only y moves for vertical
        // centering. Left padding is fixed; top padding grows with
        // bar height.
        let new_origin = NSPoint {
            x: original_x,
            y: new_y,
        };
        // SAFETY: setFrameOrigin / setNeedsDisplay are standard NSView
        // methods; the button pointer is valid while the window is alive.
        unsafe {
            button.setFrameOrigin(new_origin);
            button.setNeedsDisplay();
            // Also mark the parent (titlebar container) dirty so macOS
            // actually redraws the buttons at their new positions.
            if let Some(parent) = button.superview() {
                parent.setNeedsDisplay(true);
            }
        }
    }
}

/// Fixed left inset for the tab bar: tabs start to the right of the
/// traffic lights. Traffic lights stay at their macOS default x, so
/// this is constant regardless of bar height or zoom level.
pub fn tab_bar_left_inset() -> f32 {
    let button_w = 14.0_f64;
    let padding = 10.0_f64;
    (DEFAULT_X[2] + button_w + padding) as f32
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
