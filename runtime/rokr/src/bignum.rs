//! `int` limb kernels (`library/core/src/num/bigint.rok`) over
//! `GcBuf<u32>` limbs (little-endian, base 2^32), computed on machine
//! words: multiplication (schoolbook, Karatsuba, Toom-3, three-prime
//! NTT, squaring variants, products by an operand transformed once,
//! products mod B^n - 1), Knuth D division, single-limb division,
//! Lehmer and half-gcd, add / sub / shifts and the base case of decimal
//! output. The library does the rest (signs, recursive division, radix
//! conversion, allocation). None of these allocate: scratch space comes from the
//! caller. The long ones run parked ([`parked`]).

// Knuth's names (q, r, u, v, m, n) read better than long ones here.
#![allow(clippy::many_single_char_names)]
// `GcBuf` elements start 8-aligned after the header.
#![allow(clippy::cast_ptr_alignment)]

use core::slice;

use rokr_common::object_header::HEADER_SIZE;

// Multiplication runs on machine-word limbs: the `u32` operands are
// packed into `L`s in the scratch buffer and the product unpacked.
#[cfg(target_pointer_width = "64")]
type L = u64;
#[cfg(target_pointer_width = "64")]
type W = u128;
#[cfg(not(target_pointer_width = "64"))]
type L = u32;
#[cfg(not(target_pointer_width = "64"))]
type W = u64;
// Signed `L` / `W` (Lehmer cofactors and their sums).
#[cfg(target_pointer_width = "64")]
type SL = i64;
#[cfg(target_pointer_width = "64")]
type SW = i128;
#[cfg(not(target_pointer_width = "64"))]
type SL = i32;
#[cfg(not(target_pointer_width = "64"))]
type SW = i64;

const LB: u32 = L::BITS;
/// `u32` limbs per `L`.
const PER: usize = (L::BITS / 32) as usize;

/// `L` limbs from which products split (Karatsuba), then Toom-3, then
/// go to the NTT, and the same for squares. Measured on aarch64 (64-bit
/// limbs) in cycles; the curves are flat around these.
const KARA: usize = 32;
const TOOM3: usize = 150;
const KARA_SQR: usize = 32;
const TOOM3_SQR: usize = 200;
const NTT: usize = 700;
const NTT_SQR: usize = 1000;
const NTT_LO: usize = if NTT < NTT_SQR { NTT } else { NTT_SQR };

// `scratch_bound` relies on these (see there).
const _: () = assert!(KARA >= 18 && KARA_SQR >= KARA && TOOM3 >= KARA && TOOM3_SQR >= KARA_SQR);

/// `u32` limbs of work from which a kernel runs parked.
const PARK: usize = 2048;

/// `f()`, parked when `work` reaches `PARK` limbs: like a blocking
/// runtime call, so a collection on another thread does
/// not wait for it. A kernel touches no GC object but the buffers it
/// was passed: the caller's frame keeps them alive (these calls are
/// safepoints) and a collection never moves what a root reaches.
fn parked<R>(work: usize, f: impl FnOnce() -> R) -> R {
    if work < PARK {
        return f();
    }
    rokr_gc::world::enter_native();
    let r = f();
    rokr_gc::world::leave_native();
    r
}

/// The first `n` limbs of the `GcBuf<u32>` at `buf`.
///
/// # Safety
/// `buf` is a live `GcBuf<u32>` of at least `n` elements.
unsafe fn limbs<'a>(buf: *mut u8, n: usize) -> &'a mut [u32] {
    // SAFETY: caller contract; elements start after the header.
    unsafe { slice::from_raw_parts_mut(buf.add(HEADER_SIZE).cast::<u32>(), n) }
}

/// `r += a` over `r` (`a.len() <= r.len()`); the carry out.
fn add_in(r: &mut [L], a: &[L]) -> bool {
    let (lo, hi) = r.split_at_mut(a.len());
    let mut c = 0 as W;
    for (x, &y) in lo.iter_mut().zip(a) {
        let s = *x as W + y as W + c;
        *x = s as L;
        c = s >> LB;
    }
    if c == 0 {
        return false;
    }
    for x in hi {
        *x = x.wrapping_add(1);
        if *x != 0 {
            return false;
        }
    }
    true
}

/// `r -= a` over `r` (`a.len() <= r.len()`); the borrow out.
fn sub_in(r: &mut [L], a: &[L]) -> bool {
    let (lo, hi) = r.split_at_mut(a.len());
    let mut b = false;
    for (x, &y) in lo.iter_mut().zip(a) {
        let (d, b1) = x.overflowing_sub(y);
        let (d, b2) = d.overflowing_sub(L::from(b));
        *x = d;
        b = b1 | b2;
    }
    if !b {
        return false;
    }
    for x in hi {
        let old = *x;
        *x = old.wrapping_sub(1);
        if old != 0 {
            return false;
        }
    }
    true
}

/// `r = -r` (two's complement over `r.len()` limbs).
fn neg_in(r: &mut [L]) {
    let mut c = true;
    for x in r {
        let (v, o) = (!*x).overflowing_add(L::from(c));
        *x = v;
        c = o;
    }
}

/// `r <<= 1`; the top bit must be clear.
fn shl1_in(r: &mut [L]) {
    let mut top = 0;
    for x in r {
        let v = *x;
        *x = (v << 1) | top;
        top = v >> (LB - 1);
    }
    debug_assert!(top == 0);
}

/// `r >>= 1`, arithmetic (two's complement).
fn sar1_in(r: &mut [L]) {
    let n = r.len();
    for i in 0..n - 1 {
        r[i] = (r[i] >> 1) | (r[i + 1] << (LB - 1));
    }
    r[n - 1] = (r[n - 1] >> 1) | (r[n - 1] & (1 << (LB - 1)));
}

/// `r /= 3`, exact (Hensel division, so two's complement values too).
fn divexact3_in(r: &mut [L]) {
    const INV3: L = L::MAX / 3 * 2 + 1;
    let mut c: L = 0;
    for x in r {
        let (s, b) = x.overflowing_sub(c);
        let q = s.wrapping_mul(INV3);
        *x = q;
        c = ((q as W * 3) >> LB) as L + L::from(b);
    }
}

/// `r[..a.len()] += a * x`; the carry limb.
fn addmul_1(r: &mut [L], a: &[L], x: L) -> L {
    let mut c: L = 0;
    for (y, &v) in r.iter_mut().zip(a) {
        // (2^b - 1)^2 + 2 (2^b - 1) < 2^2b.
        let t = v as W * x as W + *y as W + c as W;
        *y = t as L;
        c = (t >> LB) as L;
    }
    c
}

/// `dst = src` zero-extended.
fn copy_ext(dst: &mut [L], src: &[L]) {
    let (lo, hi) = dst.split_at_mut(src.len());
    lo.copy_from_slice(src);
    hi.fill(0);
}

/// `r[off..] += v` where the sum fits `r` (limbs of `v` past the end of
/// `r` are zero).
fn add_at(r: &mut [L], off: usize, v: &[L]) {
    let n = v.len().min(r.len() - off);
    debug_assert!(v[n..].iter().all(|&x| x == 0));
    let c = add_in(&mut r[off..], &v[..n]);
    debug_assert!(!c);
}

/// `r[..a.len()] = a * x`; the carry limb.
fn mul_1(r: &mut [L], a: &[L], x: L) -> L {
    let mut c: L = 0;
    for (y, &v) in r.iter_mut().zip(a) {
        let t = v as W * x as W + c as W;
        *y = t as L;
        c = (t >> LB) as L;
    }
    c
}

/// `r[..a.len()] += a * (x0 + x1 B)`: two rows per pass over `r`; the
/// limbs for `r[a.len()]` and `r[a.len() + 1]`.
fn addmul_2(r: &mut [L], a: &[L], x0: L, x1: L) -> (L, L) {
    // Pending carries into the next two positions.
    let (mut c0, mut c1): (L, L) = (0, 0);
    for (y, &v) in r.iter_mut().zip(a) {
        let t = *y as W + v as W * x0 as W + c0 as W;
        *y = t as L;
        let u = v as W * x1 as W + c1 as W + (t >> LB);
        c0 = u as L;
        c1 = (u >> LB) as L;
    }
    (c0, c1)
}

/// `r = a * b`, `a.len() >= b.len() >= 1`. Row `i` writes `r[i + n]`,
/// so nothing needs clearing first.
fn mul_basecase(r: &mut [L], a: &[L], b: &[L]) {
    let n = a.len();
    r[n] = mul_1(&mut r[..n], a, b[0]);
    let mut i = 1;
    while i + 1 < b.len() {
        (r[i + n], r[i + n + 1]) = addmul_2(&mut r[i..i + n], a, b[i], b[i + 1]);
        i += 2;
    }
    if i < b.len() {
        r[i + n] = addmul_1(&mut r[i..i + n], a, b[i]);
    }
}

/// `r = a^2`: the cross products once, then doubled plus the diagonal
/// in one pass.
fn sqr_basecase(r: &mut [L], a: &[L]) {
    let n = a.len();
    r[0] = 0;
    r[2 * n - 1] = 0;
    if n > 1 {
        r[n] = mul_1(&mut r[1..n], &a[1..], a[0]);
        for i in 1..n - 1 {
            r[i + n] = addmul_1(&mut r[2 * i + 1..i + n], &a[i + 1..], a[i]);
        }
    }
    let mut top: L = 0;
    let mut c = 0 as W;
    for (p, &x) in r.chunks_exact_mut(2).zip(a) {
        let (lo, hi) = (p[0], p[1]);
        let sq = x as W * x as W;
        let s0 = ((lo << 1) | top) as W + (sq as L) as W + c;
        let s1 = ((hi << 1) | (lo >> (LB - 1))) as W + (sq >> LB) + (s0 >> LB);
        top = hi >> (LB - 1);
        p[0] = s0 as L;
        p[1] = s1 as L;
        c = s1 >> LB;
    }
    debug_assert!(c == 0 && top == 0);
}

/// Scratch `L`s that `mul(r, a, b, t)` / `sqr` may use, for operands of
/// `n` and `m` limbs. By induction over the dispatch: a balanced step
/// on `N = max(n, m)` limbs uses at most `4k + 4` (Karatsuba, `k =
/// ceil(N/2)`) or `8k + 8` (Toom-3, `k = ceil(N/3)`) limbs plus its
/// recursion on `k + 1`, which stays within `6N + 40 bits(N)` once
/// `N >= 18`; an unbalanced one uses `2m` plus a balanced `m` product.
const fn scratch_bound(n: usize, m: usize) -> usize {
    let (lo, hi) = if n < m { (n, m) } else { (m, n) };
    if lo < KARA {
        return 0;
    }
    let bits = (usize::BITS - hi.leading_zeros()) as usize;
    // NTT steps anywhere below use at most this on top (the largest
    // transform bounds it for huge operands).
    let ntt = if hi < NTT_LO {
        0
    } else if ntt_len(hi, hi) <= NTT_MAX {
        ntt_scratch(hi, hi)
    } else {
        6 * NTT_MAX / PER
    };
    6 * hi + 40 * bits + ntt
}

/// `r = a * b` (`r.len() == a.len() + b.len()`), scratch `t`.
fn mul(r: &mut [L], a: &[L], b: &[L], t: &mut [L]) {
    let (a, b) = if a.len() < b.len() { (b, a) } else { (a, b) };
    let (n, m) = (a.len(), b.len());
    if m == 0 {
        r.fill(0);
    } else if m < KARA {
        mul_basecase(r, a, b);
    } else if m >= NTT && ntt_len(n, m) <= NTT_MAX {
        ntt_mul(r, a, b, t);
    } else if 2 * m <= n {
        // Unbalanced: `m`-limb pieces of `a`, each against all of `b`.
        r.fill(0);
        let (p, t) = t.split_at_mut(2 * m);
        for (i, piece) in a.chunks(m).enumerate() {
            let p = &mut p[..piece.len() + m];
            mul(p, piece, b, t);
            let c = add_in(&mut r[i * m..], p);
            debug_assert!(!c);
        }
    } else if m < TOOM3 || m <= 2 * n.div_ceil(3) {
        karatsuba(r, a, b, t);
    } else {
        toom3(r, a, b, t);
    }
}

/// `r = a^2` (`r.len() == 2 a.len()`), scratch `t`.
fn sqr(r: &mut [L], a: &[L], t: &mut [L]) {
    let n = a.len();
    if n == 0 {
        r.fill(0);
    } else if n < KARA_SQR {
        sqr_basecase(r, a);
    } else if n < TOOM3_SQR {
        karatsuba_sqr(r, a, t);
    } else if n >= NTT_SQR && ntt_len(n, n) <= NTT_MAX {
        ntt_mul(r, a, &[], t);
    } else {
        toom3_sqr(r, a, t);
    }
}

