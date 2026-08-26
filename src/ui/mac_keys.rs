//! Translating macOS virtual key codes into the Windows VK codes the keybind
//! system is written in.
//!
//! **A chord is stored as a Windows VK code on both platforms, deliberately.**
//! It is the same call as the local voice keeping the engine id `sapi`: the
//! number is a wire format, written into `settings.json` and compared against
//! [`crate::keybind`]'s catalogue, and a settings file has to mean the same
//! thing on either machine. Translating here — at the one point where a real
//! keyboard event enters the app — keeps `keybind.rs`, `Chord::label`, the
//! catalogue and every test in them portable and untouched.
//!
//! The two numbering schemes have nothing in common. A Windows VK code is
//! roughly "the character or the named key": `A` is `0x41`, F1 is `0x70`.
//! macOS's `NSEvent.keyCode` is a **position on the keyboard**, inherited from
//! the ADB layout of 1986: `A` is 0, `S` is 1, `D` is 2, because that is where
//! they sit on a US QWERTY board. So there is no arithmetic between them, only a
//! table.
//!
//! That positional basis has one consequence worth stating, because it is a
//! feature and not a bug: **the table is written for the physical keys, so a
//! chord follows the key's position rather than the character a layout prints on
//! it.** A user on a French AZERTY keyboard who binds the key where QWERTY has
//! `A` gets that physical key, which is what "the key next to CAPS LOCK" means
//! to the hand. The alternative — translating the typed character — would move a
//! binding when the user switched input source mid-broadcast.
//!
//! Only keys that can be part of a binding are listed. A key with no entry is
//! not translated and so cannot be bound, which is the safe direction: an
//! unbindable key does nothing, where a wrongly-mapped one fires somebody else's
//! action.

/// The macOS virtual key codes, by name, for the keys a binding may use.
///
/// From `HIToolbox`'s `Events.h` (`kVK_*`). Written out rather than linked
/// against because the framework header is not in any binding crate in the tree
/// and these have not changed since the constants were defined.
mod kvk {
    // Letters, in ADB positional order rather than alphabetical.
    pub const A: u16 = 0x00;
    pub const S: u16 = 0x01;
    pub const D: u16 = 0x02;
    pub const F: u16 = 0x03;
    pub const H: u16 = 0x04;
    pub const G: u16 = 0x05;
    pub const Z: u16 = 0x06;
    pub const X: u16 = 0x07;
    pub const C: u16 = 0x08;
    pub const V: u16 = 0x09;
    pub const B: u16 = 0x0B;
    pub const Q: u16 = 0x0C;
    pub const W: u16 = 0x0D;
    pub const E: u16 = 0x0E;
    pub const R: u16 = 0x0F;
    pub const Y: u16 = 0x10;
    pub const T: u16 = 0x11;
    pub const O: u16 = 0x1F;
    pub const U: u16 = 0x20;
    pub const I: u16 = 0x22;
    pub const P: u16 = 0x23;
    pub const L: u16 = 0x25;
    pub const J: u16 = 0x26;
    pub const K: u16 = 0x28;
    pub const N: u16 = 0x2D;
    pub const M: u16 = 0x2E;

    // Digits along the top row.
    pub const ONE: u16 = 0x12;
    pub const TWO: u16 = 0x13;
    pub const THREE: u16 = 0x14;
    pub const FOUR: u16 = 0x15;
    pub const SIX: u16 = 0x16;
    pub const FIVE: u16 = 0x17;
    pub const NINE: u16 = 0x19;
    pub const SEVEN: u16 = 0x1A;
    pub const EIGHT: u16 = 0x1C;
    pub const ZERO: u16 = 0x1D;

    // Named keys.
    pub const RETURN: u16 = 0x24;
    pub const TAB: u16 = 0x30;
    pub const SPACE: u16 = 0x31;
    pub const DELETE: u16 = 0x33;
    pub const ESCAPE: u16 = 0x35;
    pub const HOME: u16 = 0x73;
    pub const PAGE_UP: u16 = 0x74;
    pub const FORWARD_DELETE: u16 = 0x75;
    pub const END: u16 = 0x77;
    pub const PAGE_DOWN: u16 = 0x79;
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const DOWN: u16 = 0x7D;
    pub const UP: u16 = 0x7E;

