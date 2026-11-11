//! The `Sec-MS-GEC` token, version 2.
//!
//! Version 1 was a plain SHA-256 of `<ticks><client token>`. Late in 2026
//! the read-aloud WebSocket stopped accepting it (403 on the upgrade) while
//! the voice list went on answering, which is how it presented: voices load,
//! nothing speaks. Version 2 is `<CS64>-<SHA-256>`, both over the same
//! string, sent with `Sec-MS-GEC-Version: 2-<chromium version>`.
//!
//! Neither half is a library primitive, and both are ported literally from
//! `edge-tts`'s `drm.py` (`generate_sec_ms_gec_v2`):
//!
//! - **The SHA-256 is of a 57-byte prefix Edge does not publish**, followed by
//!   the token string. What `drm.py` carries instead is the hash state after
//!   the first fourteen rounds of the first block, plus the message schedule
//!   and round-14 constant with the prefix already folded in, so the first
//!   block resumes at round 14 ([`first_block`]). Every later block is plain
//!   SHA-256, and the IV and round constants below are the standard ones.
//! - **CS64** is a multiply-and-rotate mix keyed by the first eight bytes of
//!   that digest ([`cs64`]).
//!
//! When this breaks again, the fix is in `drm.py`: diff its tables against
//! the ones here. `edge_speaks` is the ignored live test that notices, and
//! `the_v2_token_is_pinned` is what keeps a refactor from changing it.

use super::TRUSTED_CLIENT_TOKEN;

/// Seconds between the Windows and Unix epochs; the token is in Windows time.
const WINDOWS_EPOCH_OFFSET: u64 = 11_644_473_600;
/// The token is rounded down to a five-minute boundary, so it is stable for
/// that long and a small clock difference doesn't break the connection.
const TOKEN_PERIOD: u64 = 300;

/// The SHA-256 initial hash value (FIPS 180-4).
const INITIAL: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB,
    0x5BE0CD19,
];

/// The working registers after rounds 0..14 of the first block, i.e. after
/// the hidden prefix's first 56 bytes.
const RESUME_STATE: [u32; 8] = [
    0xFD9FBEA2, 0x91243CE7, 0xBE499094, 0xCD2E1E0A, 0x6C006F49, 0x04BD35AB, 0xFEB0A7E4,
    0x03EFFBD2,
];

/// The prefix's contribution to schedule words 16..30.
const RESUME_SCHEDULE: [u32; 14] = [
    0xEB24BF9E, 0x1558691F, 0xA0C09EE2, 0x0CFA32BA, 0xA622EE8C, 0xD1DD9491, 0xC0DD25A0,
    0xF9E70AB6, 0xF0C31573, 0x7058CE4E, 0x7188E606, 0x00E5AB33, 0xD258B060, 0x72766564,
];

/// Round 14's constant with the prefix's last byte folded in, standing in for
/// `K[14]` in the first block only.
const RESUME_K14: u32 = 0xC9DC06A7;
const SIGMA0_ADJUSTMENT: u32 = 0x059C0B80;
const SIGMA1_ADJUSTMENT: u32 = 0x000B92C0;
/// How many bytes of the message precede the token string.
const PREFIX_LEN: usize = 57;
/// How many bytes of the token string complete the first block.
const SEED_LEN: usize = 64 - PREFIX_LEN;