/// a = a1 B^k + a0, b = b1 B^k + b0 (`n >= m > n / 2`):
/// ab = a1 b1 B^2k + ((a0 + a1)(b0 + b1) - a0 b0 - a1 b1) B^k + a0 b0.
fn karatsuba(r: &mut [L], a: &[L], b: &[L], t: &mut [L]) {
    let k = a.len().div_ceil(2);
    let (a0, a1) = a.split_at(k);
    let (b0, b1) = b.split_at(k);
    {
        let (lo, hi) = r.split_at_mut(2 * k);
        mul(lo, a0, b0, t);
        mul(hi, a1, b1, t);
    }
    let (sa, t) = t.split_at_mut(k + 1);
    let (sb, t) = t.split_at_mut(k + 1);
    let (z, t) = t.split_at_mut(2 * k + 2);
    copy_ext(sa, a0);
    add_in(sa, a1);
    copy_ext(sb, b0);
    add_in(sb, b1);
    mul(z, sa, sb, t);
    sub_in(z, &r[..2 * k]);
    sub_in(z, &r[2 * k..]);
    add_at(r, k, z);
}

fn karatsuba_sqr(r: &mut [L], a: &[L], t: &mut [L]) {
    let k = a.len().div_ceil(2);
    let (a0, a1) = a.split_at(k);
    {
        let (lo, hi) = r.split_at_mut(2 * k);
        sqr(lo, a0, t);
        sqr(hi, a1, t);
    }
    let (sa, t) = t.split_at_mut(k + 1);
    let (z, t) = t.split_at_mut(2 * k + 2);
    copy_ext(sa, a0);
    add_in(sa, a1);
    sqr(z, sa, t);
    sub_in(z, &r[..2 * k]);
    sub_in(z, &r[2 * k..]);
    add_at(r, k, z);
}

/// The three pieces of `a` at `k` limbs (`a2` may be shorter).
const fn split3(a: &[L], k: usize) -> (&[L], &[L], &[L]) {
    let (a0, rest) = a.split_at(k);
    let (a1, a2) = rest.split_at(k);
    (a0, a1, a2)
}

/// `e = a(1)`.
fn eval_1(e: &mut [L], a: (&[L], &[L], &[L])) {
    copy_ext(e, a.0);
    add_in(e, a.1);
    add_in(e, a.2);
}

/// `e = |a(-1)|` (`p` scratch); whether `a(-1) < 0`.
fn eval_m1(e: &mut [L], p: &mut [L], a: (&[L], &[L], &[L])) -> bool {
    copy_ext(p, a.0);
    add_in(p, a.2);
    copy_ext(e, a.1);
    let lt = {
        // p < a1?
        let mut lt = false;
        for (x, y) in p.iter().zip(e.iter()).rev() {
            if x != y {
                lt = x < y;
                break;
            }
        }
        lt
    };
    if lt {
        sub_in(e, p);
    } else {
        neg_in(e);
        add_in(e, p);
    }
    lt
}

/// `e = a(2) = a0 + 2 a1 + 4 a2`.
fn eval_2(e: &mut [L], a: (&[L], &[L], &[L])) {
    copy_ext(e, a.2);
    shl1_in(e);
    add_in(e, a.1);
    shl1_in(e);
    add_in(e, a.0);
}

/// Toom-3 interpolation from the point values at 0 (`r[..2k]`), 1
/// (`v1`), -1 (`vm1`), 2 (`v2`) and infinity (`r[4k..]`; the middle of
/// `r` zero), each `2k + 2`-limb two's complement; adds the inner
/// coefficients c1..c3 into `r`.
fn toom3_interpolate(r: &mut [L], k: usize, v1: &mut [L], vm1: &mut [L], v2: &mut [L]) {
    let (v0, rest) = r.split_at(2 * k);
    let vinf = &rest[2 * k..];
    // v2 = (v2 - vm1) / 3 = c1 + c2 + 3 c3 + 5 c4
    sub_in(v2, vm1);
    divexact3_in(v2);
    // vm1 = (v1 - vm1) / 2 = c1 + c3
    neg_in(vm1);
    add_in(vm1, v1);
    sar1_in(vm1);
    // v1 = v1 - v0 = c1 + c2 + c3 + c4
    sub_in(v1, v0);
    // v2 = (v2 - v1) / 2 - 2 c4 = c3
    sub_in(v2, v1);
    sar1_in(v2);
    sub_in(v2, vinf);
    sub_in(v2, vinf);
    // v1 = v1 - vm1 - c4 = c2
    sub_in(v1, vm1);
    sub_in(v1, vinf);
    // vm1 = vm1 - c3 = c1
    sub_in(vm1, v2);
    add_at(r, k, vm1);
    add_at(r, 2 * k, v1);
    add_at(r, 3 * k, v2);
}

/// Toom-3 on pieces of `k = ceil(n/3)` limbs (`m > 2k`).
fn toom3(r: &mut [L], a: &[L], b: &[L], t: &mut [L]) {
    let k = a.len().div_ceil(3);
    let a3 = split3(a, k);
    let b3 = split3(b, k);
    {
        let (lo, rest) = r.split_at_mut(2 * k);
        let (mid, hi) = rest.split_at_mut(2 * k);
        mul(lo, a3.0, b3.0, t);
        mul(hi, a3.2, b3.2, t);
        mid.fill(0);
    }
    let w = 2 * k + 2;
    let (v1, t) = t.split_at_mut(w);
    let (vm1, t) = t.split_at_mut(w);
    let (v2, t) = t.split_at_mut(w);
    let (p, t) = t.split_at_mut(k + 1);
    let (ea, t) = t.split_at_mut(k + 1);
    let (eb, t) = t.split_at_mut(k + 1);
    eval_1(ea, a3);
    eval_1(eb, b3);
    mul(v1, ea, eb, t);
    let na = eval_m1(ea, p, a3);
    let nb = eval_m1(eb, p, b3);
    mul(vm1, ea, eb, t);
    if na != nb {
        neg_in(vm1);
    }
    eval_2(ea, a3);
    eval_2(eb, b3);
    mul(v2, ea, eb, t);
    toom3_interpolate(r, k, v1, vm1, v2);
}

fn toom3_sqr(r: &mut [L], a: &[L], t: &mut [L]) {
    let k = a.len().div_ceil(3);
    let a3 = split3(a, k);
    {
        let (lo, rest) = r.split_at_mut(2 * k);
        let (mid, hi) = rest.split_at_mut(2 * k);
        sqr(lo, a3.0, t);
        sqr(hi, a3.2, t);
        mid.fill(0);
    }
    let w = 2 * k + 2;
    let (v1, t) = t.split_at_mut(w);
    let (vm1, t) = t.split_at_mut(w);
    let (v2, t) = t.split_at_mut(w);
    let (p, t) = t.split_at_mut(k + 1);
    let (ea, t) = t.split_at_mut(k + 1);
    eval_1(ea, a3);
    sqr(v1, ea, t);
    eval_m1(ea, p, a3);
    sqr(vm1, ea, t);
    eval_2(ea, a3);
    sqr(v2, ea, t);
    toom3_interpolate(r, k, v1, vm1, v2);
}

/// `L`s holding `n` `u32` limbs.
const fn packed(n: usize) -> usize {
    n.div_ceil(PER)
}

//
// The `u32` digits of both operands are coefficients; their cyclic
// convolution is computed modulo three primes p < 2^30 with 2^23 | p - 1
// (Montgomery arithmetic, R = 2^32) and put back together by Garner's
// CRT. A coefficient of the product is below min(len) 2^64 <= 2^86 <
// p1 p2 p3 (2^88.2), so transform lengths up to 2^23 are exact. The
// butterflies keep values in [0, 2p) (Harvey's lazy reduction, which
// needs 4p < 2^32).

/// The primes.
const P1: u32 = 998_244_353;
const P2: u32 = 754_974_721;
const P3: u32 = 469_762_049;
/// The longest transform (`u32` coefficients).
const NTT_MAX: usize = 1 << 23;

const fn pow_mod(b: u64, e: u64, p: u64) -> u64 {
    let (mut b, mut e, mut r) = (b % p, e, 1u64);
    while e > 0 {
        if e & 1 == 1 {
            r = r * b % p;
        }
        b = b * b % p;
        e >>= 1;
    }
    r
}

/// Montgomery arithmetic modulo the prime `P` (R = 2^32).
struct Mont<const P: u32>;

impl<const P: u32> Mont<P> {
    /// -P^-1 mod 2^32 (Newton: x (2 - P x) doubles the correct bits).
    const PINV: u32 = {
        assert!(P < 1 << 30);
        let mut x = 1u32;
        let mut i = 0;
        while i < 5 {
            x = x.wrapping_mul(2u32.wrapping_sub(P.wrapping_mul(x)));
            i += 1;
        }
        x.wrapping_neg()
    };

    /// a b / R mod P, in [0, 2P) for `a b < 2^32 P` (`a < 4P, b < P`, or
    /// both below 2P).
    #[inline]
    const fn mul(a: u32, b: u32) -> u32 {
        let t = a as u64 * b as u64;
        // The low half of `t`, as 32-bit products (vectorizes).
        let m = a.wrapping_mul(b).wrapping_mul(Self::PINV);
        ((t + m as u64 * P as u64) >> 32) as u32
    }

    /// [0, 2k P) to [0, k P) (`k` = 1, 2, 4; `4 P < 2^32`): wraps unless
    /// `x >= k P`.
    #[inline]
    const fn fold<const K: u32>(x: u32) -> u32 {
        let y = x.wrapping_sub(K * P);
        if y < x { y } else { x }
    }

    /// `x R mod P`.
    const fn to_mont(x: u64) -> u32 {
        (((x % P as u64) << 32) % P as u64) as u32
    }

    /// Root tables for length `n`: `rt[h + j] = w_2h^j` for each half
    /// length `h` (Montgomery form, below P), `irt` the same for the
    /// inverses.
    fn roots(g: u32, n: usize, rt: &mut [u32], irt: &mut [u32]) {
        // The first `K` powers in sequence, then each from the one `K`
        // back: independent products instead of one chain of `h`.
        const K: usize = 16;
        let p = u64::from(P);
        let w = pow_mod(u64::from(g), (p - 1) / n as u64, p);
        let wi = pow_mod(w, p - 2, p);
        let h = n / 2;
        let (mut x, mut y) = (Self::to_mont(1), Self::to_mont(1));
        let (wm, wim) = (Self::to_mont(w), Self::to_mont(wi));
        for j in 0..h.min(K) {
            rt[h + j] = x;
            irt[h + j] = y;
            x = Self::fold::<1>(Self::mul(x, wm));
            y = Self::fold::<1>(Self::mul(y, wim));
        }
        for j in K..h {
            rt[h + j] = Self::fold::<1>(Self::mul(rt[h + j - K], x));
            irt[h + j] = Self::fold::<1>(Self::mul(irt[h + j - K], y));
        }
        let mut h = h / 2;
        while h >= 1 {
            for j in 0..h {
                rt[h + j] = rt[2 * h + 2 * j];
                irt[h + j] = irt[2 * h + 2 * j];
            }
            h /= 2;
        }
    }

    /// Forward transform (decimation in frequency, length >= 4): natural
    /// order in, bit-reversed out; values in [0, 2P) both ends.
    fn forward(a: &mut [u32], rt: &[u32]) {
        let mut h = a.len() / 2;
        while h >= 4 {
            let w = &rt[h..2 * h];
            for blk in a.chunks_exact_mut(2 * h) {
                let (x, y) = blk.split_at_mut(h);
                for ((u, v), &w) in x.iter_mut().zip(y.iter_mut()).zip(w) {
                    let (s, d) = (*u, *v);
                    *u = Self::fold::<2>(s + d);
                    *v = Self::mul(s + 2 * P - d, w);
                }
            }
            h /= 2;
        }
        // h = 2 and 1 together; the twiddles are 1 but for w_4.
        let w4 = rt[3];
        for p in a.chunks_exact_mut(4) {
            let (y0, y2) = (
                Self::fold::<2>(p[0] + p[2]),
                Self::fold::<2>(p[0] + 2 * P - p[2]),
            );
            let (y1, y3) = (
                Self::fold::<2>(p[1] + p[3]),
                Self::mul(p[1] + 2 * P - p[3], w4),
            );
            p[0] = Self::fold::<2>(y0 + y1);
            p[1] = Self::fold::<2>(y0 + 2 * P - y1);
            p[2] = Self::fold::<2>(y2 + y3);
            p[3] = Self::fold::<2>(y2 + 2 * P - y3);
        }
    }

    /// Inverse transform without the 1/n (decimation in time, length >=
    /// 4): bit-reversed in, natural order out; values in [0, 2P) both
    /// ends.
    fn inverse(a: &mut [u32], irt: &[u32]) {
        let n = a.len();
        // h = 1 and 2 together; the twiddles are 1 but for w_4^-1.
        let w4 = irt[3];
        for p in a.chunks_exact_mut(4) {
            let (y0, y1) = (
                Self::fold::<2>(p[0] + p[1]),
                Self::fold::<2>(p[0] + 2 * P - p[1]),
            );
            let (y2, y3) = (
                Self::fold::<2>(p[2] + p[3]),
                Self::fold::<2>(p[2] + 2 * P - p[3]),
            );
            let t = Self::mul(y3, w4);
            p[0] = Self::fold::<2>(y0 + y2);
            p[2] = Self::fold::<2>(y0 + 2 * P - y2);
            p[1] = Self::fold::<2>(y1 + t);
            p[3] = Self::fold::<2>(y1 + 2 * P - t);
        }
        let mut h = 4;
        while h < n {
            let w = &irt[h..2 * h];
            for blk in a.chunks_exact_mut(2 * h) {
                let (x, y) = blk.split_at_mut(h);
                for ((u, v), &w) in x.iter_mut().zip(y.iter_mut()).zip(w) {
                    let (s, d) = (*u, Self::mul(*v, w));
                    *u = Self::fold::<2>(s + d);
                    *v = Self::fold::<2>(s + 2 * P - d);
                }
            }
            h *= 2;
        }
    }