    // Function keys. Not contiguous, and not in order.
    pub const F1: u16 = 0x7A;
    pub const F2: u16 = 0x78;
    pub const F3: u16 = 0x63;
    pub const F4: u16 = 0x76;
    pub const F5: u16 = 0x60;
    pub const F6: u16 = 0x61;
    pub const F7: u16 = 0x62;
    pub const F8: u16 = 0x64;
    pub const F9: u16 = 0x65;
    pub const F10: u16 = 0x6D;
    pub const F11: u16 = 0x67;
    pub const F12: u16 = 0x6F;
}

/// The Windows VK codes this file maps onto, for the named keys.
///
/// The letters and digits need no constants — a VK code for those *is* the
/// ASCII value of the uppercase character, which is why `keybind.rs` writes
/// `b'O' as u32`.
mod vk {
    pub const BACK: u32 = 0x08;
    pub const TAB: u32 = 0x09;
    pub const RETURN: u32 = 0x0D;
    pub const ESCAPE: u32 = 0x1B;
    pub const SPACE: u32 = 0x20;
    pub const PRIOR: u32 = 0x21;
    pub const NEXT: u32 = 0x22;
    pub const END: u32 = 0x23;
    pub const HOME: u32 = 0x24;
    pub const LEFT: u32 = 0x25;
    pub const UP: u32 = 0x26;
    pub const RIGHT: u32 = 0x27;
    pub const DOWN: u32 = 0x28;
    pub const DELETE: u32 = 0x2E;
    pub const F1: u32 = 0x70;
}

/// Every key a binding may use, as `(macOS key code, Windows VK code)`.
const TABLE: &[(u16, u32)] = &[
    (kvk::A, b'A' as u32),
    (kvk::B, b'B' as u32),
    (kvk::C, b'C' as u32),
    (kvk::D, b'D' as u32),
    (kvk::E, b'E' as u32),
    (kvk::F, b'F' as u32),
    (kvk::G, b'G' as u32),
    (kvk::H, b'H' as u32),
    (kvk::I, b'I' as u32),
    (kvk::J, b'J' as u32),
    (kvk::K, b'K' as u32),
    (kvk::L, b'L' as u32),
    (kvk::M, b'M' as u32),
    (kvk::N, b'N' as u32),
    (kvk::O, b'O' as u32),
    (kvk::P, b'P' as u32),
    (kvk::Q, b'Q' as u32),
    (kvk::R, b'R' as u32),
    (kvk::S, b'S' as u32),
    (kvk::T, b'T' as u32),
    (kvk::U, b'U' as u32),
    (kvk::V, b'V' as u32),
    (kvk::W, b'W' as u32),
    (kvk::X, b'X' as u32),
    (kvk::Y, b'Y' as u32),
    (kvk::Z, b'Z' as u32),
    (kvk::ZERO, b'0' as u32),
    (kvk::ONE, b'1' as u32),
    (kvk::TWO, b'2' as u32),
    (kvk::THREE, b'3' as u32),
    (kvk::FOUR, b'4' as u32),
    (kvk::FIVE, b'5' as u32),
    (kvk::SIX, b'6' as u32),
    (kvk::SEVEN, b'7' as u32),
    (kvk::EIGHT, b'8' as u32),
    (kvk::NINE, b'9' as u32),
    (kvk::RETURN, vk::RETURN),
    (kvk::TAB, vk::TAB),
    (kvk::SPACE, vk::SPACE),
    // macOS calls the key above RETURN "delete" and the one on the extended
    // keyboard "forward delete"; Windows calls them BACKSPACE and DELETE. The
    // names cross over, which is exactly the sort of thing a table is for.
    (kvk::DELETE, vk::BACK),
    (kvk::FORWARD_DELETE, vk::DELETE),
    (kvk::ESCAPE, vk::ESCAPE),
    (kvk::HOME, vk::HOME),
    (kvk::END, vk::END),
    (kvk::PAGE_UP, vk::PRIOR),
    (kvk::PAGE_DOWN, vk::NEXT),
    (kvk::LEFT, vk::LEFT),
    (kvk::RIGHT, vk::RIGHT),
    (kvk::UP, vk::UP),
    (kvk::DOWN, vk::DOWN),
    (kvk::F1, vk::F1),
    (kvk::F2, vk::F1 + 1),
    (kvk::F3, vk::F1 + 2),
    (kvk::F4, vk::F1 + 3),
    (kvk::F5, vk::F1 + 4),
    (kvk::F6, vk::F1 + 5),
    (kvk::F7, vk::F1 + 6),
    (kvk::F8, vk::F1 + 7),
    (kvk::F9, vk::F1 + 8),
    (kvk::F10, vk::F1 + 9),
    (kvk::F11, vk::F1 + 10),
    (kvk::F12, vk::F1 + 11),
];