/// The SHA-256 round constants (FIPS 180-4).
const K: [u32; 64] = [
    0x428A2F98, 0x71374491, 0xB5C0FBCF, 0xE9B5DBA5, 0x3956C25B, 0x59F111F1, 0x923F82A4,
    0xAB1C5ED5, 0xD807AA98, 0x12835B01, 0x243185BE, 0x550C7DC3, 0x72BE5D74, 0x80DEB1FE,
    0x9BDC06A7, 0xC19BF174, 0xE49B69C1, 0xEFBE4786, 0x0FC19DC6, 0x240CA1CC, 0x2DE92C6F,
    0x4A7484AA, 0x5CB0A9DC, 0x76F988DA, 0x983E5152, 0xA831C66D, 0xB00327C8, 0xBF597FC7,
    0xC6E00BF3, 0xD5A79147, 0x06CA6351, 0x14292967, 0x27B70A85, 0x2E1B2138, 0x4D2C6DFC,
    0x53380D13, 0x650A7354, 0x766A0ABB, 0x81C2C92E, 0x92722C85, 0xA2BFE8A1, 0xA81A664B,
    0xC24B8B70, 0xC76C51A3, 0xD192E819, 0xD6990624, 0xF40E3585, 0x106AA070, 0x19A4C116,
    0x1E376C08, 0x2748774C, 0x34B0BCB5, 0x391C0CB3, 0x4ED8AA4A, 0x5B9CCA4F, 0x682E6FF3,
    0x748F82EE, 0x78A5636F, 0x84C87814, 0x8CC70208, 0x90BEFFFA, 0xA4506CEB, 0xBEF9A3F7,
    0xC67178F2,
];

/// `(coefficient, offset)` pairs that open every CS64 run.
const CS64_COEFFICIENTS: [(u32, u32); 14] = [
    (0xE82E91B1, 0xB32700D0),
    (0x44DE24C6, 0x88DFF51D),
    (0x3BEB65BC, 0x29514038),
    (0xC89C5933, 0xB328CBDF),
    (0x8D1EC211, 0xD405B4D5),
    (0x314EF5D5, 0x26E55F3C),
    (0xE92491B1, 0x607801C6),
    (0xF0A5B9D5, 0x82B2C1FB),
    (0x06064CF3, 0xFA51A3DA),
    (0xDA79FA60, 0x207CC464),
    (0xBED20560, 0x3C3C74F3),
    (0x89F9C9F8, 0xD2D14B61),
    (0x35FA38C6, 0xBB04A2C1),
    (0xCB49E3C6, 0xC5626167),
];
const CS64_FACTOR: u32 = 0x82893933;
const CS64_ADJUSTMENT0: u32 = 0xF2F90B1A;
const CS64_ADJUSTMENT1: u32 = 0x0B1A0000;

/// The token for a moment in time: `<16 hex>-<64 hex>`, uppercase.
pub(super) fn sec_ms_gec(unix_seconds: u64) -> String {
    let windows_seconds = unix_seconds + WINDOWS_EPOCH_OFFSET;
    let rounded = windows_seconds - windows_seconds % TOKEN_PERIOD;
    let ticks = rounded as u128 * 10_000_000;
    let data = format!("{ticks}{TRUSTED_CLIENT_TOKEN}");
    let digest = sha256(data.as_bytes());
    let (low, high) = cs64(data.as_bytes(), &digest);
    let mut mixed = [0u8; 8];
    mixed[..4].copy_from_slice(&low.to_le_bytes());
    mixed[4..].copy_from_slice(&high.to_le_bytes());
    format!("{}-{}", hex_upper(&mixed), hex_upper(&digest))
}

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

fn sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

/// Fills schedule words `from..64` by the standard recurrence.
fn expand(w: &mut [u32; 64], from: usize) {
    for i in from..64 {
        w[i] = sigma1(w[i - 2])
            .wrapping_add(w[i - 7])
            .wrapping_add(sigma0(w[i - 15]))
            .wrapping_add(w[i - 16]);
    }
}

/// SHA-256 rounds `first..64`, returning the working registers (not yet added
/// back into the hash state).
fn rounds(state: [u32; 8], w: &[u32; 64], first: usize, k: &[u32; 64]) -> [u32; 8] {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
    for i in first..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = g ^ (e & (f ^ g));
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(k[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) | (c & (a | b));
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(s0.wrapping_add(maj));
    }
    [a, b, c, d, e, f, g, h]
}

fn add(x: [u32; 8], y: [u32; 8]) -> [u32; 8] {
    std::array::from_fn(|i| x[i].wrapping_add(y[i]))
}

