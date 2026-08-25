//! The macOS counterpart of [`super::picker_acc`], and it does nothing at all.
//!
//! That module is 1,400 lines on Windows because a `wxDatePickerCtrl` there is a
//! native `SysDateTimePick32` whose fields — month, day, year; hour, minute,
//! second, AM/PM — are **not** accessible children. Left and right arrow move a
//! caret between them announcing nothing, `accFocus` answers `CHILDID_SELF`
//! whichever field the caret is on, and no API reports the selection. So the
//! Windows file tracks the caret itself from a window subclass, works out which
//! fields the control actually has by writing a probe value and reading the text
//! back, discovers where the caret is by nudging the value and seeing what
//! moved, and speaks the field through `help::announce`.
//!
//! None of that is a problem here. A `wxDatePickerCtrl` on macOS is an
//! `NSDatePicker`, whose fields *are* real accessibility elements, and VoiceOver
//! announces each one as the caret reaches it — measured with real VoiceOver, not
//! assumed. The entire module is a deletion rather than a port.
//!
//! [`Kind`] survives only because `schedule_ui` passes it at the three call
//! sites, which stay identical across platforms.

use wxdragon::prelude::*;

/// Which picker a control is. Unused here; kept so the call sites match
/// Windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Date,
    Time,
}

/// `NSDatePicker` announces its own fields. See the module header.
pub fn install(_picker: &dyn WxWidget, _kind: Kind) {}
