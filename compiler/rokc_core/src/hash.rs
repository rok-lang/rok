//! Hashes for build keys, symbol names and message signing: FNV-1a 64,
//! a fast word-at-a-time 64-bit hash, FIPS 180-4 SHA-256, RFC 2104
//! HMAC-SHA256, lowercase hex.

use std::fmt::Write as _;

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// FNV-1a 64 continuing from `h` (start at `FNV_OFFSET`).
#[must_use]
pub fn fnv_add(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

/// FNV-1a 64 of `bytes`.
#[must_use]
pub fn fnv(bytes: &[u8]) -> u64 {
    fnv_add(FNV_OFFSET, bytes)
}

const MIX: u64 = 0x9e37_79b9_7f4a_7c15;

/// The high and low halves of `a * b` folded together.
#[allow(clippy::cast_possible_truncation)]
const fn mum(a: u64, b: u64) -> u64 {
    let p = (a as u128) * (b as u128);
    (p as u64) ^ ((p >> 64) as u64)
}

/// A 64-bit hash of `bytes`, eight at a time (the incremental caches'
/// keys: fast over megabytes, not cryptographic).
#[must_use]
pub fn fast(bytes: &[u8]) -> u64 {
    fast_add(0x243f_6a88_85a3_08d3, bytes)
}

/// `fast` continuing from `h` (`h` and `bytes` hash as one value).
#[must_use]
pub fn fast_add(h: u64, bytes: &[u8]) -> u64 {
    let mut h = mum(h ^ bytes.len() as u64, MIX);
    let mut words = bytes.chunks_exact(8);
    for w in &mut words {
        let mut b = [0u8; 8];
        b.copy_from_slice(w);
        h = mum(h ^ u64::from_le_bytes(b), MIX);
    }
    let mut b = [0u8; 8];
    b[..words.remainder().len()].copy_from_slice(words.remainder());
    mum(
        mum(h ^ u64::from_le_bytes(b), MIX),
        MIX ^ 0x5851_f42d_4c95_7f2d,
    )
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

// a..h, w, k are the standard's names.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (w, b) in w.iter_mut().zip(block.chunks(4)) {
            *w = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let (a, b) = (w[i - 15], w[i - 2]);
            let s0 = a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3);
            let s1 = b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for (k, w) in K.iter().zip(w) {
            let [a, b, c, d, e, f, g, hh] = v;
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(w);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            v = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        }
        for (h, v) in h.iter_mut().zip(v) {
            *h = h.wrapping_add(v);
        }
    }
    let mut out = [0; 32];
    for (o, h) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&h.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 of the concatenation of `parts`.
#[must_use]
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    for p in parts {
        inner.extend_from_slice(p);
    }
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

const FX_K: u64 = 0xf135_7aea_2e62_a9c5;

/// The compiler's map hasher: multiply-add over words, no random seed.
///
/// A map iterates the same way every run, so work order does not vary
/// between builds. Weak against chosen keys; compiler keys are not.
#[derive(Clone, Copy, Default)]
pub struct FxHasher(u64);

impl FxHasher {
    #[inline]
    const fn add(&mut self, w: u64) {
        self.0 = self.0.wrapping_add(w).wrapping_mul(FX_K);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for w in &mut words {
            let mut b = [0u8; 8];
            b.copy_from_slice(w);
            self.add(u64::from_le_bytes(b));
        }
        let rest = words.remainder();
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.add(i as u64);
        self.add((i >> 64) as u64);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    // hashbrown takes the top 7 bits for its tags and the low bits for the
    // bucket: the rotate brings the well-mixed high bits down to both.
    #[inline]
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }
}

pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;
pub type HashMap<K, V> = std::collections::HashMap<K, V, FxBuild>;
pub type HashSet<T> = std::collections::HashSet<T, FxBuild>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_hash_tells_inputs_apart() {
        let mut seen = crate::HashSet::default();
        // Lengths, tails, a byte moved, a byte changed.
        for n in 0..40usize {
            let v: Vec<u8> = (0..n).map(|i| u8::try_from(i * 7 % 251).unwrap()).collect();
            assert!(seen.insert(fast(&v)), "{n}");
            if n > 0 {
                let mut w = v.clone();
                w[n / 2] ^= 1;
                assert!(seen.insert(fast(&w)), "{n}");
            }
        }
        assert!(seen.insert(fast(&[0; 8])) && seen.insert(fast(&[0; 9])));
        assert_eq!(fast(b"rok"), fast(b"rok"));
        assert_ne!(fast_add(1, b"ab"), fast_add(2, b"ab"));
    }

    #[test]
    fn known_vectors() {
        assert_eq!(fnv(b""), FNV_OFFSET);
        assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // RFC 4231 cases 1, 2 and 6 (key longer than a block).
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], &[b"Hi There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