    /// `f = d mod P`, zero-extended.
    fn load(f: &mut [u32], d: &[u32]) {
        for (x, &v) in f.iter_mut().zip(d) {
            *x = v % P;
        }
        f[d.len()..].fill(0);
    }

    /// `f = f g` pointwise (`g` empty: `f^2`).
    fn pointwise(f: &mut [u32], g: &[u32]) {
        if g.is_empty() {
            for x in f.iter_mut() {
                *x = Self::mul(*x, *x);
            }
        } else {
            for (x, &y) in f.iter_mut().zip(g) {
                *x = Self::mul(*x, y);
            }
        }
    }

    /// The factor that undoes the 1/R of the pointwise products and the
    /// factor `n` of the transforms (applied in the CRT pass).
    const fn scale(n: usize) -> u32 {
        let p = P as u64;
        let r2 = (1u64 << 32) % p;
        (r2 * r2 % p * pow_mod(n as u64, p - 2, p) % p) as u32
    }
}

/// Primitive roots of P1, P2, P3.
const G: [u32; 3] = [3, 11, 3];

/// The `u32` limbs of `res` (three residue vectors of `n`, in [0, 2p),
/// each times its `s`) put back together (Garner: x = x1 + p1 t2 + p1
/// p2 t3) and carried into `out`; the carry out of its top. `signed`:
/// coefficients past p1 p2 p3 / 2 are negative, `out` is then two's
/// complement and the carry its sign. `res` is overwritten.
fn garner(out: &mut [u32], res: &mut [u32], n: usize, s: [u32; 3], signed: bool) -> i128 {
    let (r1, rest) = res.split_at_mut(n);
    let (r2, r3) = rest.split_at_mut(n);
    let (p1, p2, p3) = (u64::from(P1), u64::from(P2), u64::from(P3));
    let i12 = const { pow_mod(P1 as u64, P2 as u64 - 2, P2 as u64) };
    let i123 = const { pow_mod(P1 as u64 * P2 as u64 % P3 as u64, P3 as u64 - 2, P3 as u64) };
    // t2 = (x2 - x1) i12, t3 = (x3 - x1 - p1 t2) i123 as Montgomery
    // products, the scales folded into the constants.
    let c2a = (u64::from(s[1]) * i12 % p2) as u32;
    let c2b = Mont::<P2>::to_mont(i12);
    let c3a = (u64::from(s[2]) * i123 % p3) as u32;
    let c3b = Mont::<P3>::to_mont(i123);
    let c3c = Mont::<P3>::to_mont(p1 % p3 * i123 % p3);
    let len = out.len();
    for ((a, b), c) in r1[..len].iter_mut().zip(&mut r2[..len]).zip(&mut r3[..len]) {
        let x1 = Mont::<P1>::fold::<1>(Mont::<P1>::mul(*a, s[0]));
        let u2 = Mont::<P2>::mul(*b, c2a) + 2 * P2 - Mont::<P2>::mul(x1, c2b);
        let t2 = Mont::<P2>::fold::<1>(Mont::<P2>::fold::<2>(u2));
        let u3 =
            Mont::<P3>::mul(*c, c3a) + 4 * P3 - Mont::<P3>::mul(x1, c3b) - Mont::<P3>::mul(t2, c3c);
        let t3 = Mont::<P3>::fold::<1>(Mont::<P3>::fold::<2>(Mont::<P3>::fold::<4>(u3)));
        (*a, *b, *c) = (x1, t2, t3);
    }
    let pp = u128::from(p1 * p2) * u128::from(p3);
    let mut carry = 0i128;
    for (i, o) in out.iter_mut().enumerate() {
        let x12 = u64::from(r1[i]) + p1 * u64::from(r2[i]);
        let x = x12 as u128 + (p1 * p2) as u128 * u128::from(r3[i]);
        let x = if signed && x > pp / 2 {
            x as i128 - pp as i128
        } else {
            x as i128
        };
        let v = carry + x;
        *o = v as u32;
        carry = v >> 32;
    }
    carry
}

/// `r += c` mod `B^r.len() - 1` (`B = 2^32`; a carry off the top comes
/// back in at the bottom), with `B^r.len() - 1` itself made 0.
fn wrap_in(r: &mut [u32], mut c: u128) {
    while c != 0 {
        for x in r.iter_mut() {
            let v = u128::from(*x) + c;
            *x = v as u32;
            c = v >> 32;
            if c == 0 {
                break;
            }
        }
    }
    if r.iter().all(|&x| x == u32::MAX) {
        r.fill(0);
    }
}

/// Root tables for transforms up to length `n`: per prime `rt` then
/// `irt`, `n` each. `rt[h + j]` depends on `h` only, so a table serves
/// every shorter transform.
fn ntt_roots(w: &mut [u32], n: usize) {
    let (w1, rest) = w.split_at_mut(2 * n);
    let (w2, w3) = rest.split_at_mut(2 * n);
    let (a, b) = w1.split_at_mut(n);
    Mont::<P1>::roots(G[0], n, a, b);
    let (a, b) = w2.split_at_mut(n);
    Mont::<P2>::roots(G[1], n, a, b);
    let (a, b) = w3[..2 * n].split_at_mut(n);
    Mont::<P3>::roots(G[2], n, a, b);
}

/// The forward transforms of `d` into `f` (`n` per prime), roots `w`
/// (`ntt_roots` for at least `n`, of `wn`).
fn ntt_forward(f: &mut [u32], n: usize, d: &[u32], w: &[u32], wn: usize) {
    let (f1, rest) = f.split_at_mut(n);
    let (f2, f3) = rest.split_at_mut(n);
    Mont::<P1>::load(f1, d);
    Mont::<P1>::forward(f1, &w[..wn]);
    Mont::<P2>::load(f2, d);
    Mont::<P2>::forward(f2, &w[2 * wn..3 * wn]);
    Mont::<P3>::load(&mut f3[..n], d);
    Mont::<P3>::forward(&mut f3[..n], &w[4 * wn..5 * wn]);
}

/// `res = ntt_forward(d) * g` pointwise, inverted (`g` the transforms of
/// the other operand, empty to square `d`); the CRT factors.
fn ntt_finish(res: &mut [u32], n: usize, g: &[u32], w: &[u32], wn: usize) -> [u32; 3] {
    let (f1, rest) = res.split_at_mut(n);
    let (f2, f3) = rest.split_at_mut(n);
    let f3 = &mut f3[..n];
    let gi = |i: usize| {
        if g.is_empty() {
            g
        } else {
            &g[i * n..(i + 1) * n]
        }
    };
    Mont::<P1>::pointwise(f1, gi(0));
    Mont::<P1>::inverse(f1, &w[wn..2 * wn]);
    Mont::<P2>::pointwise(f2, gi(1));
    Mont::<P2>::inverse(f2, &w[3 * wn..4 * wn]);
    Mont::<P3>::pointwise(f3, gi(2));
    Mont::<P3>::inverse(f3, &w[5 * wn..6 * wn]);
    [
        Mont::<P1>::scale(n),
        Mont::<P2>::scale(n),
        Mont::<P3>::scale(n),
    ]
}

/// The `u32` digits of `a`.
const fn digits32(a: &[L]) -> &[u32] {
    // SAFETY: `L` is `PER` little-endian `u32`s (see `pack`).
    unsafe { slice::from_raw_parts(a.as_ptr().cast::<u32>(), a.len() * PER) }
}

/// Transform length (`u32` coefficients) for an `n` × `m` product.
const fn ntt_len(n: usize, m: usize) -> usize {
    ((n + m) * PER).next_power_of_two()
}

/// Scratch `L`s `ntt_mul` uses.
const fn ntt_scratch(n: usize, m: usize) -> usize {
    6 * ntt_len(n, m) / PER
}

/// `f` = the cyclic convolution of `da` and `db` (`db` empty: `da` with
/// itself) mod P, in [0, 2P); `fb`, `rt`, `irt` scratch of `f.len()`.
fn convolve<const P: u32>(
    g: u32,
    f: &mut [u32],
    fb: &mut [u32],
    rt: &mut [u32],
    irt: &mut [u32],
    da: &[u32],
    db: &[u32],
) {
    Mont::<P>::roots(g, f.len(), rt, irt);
    Mont::<P>::load(f, da);
    Mont::<P>::forward(f, rt);
    if db.is_empty() {
        Mont::<P>::pointwise(f, &[]);
    } else {
        Mont::<P>::load(fb, db);
        Mont::<P>::forward(fb, rt);
        Mont::<P>::pointwise(f, fb);
    }
    Mont::<P>::inverse(f, irt);
}

/// A product sum `x[a] x[b] + x[c] x[d]` (`-` when `neg`).
type Term = (usize, usize, usize, usize, bool);

/// `out[k] = terms[k]` over operands `x` (`u32` digits): each operand
/// transformed once per prime (length `n`, at least the longest
/// result), the sums taken pointwise; the signs of the results (a
/// negative one is left as two's complement). `t` holds `(x.len() + 3
/// terms.len() + 2) n` `u32`s.
fn ntt_dot(
    out: &mut [&mut [u32]],
    x: &[&[u32]],
    terms: &[Term],
    n: usize,
    t: &mut [u32],
) -> [bool; 4] {
    let (f, t) = t.split_at_mut(x.len() * n);
    let (res, t) = t.split_at_mut(3 * terms.len() * n);
    let (rt, irt) = t[..2 * n].split_at_mut(n);
    dot_prime::<P1>(G[0], f, res, 0, rt, irt, x, terms, n);
    dot_prime::<P2>(G[1], f, res, 1, rt, irt, x, terms, n);
    dot_prime::<P3>(G[2], f, res, 2, rt, irt, x, terms, n);
    let s = [
        Mont::<P1>::scale(n),
        Mont::<P2>::scale(n),
        Mont::<P3>::scale(n),
    ];
    let mut neg = [false; 4];
    for (k, (o, r)) in out.iter_mut().zip(res.chunks_exact_mut(3 * n)).enumerate() {
        let c = garner(o, r, n, s, terms[k].4);
        debug_assert!(c == 0 || c == -1);
        neg[k] = c < 0;
    }
    neg
}

/// `ntt_dot` modulo `P`: residues `which` of each term in `res`.
#[allow(clippy::too_many_arguments)]
fn dot_prime<const P: u32>(
    g: u32,
    f: &mut [u32],
    res: &mut [u32],
    which: usize,
    rt: &mut [u32],
    irt: &mut [u32],
    x: &[&[u32]],
    terms: &[Term],
    n: usize,
) {
    Mont::<P>::roots(g, n, rt, irt);
    for (fi, xi) in f.chunks_exact_mut(n).zip(x) {
        Mont::<P>::load(fi, xi);
        Mont::<P>::forward(fi, rt);
    }
    for (k, &(a, b, c, d, minus)) in terms.iter().enumerate() {
        let o = &mut res[(3 * k + which) * n..(3 * k + which + 1) * n];
        let fi = |i: usize| &f[i * n..(i + 1) * n];
        let (fa, fb, fc, fd) = (fi(a), fi(b), fi(c), fi(d));
        for ((v, (&xa, &xb)), (&xc, &xd)) in
            o.iter_mut().zip(fa.iter().zip(fb)).zip(fc.iter().zip(fd))
        {
            let u = Mont::<P>::mul(xa, xb);
            let w = Mont::<P>::mul(xc, xd);
            *v = Mont::<P>::fold::<2>(if minus { u + 2 * P - w } else { u + w });
        }
        Mont::<P>::inverse(o, irt);
    }
}

/// The transform length `ntt_dot` takes for results of `total` limbs
/// from operands of at least `lo`, or none.
const fn dot_len(lo: usize, total: usize) -> Option<usize> {
    let n = (total * PER).next_power_of_two();
    if lo >= NTT / 2 && n <= NTT_MAX {
        Some(n)
    } else {
        None
    }
}

/// `L`s as `u32` digits, mutable.
fn digits32_mut(a: &mut [L]) -> &mut [u32] {
    // SAFETY: as in `digits32`.
    unsafe { slice::from_raw_parts_mut(a.as_mut_ptr().cast::<u32>(), a.len() * PER) }
}

/// `r = a * b` by the NTT (`b` empty squares `a`), scratch `t`: one
/// prime at a time.
fn ntt_mul(r: &mut [L], a: &[L], b: &[L], t: &mut [L]) {
    let n = ntt_len(a.len(), if b.is_empty() { a.len() } else { b.len() });
    let (da, db) = (digits32(a), digits32(b));
    // SAFETY: `t` is `L`s; any bit pattern is a `u32`.
    let t = unsafe { slice::from_raw_parts_mut(t.as_mut_ptr().cast::<u32>(), t.len() * PER) };
    let (res, t) = t.split_at_mut(3 * n);
    let (fb, t) = t.split_at_mut(n);
    let (rt, t) = t.split_at_mut(n);
    let irt = &mut t[..n];
    let (r1, rest) = res.split_at_mut(n);
    let (r2, r3) = rest.split_at_mut(n);
    convolve::<P1>(G[0], r1, fb, rt, irt, da, db);
    convolve::<P2>(G[1], r2, fb, rt, irt, da, db);
    convolve::<P3>(G[2], r3, fb, rt, irt, da, db);
    // SAFETY: as `t` above.
    let out = unsafe { slice::from_raw_parts_mut(r.as_mut_ptr().cast::<u32>(), r.len() * PER) };
    let s = [
        Mont::<P1>::scale(n),
        Mont::<P2>::scale(n),
        Mont::<P3>::scale(n),
    ];
    let c = garner(out, res, n, s, false);
    debug_assert!(c == 0);
}

