//! The two things wxWidgets does not give a keyboard user on macOS.
//!
//! Phase 0 measured wx against real VoiceOver and found almost everything the
//! Windows accessibility files fight for is native on Cocoa — see
//! [`super::native_acc_mac`] and [`super::picker_acc_mac`], both of which are
//! deletions rather than ports. Two gaps survived that measurement, and neither
//! is an accessibility-object problem: they are both keys that do not arrive.
//!
//! **ENTER does not reach a dialog's confirm button.** `wxWindow::SetDefault`
//! sets wx's own idea of a default item, which on MSW becomes the native
//! `DM_SETDEFID` and works. wxOSX has no equivalent that VoiceOver-driven
//! keyboard use can reach, so the button is told directly: an `NSButton` with a
//! key equivalent of Return is activated by Return, which is the same mechanism
//! AppKit's own default buttons use.
//!
//! **Arrow keys do not move within a radio group.** On Windows a `wxRadioBox`
//! is a group of real radio *windows* and the native group handles arrows;
//! Cocoa's is one control whose items VoiceOver names correctly but which
//! wxOSX leaves without arrow traversal. That is a keyboard behaviour rather
//! than a naming one, so it is handled here rather than in `native_acc_mac`,
//! which has nothing to name.
//!
//! Both are wired from the call sites the Windows files already have, so no
//! caller is `cfg`'d.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSButton, NSView, NSWindow};
use objc2_foundation::MainThreadMarker;
use objc2_foundation::NSString;
use std::ptr::NonNull;
use wxdragon::keycode::{WXK_DOWN, WXK_LEFT, WXK_RIGHT, WXK_UP};
use wxdragon::prelude::*;
use wxdragon::widgets::RadioBox;

/// The Objective-C object behind a wx widget, if it has a native peer yet.
fn peer(widget: &dyn WxWidget) -> Option<NonNull<AnyObject>> {
    NonNull::new(widget.get_handle().cast::<AnyObject>())
}

/// Makes ENTER activate `button`.
///
/// Called by [`super::ok_button`] and [`super::dismiss_button`] straight after
/// `set_default()`, so every hand-built dialog in the app gets it without a
/// `cfg` at the call site — the rule `agents.md` and the dialog notes in
/// `CLAUDE.md` describe as owing both keys.
///
/// ESCAPE needs nothing here: it already works, because those buttons carry a
/// real `ID_CANCEL` and wx maps Escape to that id on every platform.
pub fn set_enter_activates(button: &dyn WxWidget) {
    let Some(peer) = peer(button) else {
        return;
    };
    // SAFETY: the handle is a live Objective-C object; whether it is the
    // `NSButton` is what the downcast establishes. wx wraps its buttons in
    // subclasses of `NSButton`, so this succeeds — and if a future wx changes
    // that, this does nothing rather than sending an unrecognised selector.
    let object = unsafe { peer.as_ref() };
    let Some(button) = object.downcast_ref::<NSButton>() else {
        return;
    };
    // "\r" is the key equivalent AppKit uses for its own default buttons, and
    // setting it also gives the button the default-button look.
    button.setKeyEquivalent(&NSString::from_str("\r"));
}

/// Gives `radio` arrow-key traversal.
///
/// Left and Up move to the previous item, Right and Down to the next, and both
/// ends stop rather than wrap — which is what AppKit's own radio groups do, and
/// what a screen-reader user listening for the end of a list expects.
///
/// The selection is changed with `set_selection`, which does **not** raise a
/// command event, so the dialog's own handler is invoked directly afterwards by
/// the caller if it needs one. Every current call site reads the selection when
/// the dialog is confirmed rather than on change, so none needs that today.
pub fn install_radio_arrows(radio: &RadioBox) {
    let target = *radio;
    radio.on_key_down(move |event| {
        let Some((code, _ctrl)) = super::key_of(&event) else {
            event.skip(true);
            return;
        };
        let count = target.get_count() as i32;
        if count <= 1 {
            event.skip(true);
            return;
        }
        let current = target.get_selection();
        let next = match code {
            WXK_LEFT | WXK_UP => current - 1,
            WXK_RIGHT | WXK_DOWN => current + 1,
            _ => {
                // Not ours: skipping is what keeps TAB, SPACE and every other
                // key working on the control.
                event.skip(true);
                return;
            }
        };
        if next < 0 || next >= count {
            // At the end of the group. The key is still consumed, so focus does
            // not jump out of the group on an arrow — which is what would
            // otherwise happen and is disorienting mid-list.
            event.skip(false);
            return;
        }
        target.set_selection(next);
        // Cocoa announces the newly selected radio item on its own once the
        // selection moves, which is why there is no `help::announce` here.
        event.skip(false);
    });
}

/// The `NSWindow` a wx handle belongs to, as a plain address.
///
/// wx hands back an `NSView` for an ordinary control and, for a top-level
/// window, something that may be either — so both are accepted, exactly as
/// [`super::help`]'s announcer does. An address rather than a pointer because
/// the callers only ever compare identities.
pub fn window_id_of(handle: usize) -> Option<usize> {
    let handle = NonNull::new(handle as *mut AnyObject)?;
    // SAFETY: the handle is a live Objective-C object; which class it is, is
    // what the downcasts establish.
    let object = unsafe { handle.as_ref() };
    if let Some(window) = object.downcast_ref::<NSWindow>() {
        return Some(std::ptr::from_ref(window) as usize);
    }
    let view = object.downcast_ref::<NSView>()?;
    view.window()
        .map(|window| Retained::as_ptr(&window) as usize)
}

/// The address of the window currently taking keys, if it is ours.
pub fn key_window_id() -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    NSApplication::sharedApplication(mtm)
        .keyWindow()
        .map(|window| Retained::as_ptr(&window) as usize)
}