/// The hash state after the first block: the hidden prefix plus the first
/// [`SEED_LEN`] bytes of the token string, resumed at round 14.
fn first_block(seed: &[u8]) -> [u32; 8] {
    let mut w = [0u32; 64];
    w[14] = u32::from_be_bytes([0, seed[0], seed[1], seed[2]]);
    w[15] = u32::from_be_bytes([seed[3], seed[4], seed[5], seed[6]]);
    for i in 16..30 {
        let s1 = sigma1(w[i - 2]);
        let mut value = RESUME_SCHEDULE[i - 16]
            .wrapping_add(if i == 16 { s1 ^ SIGMA1_ADJUSTMENT } else { s1 });
        if i >= 21 {
            value = value.wrapping_add(w[i - 7]);
        }
        if i == 29 {
            value = value.wrapping_add(sigma0(w[i - 15]) ^ SIGMA0_ADJUSTMENT);
        }
        w[i] = value;
    }
    w[30] = sigma1(w[28])
        .wrapping_add(w[23])
        .wrapping_add(sigma0(w[15]))
        .wrapping_add(w[14])
        .wrapping_add(RESUME_K14)
        .wrapping_sub(K[14]);
    expand(&mut w, 31);
    let mut k = K;
    k[14] = RESUME_K14;
    add(INITIAL, rounds(RESUME_STATE, &w, 14, &k))
}

/// SHA-256 of the hidden prefix followed by `data`.
fn sha256(data: &[u8]) -> [u8; 32] {
    // The token string is always at least the 32-character client token.
    assert!(data.len() >= SEED_LEN, "token data shorter than its seed");
    let length = PREFIX_LEN + data.len();
    let mut padded = data[SEED_LEN..].to_vec();
    padded.push(0x80);
    let zeros = (55 - length as i64).rem_euclid(64) as usize;
    padded.resize(padded.len() + zeros, 0);
    padded.extend_from_slice(&(length as u64 * 8).to_be_bytes());

    let mut state = first_block(&data[..SEED_LEN]);
    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        expand(&mut w, 16);
        state = add(state, rounds(state, &w, 0, &K));
    }
    let mut digest = [0u8; 32];
    for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *bytes = word.to_be_bytes();
    }
    digest
}

fn rol16(x: u32) -> u32 {
    x.rotate_left(16)
}

/// `data` as little-endian words, zero-padded to a whole word and then to an
/// even number of words.
fn words_le4_even(data: &[u8]) -> Vec<u32> {
    let mut padded = data.to_vec();
    padded.resize(data.len().div_ceil(4) * 4, 0);
    let mut words: Vec<u32> = padded
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    if words.len() % 2 == 1 {
        words.push(0);
    }
    words
}