/// The transform length `rok_nat_mul` takes for an `an` × `bn` product
/// (`u32` limbs), 0 when it does not use the NTT.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_ntt_len(an: usize, bn: usize) -> usize {
    let (na, nb) = (packed(an), packed(bn));
    let lo = if na < nb { na } else { nb };
    let n = ntt_len(na, nb);
    if lo >= NTT && n <= NTT_MAX { n } else { 0 }
}

/// `w[0..6 n]` = root tables for transforms up to length `n` (a power of
/// two, at most what `rok_nat_ntt_len` returns).
///
/// # Safety
/// A live `GcBuf<u32>` of at least `6 n` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_ntt_roots(w: *mut u8, n: usize) {
    assert!(n.is_power_of_two() && n <= NTT_MAX);
    // SAFETY: caller contract.
    let w = unsafe { limbs(w, 6 * n) };
    parked(n, || ntt_roots(w, n));
}

/// `f[0..3 n]` = the transforms of `b[0..bn]` for products of up to `n`
/// limbs, roots `w` from `rok_nat_ntt_roots(w, wn)`, `wn >= n`.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `f` distinct from the
/// others.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_ntt_prep(
    f: *mut u8,
    b: *mut u8,
    bn: usize,
    n: usize,
    w: *mut u8,
    wn: usize,
) {
    assert!(n.is_power_of_two() && bn <= n && n <= wn);
    // SAFETY: caller contract; `b` and `w` only read.
    let (f, b, w) = unsafe { (limbs(f, 3 * n), &*limbs(b, bn), &*limbs(w, 6 * wn)) };
    parked(n, || ntt_forward(f, n, b, w, wn));
}

/// `r = a * b mod (B^n - 1)` in `min(an + bn, n)` limbs (`B = 2^32`;
/// the exact product when `an + bn <= n`) for `b` prepared in `f`
/// (`rok_nat_ntt_prep`, length `n >= an, bn`, roots `w` of `wn`); `t`
/// holds `3 n` limbs.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `r` and `t` distinct
/// from each other and from the rest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_ntt_mul(
    r: *mut u8,
    a: *mut u8,
    an: usize,
    f: *mut u8,
    bn: usize,
    n: usize,
    w: *mut u8,
    wn: usize,
    t: *mut u8,
) {
    assert!(an <= n && bn <= n && n <= wn);
    let wrap = an + bn > n;
    // SAFETY: caller contract; `a`, `f` and `w` only read.
    let (r, a, f, w, t) = unsafe {
        (
            limbs(r, if wrap { n } else { an + bn }),
            &*limbs(a, an),
            &*limbs(f, 3 * n),
            &*limbs(w, 6 * wn),
            limbs(t, 3 * n),
        )
    };
    parked(n, || {
        ntt_forward(t, n, a, w, wn);
        let s = ntt_finish(t, n, f, w, wn);
        let c = garner(r, t, n, s, false);
        if wrap {
            wrap_in(r, c as u128);
        }
        debug_assert!(wrap || c == 0);
    });
}

/// Whether `rok_nat_mulmod` takes an `an` × `bn` product mod `B^m - 1`
/// as one cyclic transform of length `m`.
const fn wrap_ntt(an: usize, bn: usize, m: usize) -> bool {
    let lo = if an < bn { an } else { bn };
    m.is_power_of_two() && m <= NTT_MAX && an <= m && bn <= m && packed(lo) >= NTT / 2
}

/// The modulus length `m >= n` (`B^m - 1`) `rok_nat_mulmod` serves best
/// for an `an` × `bn` product: a transform length, or `n` itself.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_mulmod_len(an: usize, bn: usize, n: usize) -> usize {
    let m = n.next_power_of_two();
    if wrap_ntt(an, bn, m) { m } else { n }
}

/// `u32` limbs of scratch `rok_nat_mulmod` needs.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_mulmod_scratch(an: usize, bn: usize, m: usize) -> usize {
    if wrap_ntt(an, bn, m) {
        6 * m
    } else {
        packed(an + bn) * PER + rok_nat_mul_scratch(an, bn)
    }
}

/// `r[0..m] = a * b mod (B^m - 1)` (`B = 2^32`): a cyclic convolution
/// when `m` is from `rok_nat_mulmod_len`, else the product folded; `t`
/// holds `rok_nat_mulmod_scratch(an, bn, m)` limbs. `a` and `b` may be
/// the same buffer (`an == bn` squares).
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `r` and `t` distinct
/// from each other and from `a` / `b`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_mulmod(
    r: *mut u8,
    a: *mut u8,
    an: usize,
    b: *mut u8,
    bn: usize,
    m: usize,
    t: *mut u8,
) {
    let square = a == b && an == bn;
    let tn = rok_nat_mulmod_scratch(an, bn, m);
    // SAFETY: caller contract; `a` / `b` only read; `t` is 8-aligned.
    let (a, b, r, t) = unsafe { (&*limbs(a, an), &*limbs(b, bn), limbs(r, m), limbs(t, tn)) };
    parked(an + bn, || {
        if wrap_ntt(an, bn, m) {
            let (res, rest) = t.split_at_mut(3 * m);
            let (fb, rest) = rest.split_at_mut(m);
            let (rt, irt) = rest.split_at_mut(m);
            let db = if square { &[][..] } else { b };
            let (r1, rest) = res.split_at_mut(m);
            let (r2, r3) = rest.split_at_mut(m);
            convolve::<P1>(G[0], r1, fb, rt, irt, a, db);
            convolve::<P2>(G[1], r2, fb, rt, irt, a, db);
            convolve::<P3>(G[2], r3, fb, rt, irt, a, db);
            let s = [
                Mont::<P1>::scale(m),
                Mont::<P2>::scale(m),
                Mont::<P3>::scale(m),
            ];
            let c = garner(r, res, m, s, false);
            wrap_in(r, c as u128);
        } else {
            let (p, t) = t.split_at_mut(packed(an + bn) * PER);
            let p = &mut p[..an + bn];
            // SAFETY: `t` starts on an `L` boundary.
            let t = unsafe { slice::from_raw_parts_mut(t.as_mut_ptr().cast::<L>(), t.len() / PER) };
            nat_mul(p, a, b, square, t);
            r.fill(0);
            for chunk in p.chunks(m) {
                let mut c = 0u64;
                for (x, &v) in r.iter_mut().zip(chunk) {
                    let s = u64::from(*x) + u64::from(v) + c;
                    *x = s as u32;
                    c = s >> 32;
                }
                for x in &mut r[chunk.len()..] {
                    if c == 0 {
                        break;
                    }
                    let s = u64::from(*x) + c;
                    *x = s as u32;
                    c = s >> 32;
                }
                wrap_in(r, u128::from(c));
            }
            wrap_in(r, 0);
        }
    });
}

// `PER` consecutive `u32` limbs are one `L` in memory.
const _: () = assert!(cfg!(target_endian = "little"));

/// `dst = src` (`dst.len() == packed(src.len())`).
fn pack(dst: &mut [L], src: &[u32]) {
    dst[dst.len() - 1] = 0;
    // SAFETY: `dst` holds at least `src.len()` `u32`s; distinct buffers.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr().cast::<u32>(), src.len());
    };
}

/// `dst = src` truncated to `dst.len()` `u32`s.
fn unpack(dst: &mut [u32], src: &[L]) {
    assert!(dst.len() <= src.len() * PER);
    // SAFETY: as in `pack`.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr().cast::<u32>(), dst.as_mut_ptr(), dst.len());
    };
}

/// `u32` limbs of scratch `rok_nat_mul` needs for an `an` × `bn` product.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_mul_scratch(an: usize, bn: usize) -> usize {
    let (na, nb) = (packed(an), packed(bn));
    (2 * (na + nb) + scratch_bound(na, nb)) * PER
}

/// `r[0..an + bn] = a * b`; `t` holds `rok_nat_mul_scratch(an, bn)`
/// limbs. `a` and `b` may be the same buffer (`an == bn` squares).
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `r` and `t` distinct
/// from each other and from `a` / `b`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_mul(
    r: *mut u8,
    a: *mut u8,
    an: usize,
    b: *mut u8,
    bn: usize,
    t: *mut u8,
) {
    let square = a == b && an == bn;
    let tn = rok_nat_mul_scratch(an, bn) / PER;
    // SAFETY: caller contract; `a` / `b` only read; elements are 8-aligned.
    let (a, b, r) = unsafe { (&*limbs(a, an), &*limbs(b, bn), limbs(r, an + bn)) };
    // SAFETY: as above; `t` is its own buffer of `tn * PER` limbs.
    let t = unsafe { slice::from_raw_parts_mut(t.add(HEADER_SIZE).cast::<L>(), tn) };
    parked(an + bn, || nat_mul(r, a, b, square, t));
}

/// `r = a * b` (`a` squared when `square`) on packed copies in `t`
/// (`rok_nat_mul_scratch` `u32`s).
fn nat_mul(r: &mut [u32], a: &[u32], b: &[u32], square: bool, t: &mut [L]) {
    let (na, nb) = (packed(a.len()), packed(b.len()));
    let (pa, t) = t.split_at_mut(na);
    let (pb, t) = t.split_at_mut(nb);
    let (pr, t) = t.split_at_mut(na + nb);
    pack(pa, a);
    if square {
        sqr(pr, pa, t);
    } else {
        pack(pb, b);
        mul(pr, pa, pb, t);
    }
    unpack(r, pr);
}

/// `q[0..n] = a / v`; the remainder.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least `n` elements; `v != 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_divmod_small(q: *mut u8, a: *mut u8, n: usize, v: u32) -> u32 {
    // SAFETY: caller contract (`q` may be `a`: each limb is read first).
    let (q, a) = unsafe { (limbs(q, n), limbs(a, n)) };
    let d = u64::from(v);
    let mut rem = 0u64;
    for i in (0..n).rev() {
        let cur = (rem << 32) | u64::from(a[i]);
        q[i] = (cur / d) as u32;
        rem = cur % d;
    }
    rem as u32
}

/// Writes the digits of `a[0..n]` (`n <= 64`) in base `radix` ending at
/// `out[end]` (exclusive), zero-padded to `width`; the start. Chunks of
/// the largest power of `radix` in an `L`, by single-limb division with
/// a reciprocal.
///
/// # Safety
/// Live `GcBuf`s: `out` (`u8`) of at least `end` bytes with room for the
/// digits, `a` (`u32`) of at least `n` limbs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_put_digits(
    out: *mut u8,
    end: usize,
    a: *mut u8,
    n: usize,
    width: usize,
    radix: u32,
) -> usize {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    // SAFETY: caller contract.
    let (out, a) = unsafe {
        (
            slice::from_raw_parts_mut(out.add(HEADER_SIZE), end),
            &*limbs(a, n),
        )
    };
    let mut buf = [0 as L; 64 / PER];
    let mut wn = packed(n);
    if wn > 0 {
        pack(&mut buf[..wn], a);
    }
    let rx = L::from(radix);
    let (mut chunk, mut per) = (rx, 1usize);
    while let Some(c) = chunk.checked_mul(rx) {
        chunk = c;
        per += 1;
    }
    let s = chunk.leading_zeros();
    let dn = chunk << s;
    let inv = reciprocal(dn);
    let mut at = end;
    while wn > 0 && buf[wn - 1] == 0 {
        wn -= 1;
    }
    while wn > 0 {
        let mut rem: L = 0;
        for x in buf[..wn].iter_mut().rev() {
            // (rem B + x) / chunk as (that << s) / (chunk << s).
            let n1 = if s == 0 {
                rem
            } else {
                (rem << s) | (*x >> (LB - s))
            };
            let (q, r) = div2by1(n1, *x << s, dn, inv);
            *x = q;
            rem = r >> s;
        }
        while wn > 0 && buf[wn - 1] == 0 {
            wn -= 1;
        }
        // A full chunk unless it is the leading one.
        let mut k = 0;
        while k < per && (wn > 0 || rem != 0) {
            at -= 1;
            out[at] = DIGITS[(rem % rx) as usize];
            rem /= rx;
            k += 1;
        }
    }
    while end - at < width {
        at -= 1;
        out[at] = b'0';
    }
    at
}

/// floor((B^2 - 1) / d) - B for a normalized `d` (top bit set): the
/// reciprocal of Möller and Granlund, "Improved division by invariant
/// integers" (2011).
const fn reciprocal(d: L) -> L {
    (((((!d) as W) << LB) | L::MAX as W) / d as W) as L
}

