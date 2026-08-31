//! The macOS counterpart of [`super::native_acc`] — and it is almost entirely
//! empty, which is the finding rather than a gap.
//!
//! On MSW that module exists because wx *breaks* native accessibility: every
//! control is wrapped in a `wxWindowAccessible` that answers a list's name with
//! `wxWindow::GetName()` (the literal `"listBox"`) and its child count with the
//! number of child *windows* (zero), so a `ListBox` announces as an empty
//! unnamed thing and its rows are unreachable. The fix there is to subclass
//! `WM_GETOBJECT` and hand the control back to Win32 unmodified.
//!
//! Cocoa has no such wrapper. `wxAccessible` is wxMSW-only, so an `NSTableView`
//! or `NSMatrix` reaches VoiceOver exactly as AppKit built it. This was measured
//! with real VoiceOver rather than assumed, and:
//!
//! - **list rows are announced on every arrow key**, with no code at all. That
//!   is the whole job of `native_acc::install` on Windows.
//! - **radio buttons are named** from their own titles, so
//!   `install_radio_box`'s subclassing has nothing to fix either. What macOS
//!   does *not* give is arrow-key traversal within a group — but that is a
//!   keyboard problem, not a naming one, and it is handled where the keys are.
//!
//! One thing genuinely does not work and has no workaround yet: **a `ListBox`
//! cannot be given a name.** Four routes were tried — `set_accessibility_label`
//! alone, label plus description, the list inside a `StaticBox` carrying the
//! label, and the label applied long after the native peer exists — and all
//! four left the list anonymous. The rows still read; the list has no identity
//! of its own. A real fix means hand-rolled `objc2` `NSAccessibility` on the
//! list's `NSView`, or a patch to wx upstream.
//!
//! It is partly defused by the fact that the VoiceOver cursor reads the
//! `StaticText` in front of the list on the way past, which is why **every list
//! must keep its preceding label on macOS too** — the same rule as on Windows,
//! arrived at for a different reason.
//!
//! The `name` argument is therefore accepted and dropped, deliberately: keeping
//! it in the signature means the 20-odd call sites stay identical across
//! platforms and the names are already written down for whenever the objc2 path
//! is built.

use wxdragon::prelude::*;
use wxdragon::widgets::{CheckListBox, ListBox, RadioBox};

/// Rows are announced natively. See the module header for why the name is not.
pub fn install(_list: &ListBox, _name: &str) {}

/// As [`install`], for a checkable list.
pub fn install_check_list(_list: &CheckListBox, _name: &str) {}

/// Radio items take their names from their own titles under Cocoa, so there is
/// no accessibility object to install.
///
/// Arrow-key traversal inside the group *is* missing, and that is a keyboard
/// behaviour rather than a naming one — so it lives in
/// [`super::mac_ui::install_radio_arrows`], which this calls. Called from here
/// because this is the function every radio box in the app already goes through,
/// which is what keeps the call sites identical on both platforms.
pub fn install_radio_box(radio: &RadioBox, _name: &str) {
    super::mac_ui::install_radio_arrows(radio);
}

/// A dialog's own title is its accessible name on macOS.
pub fn install_in_dialog(_dialog: &dyn WxWidget, _name: &str) {}
