//! The keyboard convention every slider in the app obeys, and nothing else.
//!
//! Split out of `slider_uia` because that module is a Windows UI Automation
//! provider and this is pure arithmetic over wx key codes. macOS needs no
//! provider — VoiceOver re-reads a slider's value on every step with focus
//! unmoved, which is the hardest thing the Windows side does — but it needs
//! exactly the same key mapping, because that mapping is a promise to the user
//! rather than a workaround for a platform.

/// Maps a movement key to the slider's new value, or `None` when `code` is not a
/// movement key — the caller must then `event.skip(true)` so the control doesn't
/// go dead.
///
/// Every slider owns its whole movement mapping rather than letting the native
/// trackbar have the keys, because the trackbar's directions are the opposite of
/// what people expect: natively Up and Page Up move *down* and Down and Page
/// Down move *up*. Home is the maximum and End the minimum here, matching the
/// mixer strips in [`super::home`] — which is the inverse of the native mapping
/// too, and the inverse of the usual convention, but consistency across the
/// app's sliders is what a screen-reader user is relying on.
///
/// `page` is per-slider: a range of 20 wants a smaller page step than a range of
/// 100. The result is always clamped to `min..=max`, so a caller can announce it
/// unconditionally.
pub fn key_step(code: i32, current: i32, min: i32, max: i32, page: i32) -> Option<i32> {
    use super::{
        WXK_DOWN, WXK_END, WXK_HOME, WXK_LEFT, WXK_PAGEDOWN, WXK_PAGEUP, WXK_RIGHT, WXK_UP,
    };
    let value = match code {
        WXK_UP | WXK_RIGHT => current.saturating_add(1),
        WXK_DOWN | WXK_LEFT => current.saturating_sub(1),
        WXK_PAGEUP => current.saturating_add(page),
        WXK_PAGEDOWN => current.saturating_sub(page),
        WXK_HOME => max,
        WXK_END => min,
        _ => return None,
    };
    Some(value.clamp(min, max))
}

#[cfg(test)]
mod tests {
    use super::key_step;
    use crate::ui::{
        WXK_DELETE, WXK_DOWN, WXK_END, WXK_HOME, WXK_LEFT, WXK_PAGEDOWN, WXK_PAGEUP, WXK_RIGHT,
        WXK_UP,
    };

    #[test]
    fn arrows_step_by_one_in_the_expected_direction() {
        assert_eq!(key_step(WXK_UP, 50, 0, 100, 10), Some(51));
        assert_eq!(key_step(WXK_RIGHT, 50, 0, 100, 10), Some(51));
        assert_eq!(key_step(WXK_DOWN, 50, 0, 100, 10), Some(49));
        assert_eq!(key_step(WXK_LEFT, 50, 0, 100, 10), Some(49));
    }

    #[test]
    fn page_keys_use_the_page_step() {
        assert_eq!(key_step(WXK_PAGEUP, 50, 0, 100, 10), Some(60));
        assert_eq!(key_step(WXK_PAGEDOWN, 50, 0, 100, 10), Some(40));
        // A narrow range gets a narrow page step.
        assert_eq!(key_step(WXK_PAGEUP, 0, -10, 10, 2), Some(2));
    }

    #[test]
    fn home_is_the_maximum_and_end_the_minimum() {
        assert_eq!(key_step(WXK_HOME, 50, 0, 100, 10), Some(100));
        assert_eq!(key_step(WXK_END, 50, 0, 100, 10), Some(0));
        assert_eq!(key_step(WXK_HOME, 0, -50, 50, 10), Some(50));
        assert_eq!(key_step(WXK_END, 0, -50, 50, 10), Some(-50));
    }

    #[test]
    fn results_are_clamped_at_both_ends() {
        assert_eq!(key_step(WXK_UP, 100, 0, 100, 10), Some(100));
        assert_eq!(key_step(WXK_DOWN, 0, 0, 100, 10), Some(0));
        assert_eq!(key_step(WXK_PAGEUP, 95, 0, 100, 10), Some(100));
        assert_eq!(key_step(WXK_PAGEDOWN, 5, 0, 100, 10), Some(0));
    }

    #[test]
    fn negative_ranges_work() {
        assert_eq!(key_step(WXK_DOWN, 0, -10, 10, 2), Some(-1));
        assert_eq!(key_step(WXK_PAGEDOWN, -9, -10, 10, 2), Some(-10));
        assert_eq!(key_step(WXK_UP, -10, -10, 10, 2), Some(-9));
    }

    #[test]
    fn other_keys_are_not_movement() {
        assert_eq!(key_step(WXK_DELETE, 50, 0, 100, 10), None);
        assert_eq!(key_step(b'A' as i32, 50, 0, 100, 10), None);
    }
}