/// `(u1 B + u0) / d` for `u1 < d`, `d` normalized, `v = reciprocal(d)`:
/// quotient and remainder (algorithm 4 of the paper).
const fn div2by1(u1: L, u0: L, d: L, v: L) -> (L, L) {
    let p = v as W * u1 as W + (((u1 as W) << LB) | u0 as W);
    let mut q1 = ((p >> LB) as L).wrapping_add(1);
    let q0 = p as L;
    let mut r = u0.wrapping_sub(q1.wrapping_mul(d));
    if r > q0 {
        q1 = q1.wrapping_sub(1);
        r = r.wrapping_add(d);
    }
    if r >= d {
        q1 += 1;
        r -= d;
    }
    (q1, r)
}

/// `r[..a.len()] -= a * x`; the borrow limb.
fn submul_1(r: &mut [L], a: &[L], x: L) -> L {
    let mut c: L = 0;
    for (y, &v) in r.iter_mut().zip(a) {
        let p = v as W * x as W + c as W;
        let (d, b) = y.overflowing_sub(p as L);
        *y = d;
        c = (p >> LB) as L + L::from(b);
    }
    c
}

/// `r <<= s` (`s < LB`); the bits shifted out.
fn shl_in(r: &mut [L], s: u32) -> L {
    if s == 0 {
        return 0;
    }
    let mut top = 0;
    for x in r {
        let v = *x;
        *x = (v << s) | top;
        top = v >> (LB - s);
    }
    top
}

/// `r >>= s` (`s < LB`), zero filled.
fn shr_in(r: &mut [L], s: u32) {
    if s == 0 {
        return;
    }
    let n = r.len();
    for i in 0..n - 1 {
        r[i] = (r[i] >> s) | (r[i + 1] << (LB - s));
    }
    r[n - 1] >>= s;
}

/// Knuth TAOCP 4.3.1 algorithm D: `u` (`m + n + 1` limbs, `u[m + n] <
/// v[n - 1]`) / `v` (`n` limbs, top bit set) → `q[0..=m]`, the
/// remainder left in `u[..n]`.
fn knuth_d(q: &mut [L], u: &mut [L], v: &[L]) {
    let n = v.len();
    let top = v[n - 1];
    let inv = reciprocal(top);
    if n == 1 {
        let mut rem = u[u.len() - 1];
        for i in (0..u.len() - 1).rev() {
            (q[i], rem) = div2by1(rem, u[i], top, inv);
            u[i] = 0;
        }
        u[0] = rem;
        return;
    }
    let next = v[n - 2];
    for j in (0..q.len()).rev() {
        let (u1, u0, u2) = (u[j + n], u[j + n - 1], u[j + n - 2]);
        let (mut qhat, mut rhat, mut big) = if u1 >= top {
            // q̂ = B - 1; r̂ = u1 B + u0 - (B - 1) top = u0 + top.
            let (r, o) = u0.overflowing_add(top);
            (L::MAX, r, o)
        } else {
            let (q, r) = div2by1(u1, u0, top, inv);
            (q, r, false)
        };
        // At most two steps make q̂ exact or one too big.
        while !big && qhat as W * next as W > (((rhat as W) << LB) | u2 as W) {
            qhat -= 1;
            (rhat, big) = rhat.overflowing_add(top);
        }
        let c = submul_1(&mut u[j..j + n], v, qhat);
        let (t, b) = u[j + n].overflowing_sub(c);
        u[j + n] = t;
        if b {
            // One too big: add the divisor back.
            qhat -= 1;
            let c = add_in(&mut u[j..j + n], v);
            u[j + n] = u[j + n].wrapping_add(L::from(c));
        }
        q[j] = qhat;
    }
}

/// `u32` limbs of scratch `rok_nat_divmod` needs.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_divmod_scratch(an: usize, bn: usize) -> usize {
    let (na, nb) = (packed(an), packed(bn));
    (2 * na + nb + 2) * PER
}

/// `q[0..=an - bn] = a / b`, `r[0..bn] = a % b` for `an >= bn >= 2`, the
/// top limb of `b` nonzero; `t` holds `rok_nat_divmod_scratch(an, bn)`.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `q`, `r` and `t`
/// distinct from each other and from `a` / `b`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_divmod(
    q: *mut u8,
    r: *mut u8,
    a: *mut u8,
    an: usize,
    b: *mut u8,
    bn: usize,
    t: *mut u8,
) {
    let (na, nb) = (packed(an), packed(bn));
    let tn = rok_nat_divmod_scratch(an, bn) / PER;
    // SAFETY: caller contract; `a` / `b` only read.
    let (a, b, q, r) = unsafe {
        (
            &*limbs(a, an),
            &*limbs(b, bn),
            limbs(q, an - bn + 1),
            limbs(r, bn),
        )
    };
    // SAFETY: as above; `t` is its own buffer of `tn * PER` limbs.
    let t = unsafe { slice::from_raw_parts_mut(t.add(HEADER_SIZE).cast::<L>(), tn) };
    let (u, t) = t.split_at_mut(na + 1);
    let (v, t) = t.split_at_mut(nb);
    let qs = &mut t[..=na - nb];
    parked(an, || {
        pack(&mut u[..na], a);
        u[na] = 0;
        pack(v, b);
        let s = v[nb - 1].leading_zeros();
        shl_in(v, s);
        u[na] = shl_in(&mut u[..na], s);
        knuth_d(qs, u, v);
        shr_in(&mut u[..nb], s);
        unpack(q, qs);
        unpack(r, &u[..nb]);
    });
}

/// gcd of two words (binary).
const fn gcd_word(mut a: L, mut b: L) -> L {
    if a == 0 {
        return b;
    }
    if b == 0 {
        return a;
    }
    let k = (a | b).trailing_zeros();
    a >>= a.trailing_zeros();
    loop {
        b >>= b.trailing_zeros();
        if a > b {
            (a, b) = (b, a);
        }
        b -= a;
        if b == 0 {
            return a << k;
        }
    }
}

/// Lehmer cofactors stay below 2^LEHMER in magnitude (see
/// `lehmer_apply`).
const LEHMER: u32 = LB - 2;

/// Knuth TAOCP 4.5.2 algorithm L on the top `2 LB - 2` bits of `x >= y`
/// (`n >= 2` limbs, `x[n - 1] != 0`; `y` at the same shift): cofactors
/// `(a, b, c, d)` such that `a x + b y` and `c x + d y` are two
/// consecutive remainders of Euclid's sequence; `b == 0` when not even
/// the first quotient is certain. `a`, `b` (and `c`, `d`) have opposite
/// signs. Two limbs of precision give quotients until the cofactors
/// reach about one limb, where the bound stops them. With `s > 0` no
/// remainder may fall below B^s: a remainder `r` of window value `w`
/// (the window `2^sh` apart from `x`) has `|r - w 2^sh| < 2^LEHMER
/// 2^sh` (its cofactors' bound), so `w` must reach `2^LEHMER + B^s /
/// 2^sh`.
fn lehmer(x: &[L], y: &[L], s: usize) -> (SL, SL, SL, SL) {
    const CAP: SW = 1 << LEHMER;
    let n = x.len();
    let z = x[n - 1].leading_zeros();
    let floor: SW = if s == 0 {
        0
    } else {
        let sh = (n as i64) * i64::from(LB) - i64::from(z) - 2 * i64::from(LB) + 2;
        let e = (s as i64) * i64::from(LB) - sh;
        if e >= 2 * i64::from(LB) - 2 {
            return (1, 0, 0, 1);
        }
        CAP + if e <= 0 { 1 } else { 1 << e }
    };
    let top = |v: &[L]| {
        let hi = (((v[n - 1] as W) << LB) | v[n - 2] as W) << z;
        let lo = if n > 2 && z > 0 {
            (v[n - 3] >> (LB - z)) as W
        } else {
            0
        };
        ((hi | lo) >> 2) as SW
    };
    let (mut xh, mut yh) = (top(x), top(y));
    let (mut a, mut b, mut c, mut d): (SW, SW, SW, SW) = (1, 0, 0, 1);
    while yh + c != 0 && yh + d != 0 {
        let q = (xh + a) / (yh + c);
        if q != (xh + b) / (yh + d) {
            break;
        }
        let next = |u: SW, v: SW| {
            q.checked_mul(v)
                .and_then(|p| u.checked_sub(p))
                .filter(|t| t.abs() < CAP)
        };
        let (Some(nc), Some(nd)) = (next(a, c), next(b, d)) else {
            break;
        };
        let r = xh - q * yh;
        if r < floor {
            break;
        }
        (a, c) = (c, nc);
        (b, d) = (d, nd);
        (xh, yh) = (yh, r);
    }
    (a as SL, b as SL, c as SL, d as SL)
}

/// `(x, y) = (a x + b y, c x + d y)` for Lehmer cofactors (both results
/// non-negative and at most `x`).
fn lehmer_apply(x: &mut [L], y: &mut [L], (a, b, c, d): (SL, SL, SL, SL)) {
    // |a u + b v| < 2^LEHMER 2^LB (opposite signs), so the sums fit `SW`.
    let (mut cx, mut cy): (SW, SW) = (0, 0);
    for (u, v) in x.iter_mut().zip(y.iter_mut()) {
        let (p, q) = (*u as SW, *v as SW);
        cx += a as SW * p + b as SW * q;
        cy += c as SW * p + d as SW * q;
        *u = cx as L;
        *v = cy as L;
        cx >>= LB;
        cy >>= LB;
    }
    debug_assert!(cx == 0 && cy == 0);
}

/// Limbs of `a` without leading zeros.
fn significant(a: &[L]) -> usize {
    a.iter().rposition(|&v| v != 0).map_or(0, |i| i + 1)
}

/// `gcd(x, y)` (`x >= y`, both `x.len()` limbs, zero-extended); the
/// result is left in `x` or `y`: which, and its limbs. Lehmer steps
/// replace runs of single-limb quotients by one linear pass; a step
/// that finds no quotient divides (Knuth D, scratch `t` of `3 x.len() +
/// 1`).
fn gcd_lehmer<'a>(mut x: &'a mut [L], mut y: &'a mut [L], t: &mut [L]) -> (&'a [L], usize) {
    let mut n = significant(x);
    loop {
        let yn = significant(&y[..n]);
        if yn == 0 {
            return (x, n);
        }
        if n == 1 {
            x[0] = gcd_word(x[0], y[0]);
            return (x, 1);
        }
        let f = lehmer(&x[..n], &y[..n], 0);
        if f.1 != 0 {
            lehmer_apply(&mut x[..n], &mut y[..n], f);
            n = significant(&x[..n]);
            continue;
        }
        // x = x mod y, then swap.
        let (q, t) = t.split_at_mut(n - yn + 1);
        divrem(q, &mut x[..n], &y[..yn], t);
        core::mem::swap(&mut x, &mut y);
        n = yn;
    }
}

/// `q = x / y`, `x = x % y` (`x.len() >= y.len()`, `y`'s top limb
/// nonzero; `q` of `x.len() - y.len() + 1`); `t` of `x.len() + y.len() +
/// 1` limbs.
fn divrem(q: &mut [L], x: &mut [L], y: &[L], t: &mut [L]) {
    let (n, yn) = (x.len(), y.len());
    let (u, rest) = t.split_at_mut(n + 1);
    let v = &mut rest[..yn];
    let sh = y[yn - 1].leading_zeros();
    v.copy_from_slice(y);
    shl_in(v, sh);
    u[..n].copy_from_slice(x);
    u[n] = shl_in(&mut u[..n], sh);
    knuth_d(q, u, v);
    shr_in(&mut u[..yn], sh);
    x[..yn].copy_from_slice(&u[..yn]);
    x[yn..].fill(0);
}

/// Limbs from which `hgcd` recurses, and from which `gcd` reduces by it.
const HGCD: usize = 100;
const GCD_DC: usize = 1200;

/// A 2x2 matrix of non-negative entries, determinant -1 when `neg`
/// (else 1): the cofactors of an `hgcd` reduction, the original pair
/// being `M` times the current one. Entries are zero-extended to their
/// common capacity.
struct Mat<'a> {
    e: [&'a mut [L]; 4],
    /// Entries are zero from limb `len` on.
    len: usize,
    neg: bool,
}

impl<'a> Mat<'a> {
    /// The identity with entries of `cap` limbs, carved from `t`.
    fn new(t: &'a mut [L], cap: usize) -> (Self, &'a mut [L]) {
        let (e0, t) = t.split_at_mut(cap);
        let (e1, t) = t.split_at_mut(cap);
        let (e2, t) = t.split_at_mut(cap);
        let (e3, t) = t.split_at_mut(cap);
        for e in [&mut *e0, &mut *e1, &mut *e2, &mut *e3] {
            e.fill(0);
        }
        e0[0] = 1;
        e3[0] = 1;
        let e = [e0, e1, e2, e3];
        (
            Mat {
                e,
                len: 1,
                neg: false,
            },
            t,
        )
    }

    /// Entry `i` up to `len`.
    fn at(&self, i: usize) -> &[L] {
        &self.e[i][..self.len]
    }

    /// `len` lowered past zero top limbs.
    fn trim(&mut self) {
        while self.len > 1 && self.e.iter().all(|e| e[self.len - 1] == 0) {
            self.len -= 1;
        }
    }

