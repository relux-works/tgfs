//! SHA-256 (FIPS 180-4) for content addressing and integrity (DOM-021,
//! SYNC-042, SYNC-052).
//!
//! Vendored rather than pulled from a crate on purpose. Content addressing is
//! the one hash the core cannot afford to get wrong, and a self-contained
//! implementation keeps the platform-neutral layer-0 vocabulary free of the
//! build scripts and transitive tree a hashing crate drags in — every build
//! script in this workspace is named on purpose in `deny.toml` `[bans.build]`
//! (POL-6), and `sha2` would add `typenum`'s at minimum. SHA-256 is a fixed,
//! fully specified function with published known-answer vectors, so the choice
//! trades a dependency for a test obligation the module discharges below: the
//! `tests` pin this implementation to the FIPS 180-4 §D examples and the NIST
//! one-million-'a' vector. Any wrong constant, rotation, or step changes the
//! digest of `"abc"`, so the vectors are a complete net.
//!
//! Scope: this hashes already-public content for *identity and integrity*, not
//! secrets, MACs, or password storage. The constant-time and side-channel
//! properties a cryptographic crate advertises are therefore not requirements
//! here, and this implementation makes no such claim.

use crate::identity::ContentHash;

/// Initial hash value H(0): the first 32 bits of the fractional parts of the
/// square roots of the first eight primes (FIPS 180-4 §5.3.3).
const INITIAL_STATE: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Round constants K: the first 32 bits of the fractional parts of the cube
/// roots of the first sixty-four primes (FIPS 180-4 §4.2.2).
const ROUND_CONSTANTS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// One SHA-256 block in bytes.
const BLOCK_BYTES: usize = 64;

/// A streaming SHA-256 hasher (FIPS 180-4).
///
/// Feed the message with any sequence of [`Sha256::update`] calls, then take
/// the digest with [`Sha256::finalize`] or, straight into the domain type,
/// [`Sha256::content_hash`]. The result depends only on the byte sequence, not
/// on how it was chunked, which is what lets the integrity layer hash a staged
/// object one read at a time and still name the same [`ContentHash`].
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// Partial final block; `buffered` bytes are live.
    block: [u8; BLOCK_BYTES],
    buffered: usize,
    /// Total message length in bits, low 64 bits (the padding field is 64-bit
    /// and defined modulo 2^64, FIPS 180-4 §5.1.1).
    length_bits: u64,
}

impl Sha256 {
    /// A fresh hasher over the empty message.
    pub fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            block: [0u8; BLOCK_BYTES],
            buffered: 0,
            length_bits: 0,
        }
    }

    /// Absorbs `data` into the running digest.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self
            .length_bits
            .wrapping_add((data.len() as u64).wrapping_mul(8));

        // Top up a partial block first.
        if self.buffered > 0 {
            let need = BLOCK_BYTES - self.buffered;
            let take = need.min(data.len());
            self.block[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == BLOCK_BYTES {
                let block = self.block;
                compress(&mut self.state, &block);
                self.buffered = 0;
            }
        }

        // Consume whole blocks straight from the input.
        while data.len() >= BLOCK_BYTES {
            let mut block = [0u8; BLOCK_BYTES];
            block.copy_from_slice(&data[..BLOCK_BYTES]);
            compress(&mut self.state, &block);
            data = &data[BLOCK_BYTES..];
        }

        // Hold the remainder for the next call or for finalization.
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    /// Consumes the hasher and returns the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        // Length is captured before padding: the padding bytes fed below must
        // not count toward the length field they encode (FIPS 180-4 §5.1.1).
        let length_bits = self.length_bits;

        // 0x80, then zeros, until exactly 8 bytes remain in the final block
        // for the length. Feeding through `update` reuses the block machinery,
        // including the two-block case where the 0x80 pushes past the boundary.
        self.update(&[0x80]);
        while self.buffered != BLOCK_BYTES - 8 {
            self.update(&[0x00]);
        }
        self.update(&length_bits.to_be_bytes());
        debug_assert_eq!(self.buffered, 0, "padding must land on a block boundary");

        let mut digest = [0u8; 32];
        for (word, slot) in self.state.iter().zip(digest.chunks_exact_mut(4)) {
            slot.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// Consumes the hasher and returns the digest as a domain [`ContentHash`].
    pub fn content_hash(self) -> ContentHash {
        ContentHash::Sha256(self.finalize())
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

// A hasher's `Debug` must not print internal state as if it were the digest;
// keep it opaque, matching how the rest of the core hides working state.
impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sha256").finish_non_exhaustive()
    }
}

/// The SHA-256 digest of `bytes`, as a domain [`ContentHash`] — the one-shot
/// form of [`Sha256`] for callers that already hold the whole message.
pub fn sha256(bytes: &[u8]) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.content_hash()
}

/// The SHA-256 block compression function (FIPS 180-4 §6.2.2).
fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_BYTES]) {
    let mut w = [0u32; 64];
    for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for t in 16..64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    for t in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(big_s1)
            .wrapping_add(ch)
            .wrapping_add(ROUND_CONSTANTS[t])
            .wrapping_add(w[t]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest of `message` as lowercase hex.
    fn hex(message: &[u8]) -> String {
        let ContentHash::Sha256(digest) = sha256(message);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn empty_message_vector() {
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc_vector() {
        // FIPS 180-4 §D.1.
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn two_block_448_bit_vector() {
        // FIPS 180-4 §D.2 — 448 bits, one byte short of a full block, so the
        // padding spills into a second block.
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn multi_block_896_bit_vector() {
        // FIPS 180-4 §D.3 — 896 bits, forcing a full extra padding block.
        assert_eq!(
            hex(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn one_million_a_vector() {
        // The NIST long message vector: 1,000,000 'a' bytes.
        let message = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&message),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn exact_block_boundary_vector() {
        // 64 bytes: exactly one block of message plus a whole padding block,
        // the case the two-block padding loop in `finalize` must get right.
        let message = vec![0u8; 64];
        assert_eq!(
            hex(&message),
            "f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b"
        );
    }

    #[test]
    fn streaming_matches_one_shot_for_every_split() {
        // The digest must not depend on how the message is chunked: split the
        // same 200-byte message at every offset and compare to the one-shot.
        let message: Vec<u8> = (0..200u32).map(|i| (i * 31 + 7) as u8).collect();
        let ContentHash::Sha256(expected) = sha256(&message);
        for split in 0..=message.len() {
            let mut hasher = Sha256::new();
            hasher.update(&message[..split]);
            hasher.update(&message[split..]);
            assert_eq!(hasher.finalize(), expected, "split at {split}");
        }
    }

    #[test]
    fn many_small_updates_match_one_shot() {
        let message: Vec<u8> = (0..1_000u32).map(|i| (i % 251) as u8).collect();
        let ContentHash::Sha256(expected) = sha256(&message);
        let mut hasher = Sha256::new();
        for byte in &message {
            hasher.update(std::slice::from_ref(byte));
        }
        assert_eq!(hasher.finalize(), expected);
    }
}