/// The CS64 mix of `data`, keyed by the first eight bytes of its digest.
fn cs64(data: &[u8], digest: &[u8; 32]) -> (u32, u32) {
    let key_a = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) | 1;
    let key_b = u32::from_le_bytes([digest[4], digest[5], digest[6], digest[7]]) | 1;
    let factors = [
        key_a.wrapping_mul(CS64_FACTOR),
        key_b.wrapping_mul(CS64_FACTOR),
    ];
    let mut terms: Vec<u32> = CS64_COEFFICIENTS
        .iter()
        .enumerate()
        .map(|(i, &(coefficient, _))| coefficient.wrapping_mul(factors[i % 2]))
        .collect();
    let mut offsets: Vec<u32> = CS64_COEFFICIENTS.iter().map(|&(_, o)| o).collect();

    let mut prefixed = Vec::with_capacity(data.len() + 1);
    prefixed.push(0);
    prefixed.extend_from_slice(data);
    for (i, word) in words_le4_even(&prefixed).into_iter().enumerate() {
        let even = i % 2 == 0;
        let mut term = word.wrapping_mul(if even { key_a } else { key_b });
        let mut offset = rol16(word).wrapping_mul(if even { 0xEF0569FB } else { 0xC31713DB });
        if i == 0 {
            term = term.wrapping_add(CS64_ADJUSTMENT0.wrapping_mul(factors[0]));
            offset = offset.wrapping_add(CS64_ADJUSTMENT1);
        }
        terms.push(term);
        offsets.push(offset);
    }

    let (mut swap_acc, mut swap_last, mut state, mut acc) = (0u32, 0u32, 0u32, 0u32);
    for i in (0..terms.len()).step_by(2) {
        let (x, y) = (terms[i], terms[i + 1]);

        let q = rol16(offsets[i].wrapping_add(x)).wrapping_mul(0x76F50F34);
        swap_acc = swap_acc.wrapping_add(rol16(q).wrapping_mul(0xC3EFEA97).wrapping_add(q));
        let q = rol16(offsets[i + 1].wrapping_add(y)).wrapping_mul(0x1881CE3C);
        swap_last = rol16(q).wrapping_mul(0x35BD1EC9).wrapping_add(q);
        swap_acc = swap_acc.wrapping_add(swap_last);

        state = state.wrapping_mul(key_a).wrapping_add(x);
        state = rol16(state).wrapping_mul(0xCF98B111);
        state = rol16(state).wrapping_mul(0x87085B9F);
        state = rol16(state).wrapping_mul(0x12CEB96D);
        state = rol16(state).wrapping_mul(0x257E1D83);
        acc = acc.wrapping_add(state);
        let mut t = state.wrapping_mul(key_b).wrapping_add(y);
        t = rol16(t).wrapping_mul(0xA27416F5);
        t = rol16(t).wrapping_mul(0xD38396FF);
        state = rol16(rol16(t).wrapping_mul(0x7C932B89)).wrapping_mul(0xBFA49F69);
        acc = acc.wrapping_add(state);
    }
    (swap_last ^ state, swap_acc ^ acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resumed-block machinery is only trustworthy if the plain SHA-256
    /// parts are, and those can be checked against a real implementation:
    /// running every round from the standard IV must be SHA-256.
    #[test]
    fn the_rounds_are_standard_sha256() {
        use sha2::{Digest, Sha256};
        // "abc", padded to one block by hand.
        let mut block = [0u8; 64];
        block[..3].copy_from_slice(b"abc");
        block[3] = 0x80;
        block[63] = 24;
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        expand(&mut w, 16);
        let state = add(INITIAL, rounds(INITIAL, &w, 0, &K));
        let mut ours = [0u8; 32];
        for (bytes, word) in ours.as_chunks_mut::<4>().0.iter_mut().zip(state) {
            *bytes = word.to_be_bytes();
        }
        assert_eq!(ours.as_slice(), Sha256::digest(b"abc").as_slice());
    }

    /// Pins the output. These values were taken from this implementation
    /// once `edge_speaks` had passed against the live service with it, so a
    /// change here is a change to what the service is sent.
    #[test]
    fn the_v2_token_is_pinned() {
        assert_eq!(
            sec_ms_gec(1_704_067_200),
            "9586E5FBB4BD2CFF-DE636BA8369C7431ED826BDEEB00ECA1CC52826A6BB736DCB150A2AF37DFC498"
        );
        assert_eq!(
            sec_ms_gec(1_762_858_000),
            "4D83EF8C2B630728-EAAC525D7D95E9F5C53D185DACC505F90744D948BE2F61D278E8406D8523AF26"
        );
    }

    #[test]
    fn the_token_has_the_v2_shape() {
        let token = sec_ms_gec(1_704_067_200);
        let (mixed, digest) = token.split_once('-').expect("two halves");
        assert_eq!(mixed.len(), 16, "{token}");
        assert_eq!(digest.len(), 64, "{token}");
        assert!(
            token
                .chars()
                .all(|c| c == '-' || (c.is_ascii_hexdigit() && !c.is_lowercase())),
            "must be uppercase hex: {token}"
        );
    }

    /// The whole point of the rounding is that clients a few minutes apart
    /// still agree.
    #[test]
    fn the_token_is_stable_across_a_five_minute_window() {
        // 1_704_067_200 + 11_644_473_600 is a multiple of 300, so this is the
        // start of a window.
        let base = 1_704_067_200;
        assert_eq!(sec_ms_gec(base), sec_ms_gec(base + 299));
        assert_ne!(sec_ms_gec(base), sec_ms_gec(base + 300));
    }

    #[test]
    fn words_are_padded_to_an_even_count() {
        assert_eq!(words_le4_even(&[1]), vec![1, 0]);
        assert_eq!(words_le4_even(&[1, 0, 0, 0, 2]), vec![1, 2]);
        assert!(words_le4_even(&[]).is_empty());
    }
}