    /// Row `r`'s two entries, `w` limbs of them.
    fn row(&mut self, r: usize, w: usize) -> (&mut [L], &mut [L]) {
        let (lo, hi) = self.e.split_at_mut(2 * r + 1);
        (&mut lo[2 * r][..w], &mut hi[0][..w])
    }

    /// `M = M [[c0, c1], [c2, c3]]` for single limbs `c` below
    /// 2^LEHMER (determinant -1 when `neg`).
    fn mul1(&mut self, c: [L; 4], neg: bool) {
        let w = (self.len + 1).min(self.e[0].len());
        for r in 0..2 {
            let (x, y) = self.row(r, w);
            let (mut cx, mut cy) = (0 as W, 0 as W);
            for (u, v) in x.iter_mut().zip(y.iter_mut()) {
                let (p, q) = (*u as W, *v as W);
                // < 2 (2^LB - 1) 2^LEHMER + carry < 2^2LB.
                cx += p * c[0] as W + q * c[2] as W;
                cy += p * c[1] as W + q * c[3] as W;
                *u = cx as L;
                *v = cy as L;
                cx >>= LB;
                cy >>= LB;
            }
            debug_assert!(cx == 0 && cy == 0);
        }
        self.len = w;
        self.trim();
        self.neg ^= neg;
    }

    /// `M = M [[q, 1], [1, 0]]` (one Euclid step of quotient `q`).
    fn mul_q(&mut self, q: &[L], t: &mut [L]) {
        let cap = self.e[0].len();
        let w = (significant(q) + self.len + 1).min(cap);
        let (p, t) = t.split_at_mut(significant(q) + self.len + 1);
        for r in 0..2 {
            let (x, y) = self.row(r, w);
            mul_into(p, q, x, t);
            add_in(p, y);
            y.copy_from_slice(x);
            debug_assert!(significant(p) <= w);
            x.copy_from_slice(&p[..w]);
        }
        self.len = w;
        self.trim();
        self.neg = !self.neg;
    }

    /// Columns swapped (`M [[0, 1], [1, 0]]`).
    fn swap_cols(&mut self) {
        let w = self.len;
        for r in 0..2 {
            let (x, y) = self.row(r, w);
            x.swap_with_slice(y);
        }
        self.neg = !self.neg;
    }

    /// `M = M N`.
    fn mul(&mut self, n: &Mat<'_>, t: &mut [L]) {
        // A sum of two products: one limb past them.
        let k = self.len + n.len + 1;
        let w = k.min(self.e[0].len());
        let (p0, t) = t.split_at_mut(k);
        let (p1, t) = t.split_at_mut(k);
        let (p2, t) = t.split_at_mut(k);
        let (p3, t) = t.split_at_mut(k);
        let len = self.len;
        if let Some(nt) = dot_len(len.min(n.len), k) {
            // One transform per entry: row r of M N is (x n0 + y n2, x
            // n1 + y n3) for M's row (x, y).
            let x: [&[u32]; 8] = [
                digits32(self.at(0)),
                digits32(self.at(1)),
                digits32(self.at(2)),
                digits32(self.at(3)),
                digits32(n.at(0)),
                digits32(n.at(1)),
                digits32(n.at(2)),
                digits32(n.at(3)),
            ];
            let terms = [
                (0, 4, 1, 6, false),
                (0, 5, 1, 7, false),
                (2, 4, 3, 6, false),
                (2, 5, 3, 7, false),
            ];
            let mut out = [
                digits32_mut(p0),
                digits32_mut(p1),
                digits32_mut(p2),
                digits32_mut(p3),
            ];
            ntt_dot(&mut out, &x, &terms, nt, digits32_mut(t));
            for (i, q) in [&*p0, &*p1, &*p2, &*p3].into_iter().enumerate() {
                debug_assert!(significant(q) <= w);
                self.e[i][..w].copy_from_slice(&q[..w]);
            }
        } else {
            for r in 0..2 {
                let (x, y) = self.row(r, w);
                mul_into(p0, &x[..len], n.at(0), t);
                mul_into(p1, &y[..len], n.at(2), t);
                mul_into(p2, &x[..len], n.at(1), t);
                mul_into(p3, &y[..len], n.at(3), t);
                let c = add_in(p0, p1) | add_in(p2, p3);
                debug_assert!(!c && significant(p0) <= w && significant(p2) <= w);
                x.copy_from_slice(&p0[..w]);
                y.copy_from_slice(&p2[..w]);
            }
        }
        self.len = w;
        self.trim();
        self.neg ^= n.neg;
    }
}

/// `r = a b` zero-extended (`r` of at least their significant limbs).
fn mul_into(r: &mut [L], a: &[L], b: &[L], t: &mut [L]) {
    let (an, bn) = (significant(a), significant(b));
    if an == 0 || bn == 0 {
        r.fill(0);
        return;
    }
    let (lo, hi) = r.split_at_mut(an + bn);
    mul(lo, &a[..an], &b[..bn], t);
    hi.fill(0);
}

/// `x < y` (same length).
fn less(x: &[L], y: &[L]) -> bool {
    x.iter().rev().cmp(y.iter().rev()) == core::cmp::Ordering::Less
}

/// `(a, b) = M^-1 (a, b)` for `a = α B^p + a_lo`, `b = β B^p + b_lo`
/// where `(α, β)` already is `M^-1` of the old high parts, ordered so
/// that `a >= b` (`M`'s columns swap when needed). With `M^-1 = det
/// [[m11, -m01], [-m10, m00]]`: `a = α B^p + det (m11 a_lo - m01
/// b_lo)`, the same for `b`. Both stay positive when α, β are at least
/// B^s and the entries below B^(k - s) for high parts of `k <= 2s`
/// limbs (what `hgcd` leaves): the low terms are below B^(k - s + p).
fn adjust(a: &mut [L], b: &mut [L], p: usize, m: &mut Mat<'_>, t: &mut [L]) {
    let w = p + m.len;
    let (u, t) = t.split_at_mut(w);
    let (v, t) = t.split_at_mut(w);
    let (x, t) = t.split_at_mut(w);
    let (y, t) = t.split_at_mut(w);
    // (U, V) = (m11 a_lo - m01 b_lo, m00 b_lo - m10 a_lo), as magnitudes
    // in `u` and `x` with their signs.
    let signs = if let Some(nt) = dot_len(m.len.min(p), w) {
        let ops: [&[u32]; 6] = [
            digits32(&a[..p]),
            digits32(&b[..p]),
            digits32(m.at(0)),
            digits32(m.at(1)),
            digits32(m.at(2)),
            digits32(m.at(3)),
        ];
        let terms = [(5, 0, 3, 1, true), (2, 1, 4, 0, true)];
        let mut out = [digits32_mut(u), digits32_mut(x)];
        let neg = ntt_dot(&mut out, &ops, &terms, nt, digits32_mut(t));
        for (d, &ng) in [&mut *u, &mut *x].into_iter().zip(&neg) {
            if ng {
                neg_in(d);
            }
        }
        [neg[0], neg[1]]
    } else {
        mul_into(u, m.at(3), &a[..p], t);
        mul_into(y, m.at(2), &a[..p], t);
        mul_into(v, m.at(1), &b[..p], t);
        mul_into(x, m.at(0), &b[..p], t);
        let mut signs = [false; 2];
        for ((pos, neg), sg) in [(&mut *u, &mut *v), (&mut *x, &mut *y)]
            .into_iter()
            .zip(&mut signs)
        {
            *sg = less(pos, neg);
            if *sg {
                sub_in(neg, pos);
                pos.copy_from_slice(neg);
            } else {
                sub_in(pos, neg);
            }
        }
        signs
    };
    a[..p].fill(0);
    b[..p].fill(0);
    for ((r, d), sg) in [(&mut *a, &*u), (&mut *b, &*x)].into_iter().zip(signs) {
        // r += det (U or V).
        let minus = sg != m.neg;
        let dn = significant(d);
        let o = if minus {
            sub_in(r, &d[..dn])
        } else {
            add_in(r, &d[..dn])
        };
        debug_assert!(!o);
    }
    if less(a, b) {
        a.swap_with_slice(b);
        m.swap_cols();
    }
}

/// One reduction of `a >= b` keeping both at least B^s, into `M`: a
/// Lehmer step, else one Euclid step; whether there was one.
fn hgcd_step(a: &mut [L], b: &mut [L], s: usize, m: &mut Mat<'_>, t: &mut [L]) -> bool {
    let n = significant(a);
    let bn = significant(b);
    if bn <= s {
        return false;
    }
    let f = lehmer(&a[..n], &b[..n], s);
    if f.1 != 0 {
        lehmer_apply(&mut a[..n], &mut b[..n], f);
        // M (a', b') with (a', b') = E (a, b): M E^-1, E^-1 = det E [[d,
        // -b], [-c, a]] (no negative entries).
        let (ca, cb, cc, cd) = f;
        let det = SW::from(ca) * SW::from(cd) - SW::from(cb) * SW::from(cc);
        let u = |v: SL| v.unsigned_abs() as L;
        m.mul1([u(cd), u(cb), u(cc), u(ca)], det < 0);
        return true;
    }
    // (a, b) = (b, a mod b) unless that falls below B^s.
    let (q, t) = t.split_at_mut(n - bn + 1);
    let (r, t) = t.split_at_mut(n);
    r.copy_from_slice(&a[..n]);
    divrem(q, r, &b[..bn], t);
    if significant(r) <= s {
        return false;
    }
    m.mul_q(q, t);
    a[..n].copy_from_slice(&b[..n]);
    b[..n].copy_from_slice(r);
    true
}

/// Half gcd (Möller, "On Schönhage's algorithm and subquadratic integer
/// gcd computation", 2008): reduces `a >= b` (`n` significant limbs) by
/// Euclid steps while both stay at least B^s, `s = n/2 + 1`, into `M`
/// (the identity on entry, entries of at least `n - s + 1` limbs);
/// whether it did. From `HGCD` limbs on, the top half first reduces
/// itself recursively and its matrix is applied to the rest
/// (`adjust`), then the top of what is left once more, so the steps
/// in between and at the end are few.
fn hgcd(a: &mut [L], b: &mut [L], m: &mut Mat<'_>, t: &mut [L]) -> bool {
    let n = significant(a);
    let s = n / 2 + 1;
    if significant(b) <= s {
        return false;
    }
    let mut done = false;
    if n >= HGCD {
        let p = n / 2;
        if hgcd(&mut a[p..n], &mut b[p..n], m, t) {
            adjust(&mut a[..n], &mut b[..n], p, m, t);
            done = true;
        }
        while significant(a) > 3 * n / 4 + 1 {
            if !hgcd_step(a, b, s, m, t) {
                return done;
            }
            done = true;
        }
        let k = significant(a);
        if k > s + 2 {
            // The top `2 (k - s) - 1` limbs, whose own floor lands at
            // about B^(s + 1) for the whole.
            let p = 2 * s - k + 1;
            let (mut m1, t) = Mat::new(t, (k - p) / 2 + 3);
            if hgcd(&mut a[p..k], &mut b[p..k], &mut m1, t) {
                adjust(&mut a[..k], &mut b[..k], p, &mut m1, t);
                m.mul(&m1, t);
                done = true;
            }
        }
    }
    while hgcd_step(a, b, s, m, t) {
        done = true;
    }
    done
}

/// Scratch `L`s `hgcd` may use for `n` limbs: at each level what
/// `hgcd_step`, `adjust` and `Mat::mul` take (their temporaries plus a
/// product's scratch, or `ntt_dot`'s: at most 44 `L`s per result limb),
/// and the second half's matrix.
const fn hgcd_scratch(n: usize) -> usize {
    let own = 48 * n + 64 + scratch_bound(n, n);
    if n < HGCD {
        return own;
    }
    let first = hgcd_scratch(n - n / 2);
    let inner = hgcd_scratch(n / 2);
    let second = 4 * (n / 4 + 4) + if inner > own { inner } else { own };
    let r = if first > own { first } else { own };
    if second > r { second } else { r }
}

/// `x, y` (`x >= y`, zero-extended alike) reduced by `hgcd` on their top
/// third until below `GCD_DC` limbs (one Euclid step when it finds
/// nothing); the gcd is unchanged.
fn gcd_dc(x: &mut [L], y: &mut [L], t: &mut [L]) {
    loop {
        let n = significant(x);
        let yn = significant(y);
        if n < GCD_DC || yn == 0 {
            return;
        }
        let p = 2 * n / 3;
        let (mut m, t) = Mat::new(t, (n - p) / 2 + 3);
        if hgcd(&mut x[p..n], &mut y[p..n], &mut m, t) {
            adjust(&mut x[..n], &mut y[..n], p, &mut m, t);
        } else {
            let (q, t) = t.split_at_mut(n - yn + 1);
            divrem(q, &mut x[..n], &y[..yn], t);
            x.swap_with_slice(y);
        }
    }
}

/// `u32` limbs of scratch `rok_nat_gcd` needs.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_nat_gcd_scratch(an: usize, bn: usize) -> usize {
    let m = packed(if an > bn { an } else { bn });
    let dc = if m < GCD_DC {
        0
    } else {
        let (h, a) = (
            hgcd_scratch(m - 2 * m / 3),
            32 * m + 64 + scratch_bound(m, m),
        );
        4 * (m / 6 + 4) + if h > a { h } else { a }
    };
    (2 * m + if dc > 3 * m + 1 { dc } else { 3 * m + 1 }) * PER
}