/// The Windows VK code for a macOS key code, or `None` for a key no binding may
/// use.
pub fn vk_for(key_code: u16) -> Option<u32> {
    TABLE
        .iter()
        .find(|(mac, _)| *mac == key_code)
        .map(|(_, vk)| *vk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybind::{VK_F1, VK_F6, VK_F9, VK_F10, VK_O};

    /// The four keys the app ships bindings for, plus the one it hands a new
    /// Media Player. If any of these is wrong, a default binding fires the wrong
    /// action or none at all.
    #[test]
    fn the_shipped_bindings_translate() {
        assert_eq!(vk_for(kvk::F1), Some(VK_F1));
        assert_eq!(vk_for(kvk::F6), Some(VK_F6));
        assert_eq!(vk_for(kvk::F9), Some(VK_F9));
        assert_eq!(vk_for(kvk::F10), Some(VK_F10));
        assert_eq!(vk_for(kvk::O), Some(VK_O));
    }

    /// Function keys are contiguous in the Windows numbering and scattered in
    /// the macOS one, which is the easiest place in the table to introduce an
    /// off-by-one that only shows up as "F8 does what F7 should".
    #[test]
    fn every_function_key_lands_on_its_own_number() {
        let macs = [
            kvk::F1,
            kvk::F2,
            kvk::F3,
            kvk::F4,
            kvk::F5,
            kvk::F6,
            kvk::F7,
            kvk::F8,
            kvk::F9,
            kvk::F10,
            kvk::F11,
            kvk::F12,
        ];
        for (index, mac) in macs.iter().enumerate() {
            assert_eq!(
                vk_for(*mac),
                Some(VK_F1 + index as u32),
                "F{} translated wrongly",
                index + 1
            );
        }
    }

    /// A VK code for a letter or digit is its ASCII value, which is what
    /// `keybind.rs` relies on when it writes `b'O' as u32`.
    #[test]
    fn letters_and_digits_are_their_ascii_values() {
        assert_eq!(vk_for(kvk::A), Some(0x41));
        assert_eq!(vk_for(kvk::Z), Some(0x5A));
        assert_eq!(vk_for(kvk::ZERO), Some(0x30));
        assert_eq!(vk_for(kvk::NINE), Some(0x39));
    }

    /// The two platforms swap these names, so a copy-paste error here is a
    /// binding on BACKSPACE that fires on DELETE.
    #[test]
    fn backspace_and_delete_are_not_crossed() {
        assert_eq!(vk_for(kvk::DELETE), Some(vk::BACK));
        assert_eq!(vk_for(kvk::FORWARD_DELETE), Some(vk::DELETE));
    }

    /// No two macOS keys may map to one VK code, or two physical keys would fire
    /// the same binding. This is the check that catches a mistyped constant.
    #[test]
    fn the_table_is_one_to_one() {
        let mut vks: Vec<u32> = TABLE.iter().map(|(_, vk)| *vk).collect();
        vks.sort_unstable();
        let before = vks.len();
        vks.dedup();
        assert_eq!(before, vks.len(), "two macOS keys map to one VK code");

        let mut macs: Vec<u16> = TABLE.iter().map(|(mac, _)| *mac).collect();
        macs.sort_unstable();
        let before = macs.len();
        macs.dedup();
        assert_eq!(before, macs.len(), "one macOS key appears twice");
    }

    /// An unlisted key must translate to nothing rather than to something: a key
    /// that cannot be bound does nothing, where a wrongly-mapped one fires
    /// somebody else's action. `0x3F` is the Fn key, which is not bindable.
    #[test]
    fn an_unlisted_key_is_not_translated() {
        assert_eq!(vk_for(0x3F), None);
        assert_eq!(vk_for(0xFFFF), None);
    }
}
