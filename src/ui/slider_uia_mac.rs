//! The macOS counterpart of [`super::slider_uia`] — one AppKit property in
//! place of a hand-written UI Automation server.
//!
//! On MSW a `wxSlider` is a native `msctls_trackbar32` whose built-in UIA
//! provider exposes a bare 0..1000 position, which NVDA reads as a percentage
//! rather than as "-3.0 dB"; and MSAA can supply the right string on *focus* but
//! cannot make a screen reader re-read it on a keyboard step that does not move
//! focus. That is why the Windows file implements `IRawElementProviderSimple`
//! and `IValueProvider`, subclasses the window to answer `WM_GETOBJECT`, keeps a
//! per-HWND provider registry, and raises a property-changed event per step.
//!
//! Cocoa gives all of that for a string assignment. Measured with real
//! VoiceOver: setting `accessibilityValue` makes VoiceOver speak that exact
//! string, **and re-read it on every arrow key with focus unmoved** — the one
//! behaviour the Windows provider exists to obtain. So every method here is the
//! same one wxWidgets call, and there is no event to raise, nothing to register
//! and nothing to tear down.
//!
//! Two consequences for the shared API:
//!
//! - `set_text` and `update` differ on Windows only in whether an event is
//!   raised. There is no such distinction here, so they do the same thing. That
//!   is not a lost feature: the Windows split exists to avoid a *redundant*
//!   re-read when something else is already speaking, and VoiceOver only speaks
//!   the value when the user moves the slider anyway.
//! - `uninstall` has nothing to remove. It is still honoured, and the announcer
//!   still stops touching the control afterwards, because the caller's contract
//!   — uninstall before the window is destroyed — is what keeps the stored
//!   pointer from outliving the widget.
//!
//! [`key_step`] is shared with Windows, from `super::slider_keys`: the arrow,
//! page, Home and End convention is a promise to the user rather than a
//! workaround for a platform.

use std::cell::{Cell, RefCell};

use wxdragon::ffi;
use wxdragon::prelude::*;

/// See [`super::slider_keys::key_step`]. Re-exported so `slider_uia::key_step`
/// names the same function on both platforms.
pub use super::slider_keys::key_step;

/// A borrowed wx window, kept by pointer so the announcer outlives the `&dyn
/// WxWidget` it was installed from.
///
/// `WxWidget` needs exactly one method, and every accessibility setter in
/// wxdragon null-checks the pointer before using it — so a slider destroyed
/// without the documented `uninstall` degrades to a no-op rather than to
/// undefined behaviour, provided the wx window object itself is still allocated.
/// Keeping to the contract is still the rule; this is a floor, not a licence.
struct WindowRef(*mut ffi::wxd_Window_t);

impl WxWidget for WindowRef {
    fn handle_ptr(&self) -> *mut ffi::wxd_Window_t {
        self.0
    }
}

/// The accessible name and value of one slider. Drop (or
/// [`uninstall`](Self::uninstall)) stops it writing to the control; keep it
/// alive for as long as the slider exists.
pub struct SliderAnnouncer {
    window: WindowRef,
    /// Mirrors what was last written, so `update_value` can keep the name and
    /// `set_name` can keep the value without reading them back out of AppKit.
    text: RefCell<(String, String)>,
    done: Cell<bool>,
}

/// Binds an announcer to `slider`. Nothing is installed on the control itself —
/// the name is the Windows one, kept so both platforms read alike at the call
/// sites.
pub fn install(slider: &dyn WxWidget) -> SliderAnnouncer {
    SliderAnnouncer {
        window: WindowRef(slider.handle_ptr()),
        text: RefCell::new((String::new(), String::new())),
        done: Cell::new(false),
    }
}

impl SliderAnnouncer {
    /// Sets the announced name and formatted value.
    pub fn set_text(&self, name: &str, value: &str) {
        self.set_name(name);
        self.set_value_text(value);
    }

    /// Replaces the announced name, keeping the current value. For a slider
    /// whose meaning changes under it (the TTS pitch slider, which says so when
    /// the chosen engine ignores it).
    pub fn set_name(&self, name: &str) {
        if self.done.get() {
            return;
        }
        self.text.borrow_mut().0 = name.to_string();
        self.window.set_accessibility_label(name);
    }

    /// Replaces the announced value, keeping the current name.
    pub fn set_value_text(&self, value: &str) {
        if self.done.get() {
            return;
        }
        self.text.borrow_mut().1 = value.to_string();
        self.window.set_accessibility_value(value);
    }

    /// Announces a new value under whatever name is already set, so callers that
    /// set the name once don't have to carry it into every keystroke.
    pub fn update_value(&self, value: &str) {
        self.set_value_text(value);
    }

    /// Sets the announced name and formatted value. Identical to
    /// [`set_text`](Self::set_text) here — see the module header.
    pub fn update(&self, name: &str, value: &str) {
        self.set_text(name, value);
    }

    /// Stops the announcer touching the control. Idempotent; also runs on drop.
    pub fn uninstall(&self) {
        self.done.set(true);
    }
}

impl Drop for SliderAnnouncer {
    fn drop(&mut self) {
        self.uninstall();
    }
}