/// `r = gcd(a, b)` (`an >= bn >= 1`, `a >= b`, both without leading
/// zero limbs): half-gcd reductions down to `GCD_DC` limbs, then
/// Lehmer; its limbs (at most `bn`). `t` holds
/// `rok_nat_gcd_scratch(an, bn)` limbs.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths (`r` of `bn`); `r` and
/// `t` distinct from each other and from `a` / `b`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_gcd(
    r: *mut u8,
    a: *mut u8,
    an: usize,
    b: *mut u8,
    bn: usize,
    t: *mut u8,
) -> usize {
    let m = packed(an);
    let tn = rok_nat_gcd_scratch(an, bn) / PER;
    // SAFETY: caller contract; `a` / `b` only read.
    let (a, b, r) = unsafe { (&*limbs(a, an), &*limbs(b, bn), limbs(r, bn)) };
    // SAFETY: as above; `t` is its own buffer of `tn * PER` limbs.
    let t = unsafe { slice::from_raw_parts_mut(t.add(HEADER_SIZE).cast::<L>(), tn) };
    let (x, t) = t.split_at_mut(m);
    let (y, t) = t.split_at_mut(m);
    parked(an, || {
        pack(x, a);
        y.fill(0);
        pack(&mut y[..packed(bn)], b);
        gcd_dc(x, y, t);
        let (g, gn) = gcd_lehmer(x, y, t);
        // SAFETY: as in `digits32`.
        let g32 = unsafe { slice::from_raw_parts(g.as_ptr().cast::<u32>(), gn * PER) };
        let k = g32.iter().rposition(|&v| v != 0).map_or(0, |i| i + 1);
        r[..k].copy_from_slice(&g32[..k]);
        k
    })
}

/// `r[0..an] = a + b` (`an >= bn`); the carry.
///
/// # Safety
/// Live `GcBuf<u32>`s of at least those lengths; `r` distinct from `a`
/// and `b`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_add(
    r: *mut u8,
    a: *mut u8,
    an: usize,
    b: *mut u8,
    bn: usize,
) -> u32 {
    // SAFETY: caller contract; `a` / `b` only read.
    let (r, a, b) = unsafe { (limbs(r, an), &*limbs(a, an), &*limbs(b, bn)) };
    let (lo, hi) = r.split_at_mut(bn);
    let mut c = 0u64;
    for ((x, &p), &q) in lo.iter_mut().zip(a).zip(b) {
        let s = u64::from(p) + u64::from(q) + c;
        *x = s as u32;
        c = s >> 32;
    }
    for (x, &p) in hi.iter_mut().zip(&a[bn..]) {
        let s = u64::from(p) + c;
        *x = s as u32;
        c = s >> 32;
    }
    c as u32
}

/// `r[0..an] = a - b` (`a >= b`, `an >= bn`).
///
/// # Safety
/// As `rok_nat_add`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_sub(r: *mut u8, a: *mut u8, an: usize, b: *mut u8, bn: usize) {
    // SAFETY: as in `rok_nat_add`.
    let (r, a, b) = unsafe { (limbs(r, an), &*limbs(a, an), &*limbs(b, bn)) };
    let (lo, hi) = r.split_at_mut(bn);
    let mut borrow = 0u32;
    for ((x, &p), &q) in lo.iter_mut().zip(a).zip(b) {
        let (d, b1) = p.overflowing_sub(q);
        let (d, b2) = d.overflowing_sub(borrow);
        *x = d;
        borrow = u32::from(b1 | b2);
    }
    for (x, &p) in hi.iter_mut().zip(&a[bn..]) {
        let (d, b1) = p.overflowing_sub(borrow);
        *x = d;
        borrow = u32::from(b1);
    }
}

/// `r[off..=off + n] = a[0..n] << s` (`s < 32`).
///
/// # Safety
/// Live, distinct `GcBuf<u32>`s of at least those lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_shl(r: *mut u8, off: usize, a: *mut u8, n: usize, s: u32) {
    // SAFETY: caller contract.
    let (r, a) = unsafe { (&mut limbs(r, off + n + 1)[off..], &*limbs(a, n)) };
    let mut top = 0u64;
    for (x, &v) in r.iter_mut().zip(a) {
        let w = (u64::from(v) << s) | top;
        *x = w as u32;
        top = w >> 32;
    }
    r[n] = top as u32;
}

/// `r[0..n - s / 32] = a[0..n] >> s` (`s < 32 n`).
///
/// # Safety
/// Live, distinct `GcBuf<u32>`s of at least those lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_nat_shr(r: *mut u8, a: *mut u8, n: usize, s: u64) {
    let skip = (s / 32) as usize;
    // SAFETY: caller contract.
    let (r, a) = unsafe { (limbs(r, n - skip), &limbs(a, n)[skip..]) };
    for (i, x) in r.iter_mut().enumerate() {
        let next = a.get(i + 1).copied().unwrap_or(0);
        *x = (((u64::from(next) << 32) | u64::from(a[i])) >> (s % 32)) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap buffer laid out like a `GcBuf<u32>` (header + limbs).
    fn buf(limbs: &[u32], len: usize) -> Vec<u64> {
        let mut v = vec![0u64; (HEADER_SIZE + 4 * len.max(limbs.len())).div_ceil(8) + 1];
        let p = v.as_mut_ptr().cast::<u8>();
        for (i, &l) in limbs.iter().enumerate() {
            // SAFETY: in bounds of `v`.
            unsafe { p.add(HEADER_SIZE).cast::<u32>().add(i).write(l) };
        }
        v
    }

    fn read(v: &mut [u64], n: usize) -> Vec<u32> {
        // SAFETY: `v` came from `buf` with room for `n` limbs.
        unsafe { limbs(v.as_mut_ptr().cast(), n).to_vec() }
    }

    fn num(l: &[u32]) -> u128 {
        l.iter()
            .rev()
            .fold(0u128, |acc, &x| (acc << 32) | u128::from(x))
    }

    /// xorshift64*; all-ones limbs now and then (carry chains).
    fn rand_limbs(seed: &mut u64, n: usize) -> Vec<L> {
        (0..n)
            .map(|_| {
                *seed ^= *seed >> 12;
                *seed ^= *seed << 25;
                *seed ^= *seed >> 27;
                let v = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
                if v % 7 == 0 { L::MAX } else { v as L }
            })
            .collect()
    }

    #[test]
    fn mul_matches_schoolbook() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let sizes = [
            1,
            2,
            17,
            KARA - 1,
            KARA,
            KARA + 1,
            2 * KARA + 3,
            KARA_SQR,
            TOOM3 - 1,
            TOOM3,
            TOOM3 + 1,
            TOOM3_SQR,
            TOOM3_SQR + 1,
            3 * TOOM3 + 2,
            700,
            NTT - 1,
            NTT,
            NTT_SQR + 1,
            2 * NTT + 7,
        ];
        for &n in &sizes {
            for &m in &sizes {
                if m > n {
                    continue;
                }
                let a = rand_limbs(&mut seed, n);
                let b = rand_limbs(&mut seed, m);
                let mut want = vec![0; n + m];
                mul_basecase(&mut want, &a, &b);
                let mut got = vec![0; n + m];
                let mut t = vec![0; scratch_bound(n, m)];
                mul(&mut got, &a, &b, &mut t);
                assert_eq!(got, want, "{n} x {m}");
            }
            let a = rand_limbs(&mut seed, n);
            let mut want = vec![0; 2 * n];
            mul_basecase(&mut want, &a, &a);
            let mut got = vec![0; 2 * n];
            let mut t = vec![0; scratch_bound(n, n)];
            sqr(&mut got, &a, &mut t);
            assert_eq!(got, want, "{n}^2");
        }
    }

    /// Binary gcd on `L` vectors (reference).
    fn gcd_ref(mut a: Vec<L>, mut b: Vec<L>) -> Vec<L> {
        let n = a.len().max(b.len());
        a.resize(n, 0);
        b.resize(n, 0);
        let zero = |v: &[L]| v.iter().all(|&x| x == 0);
        let even = |v: &[L]| v[0] & 1 == 0;
        let less =
            |x: &[L], y: &[L]| x.iter().rev().cmp(y.iter().rev()) == core::cmp::Ordering::Less;
        if zero(&a) {
            return b;
        }
        let mut k = 0;
        while even(&a) && even(&b) && !zero(&b) {
            shr_in(&mut a, 1);
            shr_in(&mut b, 1);
            k += 1;
        }
        while !zero(&b) {
            while even(&a) {
                shr_in(&mut a, 1);
            }
            while !zero(&b) && even(&b) {
                shr_in(&mut b, 1);
            }
            if zero(&b) {
                break;
            }
            if less(&b, &a) {
                core::mem::swap(&mut a, &mut b);
            }
            sub_in(&mut b, &a);
        }
        a.push(0);
        for _ in 0..k {
            shl1_in(&mut a);
        }
        a
    }

    /// (B^k - 1)^2 = B^2k - 2 B^k + 1 (`u32` digits): every coefficient
    /// of the convolution at its maximum, the products and squares
    /// through the NTT (`k` u32 limbs).
    #[test]
    fn ntt_max_coefficients() {
        let k = 1 << 16;
        let a = vec![L::MAX; k / PER];
        let mut want = vec![0u32; 2 * k];
        want[0] = 1;
        want[k] = 0xFFFF_FFFE;
        want[k + 1..].fill(u32::MAX);
        let mut t = vec![0; ntt_scratch(a.len(), a.len())];
        for square in [false, true] {
            let mut r = vec![0; 2 * a.len()];
            ntt_mul(&mut r, &a, if square { &[] } else { &a }, &mut t);
            assert_eq!(digits32(&r), &want[..], "square {square}");
        }
    }

    #[test]
    fn ntt_prepared_matches_mul() {
        let mut seed = 0xA076_1D64_78BD_642Fu64;
        let wn = 1 << 13;
        let mut w = vec![0u32; 6 * wn];
        ntt_roots(&mut w, wn);
        for &(an, bn, n) in &[
            (1usize, 1usize, 4usize),
            (1500, 1400, 4096),
            (2000, 2096, 4096),
            (3000, 2000, 8192),
        ] {
            let a = digits32(&rand_limbs(&mut seed, an.div_ceil(PER)))[..an].to_vec();
            let b = digits32(&rand_limbs(&mut seed, bn.div_ceil(PER)))[..bn].to_vec();
            let mut f = vec![0u32; 3 * n];
            ntt_forward(&mut f, n, &b, &w, wn);
            let mut t = vec![0u32; 3 * n];
            ntt_forward(&mut t, n, &a, &w, wn);
            let s = ntt_finish(&mut t, n, &f, &w, wn);
            let mut got = vec![0u32; an + bn];
            garner(&mut got, &mut t, n, s, false);
            // Schoolbook on u32 digits.
            let mut want = vec![0u32; an + bn];
            for (i, &x) in a.iter().enumerate() {
                let mut c = 0u64;
                for (j, &y) in b.iter().enumerate() {
                    let v = u64::from(x) * u64::from(y) + u64::from(want[i + j]) + c;
                    want[i + j] = v as u32;
                    c = v >> 32;
                }
                want[i + bn] = c as u32;
            }
            assert_eq!(got, want, "{an} x {bn} in {n}");
        }
    }

    /// `a * b mod (B^m - 1)` (`u32` digits) by schoolbook and folding.
    fn mulmod_ref(a: &[u32], b: &[u32], m: usize) -> Vec<u32> {
        let mut p = vec![0u32; a.len() + b.len()];
        for (i, &x) in a.iter().enumerate() {
            let mut c = 0u64;
            for (j, &y) in b.iter().enumerate() {
                let v = u64::from(x) * u64::from(y) + u64::from(p[i + j]) + c;
                p[i + j] = v as u32;
                c = v >> 32;
            }
            p[i + b.len()] = c as u32;
        }
        let mut r = vec![0u32; m];
        for chunk in p.chunks(m) {
            let mut c = 0u128;
            for (x, &v) in r.iter_mut().zip(chunk) {
                let s = u128::from(*x) + u128::from(v) + c;
                *x = s as u32;
                c = s >> 32;
            }
            for x in &mut r[chunk.len()..] {
                let s = u128::from(*x) + c;
                *x = s as u32;
                c = s >> 32;
            }
            wrap_in(&mut r, c);
        }
        wrap_in(&mut r, 0);
        r
    }

    #[test]
    fn mulmod_matches_folded_product() {
        let mut seed = 0x3C6E_F372_FE94_F82Bu64;
        let big = NTT * PER;
        for &(an, bn, n) in &[
            (1usize, 1usize, 1usize),
            (3, 2, 3),
            (40, 30, 50),
            (200, 150, 202),
            (big, big, big + 2),
            (big + 5, big - 9, big),
            (2 * big, big, 2 * big + 1),
            (4096, 4096, 4096),
            (4096, 4000, 4096),
            (4096, 1, 4096),
        ] {
            let m = rok_nat_mulmod_len(an, bn, n);
            assert!(m >= n);
            for square in [false, true] {
                if square && an != bn {
                    continue;
                }
                let a = digits32(&rand_limbs(&mut seed, an.div_ceil(PER)))[..an].to_vec();
                let b = if square {
                    a.clone()
                } else {
                    digits32(&rand_limbs(&mut seed, bn.div_ceil(PER)))[..bn].to_vec()
                };
                let (mut ab, mut bb, mut r) = (buf(&a, an), buf(&b, bn), buf(&[], m));
                let mut t = buf(&[], rok_nat_mulmod_scratch(an, bn, m));
                let bp = if square {
                    ab.as_mut_ptr()
                } else {
                    bb.as_mut_ptr()
                };
                // SAFETY: test buffers sized above.
                unsafe {
                    rok_nat_mulmod(
                        r.as_mut_ptr().cast(),
                        ab.as_mut_ptr().cast(),
                        an,
                        bp.cast(),
                        bn,
                        m,
                        t.as_mut_ptr().cast(),
                    );
                }
                assert_eq!(
                    read(&mut r, m),
                    mulmod_ref(&a, &b, m),
                    "{an} x {bn} mod {m}"
                );
            }
        }
        // All ones squared: B^m - 1 = 0 comes out as 0.
        let m = 1 << 12;
        let a = vec![u32::MAX; m];
        assert_eq!(rok_nat_mulmod_len(m, m, m), m);
        let (mut ab, mut r) = (buf(&a, m), buf(&[], m));
        let mut t = buf(&[], rok_nat_mulmod_scratch(m, m, m));
        // SAFETY: as above.
        unsafe {
            let p = ab.as_mut_ptr().cast();
            rok_nat_mulmod(r.as_mut_ptr().cast(), p, m, p, m, m, t.as_mut_ptr().cast());
        }
        assert!(read(&mut r, m).iter().all(|&x| x == 0));
    }

    #[test]
    fn ntt_prepared_wraps() {
        let mut seed = 0x1B87_3593_8BD5_4F2Fu64;
        let n = 1 << 12;
        let mut w = vec![0u32; 6 * n];
        ntt_roots(&mut w, n);
        for &(an, bn) in &[(n, n), (3000, n), (n, 1500), (2100, 2100)] {
            let a = digits32(&rand_limbs(&mut seed, an.div_ceil(PER)))[..an].to_vec();
            let b = digits32(&rand_limbs(&mut seed, bn.div_ceil(PER)))[..bn].to_vec();
            let (mut fb, mut bb) = (buf(&[], 3 * n), buf(&b, bn));
            let (mut ab, mut wb) = (buf(&a, an), buf(&w, 6 * n));
            let rn = n.min(an + bn);
            let (mut r, mut t) = (buf(&[], rn), buf(&[], 3 * n));
            // SAFETY: test buffers sized above.
            unsafe {
                rok_nat_ntt_prep(
                    fb.as_mut_ptr().cast(),
                    bb.as_mut_ptr().cast(),
                    bn,
                    n,
                    wb.as_mut_ptr().cast(),
                    n,
                );
                rok_nat_ntt_mul(
                    r.as_mut_ptr().cast(),
                    ab.as_mut_ptr().cast(),
                    an,
                    fb.as_mut_ptr().cast(),
                    bn,
                    n,
                    wb.as_mut_ptr().cast(),
                    n,
                    t.as_mut_ptr().cast(),
                );
            }
            let mut want = mulmod_ref(&a, &b, n);
            want.truncate(rn);
            assert_eq!(read(&mut r, rn), want, "{an} x {bn} mod {n}");
        }
    }

    #[test]
    fn gcd_matches_binary_gcd() {
        let mut seed = 0x5DEE_CE66_D1CE_4E5Bu64;
        for &(n, m, gl) in &[
            (1, 1, 1),
            (2, 1, 1),
            (2, 2, 1),
            (3, 2, 2),
            (5, 5, 3),
            (9, 4, 4),
            (17, 16, 6),
            (30, 30, 1),
        ] {
            for _ in 0..30 {
                // Shared factor g, cofactors without the all-ones bias.
                let g = rand_limbs(&mut seed, gl);
                let mut a = vec![0; n + gl];
                let mut b = vec![0; m + gl];
                let (ca, cb) = (rand_limbs(&mut seed, n), rand_limbs(&mut seed, m));
                mul_basecase(&mut a, &ca, &g);
                if m >= gl {
                    mul_basecase(&mut b, &cb, &g);
                } else {
                    mul_basecase(&mut b, &g, &cb);
                }
                let mut want = gcd_ref(a.clone(), b.clone());
                let (a, b) = if gcd_ref_less(&a, &b) { (b, a) } else { (a, b) };
                let w = a.len();
                let mut x = a.clone();
                let mut y = b.clone();
                y.resize(w, 0);
                let mut t = vec![0; 3 * w + 1];
                let (gv, gn) = gcd_lehmer(&mut x, &mut y, &mut t);
                let mut got = gv[..gn].to_vec();
                got.resize(want.len().max(gn), 0);
                want.resize(got.len(), 0);
                assert_eq!(got, want, "{n} {m} {gl}");
            }
        }
    }

    /// `gcd(a, b)` (`a >= b`) through `rok_nat_gcd`'s path and through
    /// Lehmer alone.
    fn gcd_both(a: &[L], b: &[L]) -> (Vec<L>, Vec<L>) {
        let w = a.len();
        let run = |dc: bool| {
            let (mut x, mut y) = (a.to_vec(), b.to_vec());
            y.resize(w, 0);
            let mut t = vec![0; rok_nat_gcd_scratch(w * PER, w * PER) / PER];
            if dc {
                gcd_dc(&mut x, &mut y, &mut t);
            }
            let (g, gn) = gcd_lehmer(&mut x, &mut y, &mut t);
            g[..gn].to_vec()
        };
        (run(true), run(false))
    }

    #[test]
    fn hgcd_matches_lehmer() {
        let mut seed = 0x6A09_E667_F3BC_C909u64;
        for &(n, m, gl) in &[
            (GCD_DC, GCD_DC, 1),
            (GCD_DC + 7, GCD_DC - 3, 40),
            (GCD_DC + 2 * HGCD + 5, GCD_DC + 2 * HGCD, 3),
            (2000, 1999, 600),
            (2500, 2500, 1),
            (3000, GCD_DC, 700),
            (5000, 5000, 9),
        ] {
            // Shared factor g; all-ones limbs now and then.
            let g = rand_limbs(&mut seed, gl);
            let (ca, cb) = (rand_limbs(&mut seed, n - gl), rand_limbs(&mut seed, m - gl));
            let mut a = vec![0; n];
            let mut b = vec![0; m];
            mul(&mut a, &ca, &g, &mut vec![0; scratch_bound(n, gl)]);
            mul(&mut b, &cb, &g, &mut vec![0; scratch_bound(m, gl)]);
            let (a, b) = if gcd_ref_less(&a, &b) { (b, a) } else { (a, b) };
            let (got, want) = gcd_both(&a, &b);
            assert_eq!(got, want, "{n} {m} {gl}");
            assert!(want.len() >= gl.min(2) || gl == 1);
        }
        // Consecutive Fibonacci numbers: every quotient 1.
        let n = 2 * GCD_DC;
        let (mut x, mut y) = (vec![0 as L; n], vec![0 as L; n]);
        y[0] = 1;
        while x[n - 1] >> (LB - 2) == 0 {
            add_in(&mut x, &y);
            core::mem::swap(&mut x, &mut y);
        }
        let (got, want) = gcd_both(&y, &x);
        assert_eq!((got.as_slice(), want.as_slice()), (&[1][..], &[1][..]));
        // a = b q + 1 with a huge quotient, and b | a.
        let b = rand_limbs(&mut seed, GCD_DC + 50);
        let q = rand_limbs(&mut seed, 2 * GCD_DC);
        let mut a = vec![0; b.len() + q.len()];
        mul(
            &mut a,
            &q,
            &b,
            &mut vec![0; scratch_bound(q.len(), b.len())],
        );
        let (got, want) = gcd_both(&a, &b);
        assert_eq!(got, want);
        add_in(&mut a, &[1]);
        let (got, want) = gcd_both(&a, &b);
        assert_eq!(got, want);
    }

    /// M N for entries all B^k - 1: each a sum of two full products,
    /// 2 (B^k - 1)^2, one limb past them (both product paths).
    #[test]
    fn mat_mul_carries() {
        for k in [3, NTT / 2 + 1] {
            let cap = 2 * k + 2;
            let mut ones = vec![L::MAX; k];
            ones.resize(cap, 0);
            let (mut sm, mut sn) = (vec![0; 4 * cap], vec![0; 4 * cap]);
            let (mut m, _) = Mat::new(&mut sm, cap);
            let (mut n, _) = Mat::new(&mut sn, cap);
            for e in m.e.iter_mut().chain(n.e.iter_mut()) {
                e.copy_from_slice(&ones);
            }
            (m.len, n.len) = (k, k);
            let mut t = vec![0; 64 * cap + scratch_bound(cap, cap)];
            m.mul(&n, &mut t);
            // 2 (B^k - 1)^2 = B^2k + (B^k - 4) B^k + 2.
            let mut want = vec![0 as L; cap];
            want[0] = 2;
            want[k] = L::MAX - 3;
            want[k + 1..2 * k].fill(L::MAX);
            want[2 * k] = 1;
            for e in &m.e {
                assert_eq!(&e[..], &want[..], "k {k}");
            }
        }
    }

    fn gcd_ref_less(x: &[L], y: &[L]) -> bool {
        let n = x.len().max(y.len());
        (0..n)
            .rev()
            .map(|i| {
                (
                    x.get(i).copied().unwrap_or(0),
                    y.get(i).copied().unwrap_or(0),
                )
            })
            .find(|(p, q)| p != q)
            .is_some_and(|(p, q)| p < q)
    }

    #[test]
    fn knuth_d_recovers_quotient() {
        let mut seed = 0x0123_4567_89AB_CDEFu64;
        for &(nq, nv) in &[
            (1, 1),
            (5, 1),
            (1, 2),
            (7, 2),
            (3, 9),
            (40, 17),
            (17, 40),
            (64, 64),
        ] {
            for _ in 0..20 {
                let q = rand_limbs(&mut seed, nq);
                let mut v = rand_limbs(&mut seed, nv);
                v[nv - 1] |= 1 << (LB - 1);
                let mut r = rand_limbs(&mut seed, nv);
                r[nv - 1] &= v[nv - 1] - 1;
                // u = q v + r in nq + nv + 1 limbs.
                let mut u = vec![0; nq + nv + 1];
                mul_basecase(
                    &mut u[..nq + nv],
                    if nq >= nv { &q } else { &v },
                    if nq >= nv { &v } else { &q },
                );
                add_in(&mut u, &r);
                let mut got = vec![0; nq + 1];
                knuth_d(&mut got, &mut u, &v);
                assert_eq!(&got[..nq], &q[..], "{nq} / {nv}");
                assert_eq!(got[nq], 0);
                assert_eq!(&u[..nv], &r[..]);
            }
        }
    }

    #[test]
    fn mul_divmod_round_trip() {
        let a = [0x9ABC_DEF7u32];
        let b = [0x8000_0001u32, 0xFFFF_FFFF];
        let (mut ab, mut bb, mut r) = (buf(&a, 1), buf(&b, 2), buf(&[], 3));
        let mut t = buf(&[], rok_nat_mul_scratch(1, 2));
        // SAFETY: test buffers sized above.
        unsafe {
            rok_nat_mul(
                r.as_mut_ptr().cast(),
                ab.as_mut_ptr().cast(),
                1,
                bb.as_mut_ptr().cast(),
                2,
                t.as_mut_ptr().cast(),
            );
        }
        let prod = read(&mut r, 3);
        assert_eq!(num(&prod), num(&a) * num(&b));
        // (prod + 5) / b == a, remainder 5 (`b` not normalized).
        let b = [5u32, 0x0000_0FFF];
        let bb = buf(&b, 2);
        let mut rb = buf(&[], 3);
        let mut t = buf(&[], rok_nat_mul_scratch(1, 2));
        // SAFETY: as above.
        unsafe {
            rok_nat_mul(
                rb.as_mut_ptr().cast(),
                ab.as_mut_ptr().cast(),
                1,
                bb.as_ptr().cast_mut().cast(),
                2,
                t.as_mut_ptr().cast(),
            );
        }
        let mut prod = read(&mut rb, 3);
        prod[0] += 5;
        let mut un = buf(&prod, 3);
        let (mut q, mut rem) = (buf(&[], 2), buf(&[], 2));
        let mut t = buf(&[], rok_nat_divmod_scratch(3, 2));
        let mut bb = bb;
        // SAFETY: as above.
        unsafe {
            rok_nat_divmod(
                q.as_mut_ptr().cast(),
                rem.as_mut_ptr().cast(),
                un.as_mut_ptr().cast(),
                3,
                bb.as_mut_ptr().cast(),
                2,
                t.as_mut_ptr().cast(),
            );
        }
        assert_eq!(num(&read(&mut q, 2)), num(&a));
        assert_eq!(num(&read(&mut rem, 2)), 5);
        let mut sq = buf(&[], 1);
        // SAFETY: as above.
        let rem =
            unsafe { rok_nat_divmod_small(sq.as_mut_ptr().cast(), ab.as_mut_ptr().cast(), 1, 10) };
        assert_eq!(num(&read(&mut sq, 1)) * 10 + u128::from(rem), num(&a));
    }
}
