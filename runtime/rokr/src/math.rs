//! libm behind `rok_`-prefixed symbols for core's `f32` / `f64` methods.
//! core cannot declare `floor` / `log` itself: an
//! extern's name is global in rok, so a user `fn log` would clash. The
//! driver links `-lm` (the UCRT on Windows). `abs` / `copysign` / `min`
//! / `max` are Rust's `core` (bit operations; the UCRT does not export
//! every `f32` form on every architecture).

macro_rules! libm {
    ($($rok:ident = $c:ident($($a:ident),+) : $t:ty;)*) => {
        unsafe extern "C" {
            $(fn $c($($a: $t),+) -> $t;)*
        }
        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn $rok($($a: $t),+) -> $t {
                // SAFETY: libm math functions are pure on their arguments.
                unsafe { $c($($a),+) }
            }
        )*
    };
}

// C89's `float` forms: x86's UCRT has none (MSVC's headers widen them
// to the `double` ones, as here).
macro_rules! libm_f32 {
    ($($rok:ident = $c:ident / $d:ident($($a:ident),+);)*) => {
        #[cfg(not(all(windows, target_arch = "x86")))]
        libm! { $($rok = $c($($a),+): f32;)* }
        $(
            #[cfg(all(windows, target_arch = "x86"))]
            #[unsafe(no_mangle)]
            pub extern "C" fn $rok($($a: f32),+) -> f32 {
                unsafe extern "C" {
                    fn $d($($a: f64),+) -> f64;
                }
                // SAFETY: as in `libm!`.
                unsafe { $d($(f64::from($a)),+) as f32 }
            }
        )*
    };
}

libm_f32! {
    rok_f32_floor = floorf / floor(x);
    rok_f32_ceil = ceilf / ceil(x);
    rok_f32_exp = expf / exp(x);
    rok_f32_ln = logf / log(x);
    rok_f32_log10 = log10f / log10(x);
    rok_f32_sin = sinf / sin(x);
    rok_f32_cos = cosf / cos(x);
    rok_f32_tan = tanf / tan(x);
    rok_f32_asin = asinf / asin(x);
    rok_f32_acos = acosf / acos(x);
    rok_f32_atan = atanf / atan(x);
    rok_f32_sinh = sinhf / sinh(x);
    rok_f32_cosh = coshf / cosh(x);
    rok_f32_tanh = tanhf / tanh(x);
    rok_f32_atan2 = atan2f / atan2(x, y);
}

libm! {
    rok_f64_floor = floor(x): f64;
    rok_f64_ceil = ceil(x): f64;
    rok_f64_round = round(x): f64;
    rok_f64_trunc = trunc(x): f64;
    rok_f64_cbrt = cbrt(x): f64;
    rok_f64_exp = exp(x): f64;
    rok_f64_exp2 = exp2(x): f64;
    rok_f64_exp_m1 = expm1(x): f64;
    rok_f64_ln = log(x): f64;
    rok_f64_ln_1p = log1p(x): f64;
    rok_f64_log2 = log2(x): f64;
    rok_f64_log10 = log10(x): f64;
    rok_f64_sin = sin(x): f64;
    rok_f64_cos = cos(x): f64;
    rok_f64_tan = tan(x): f64;
    rok_f64_asin = asin(x): f64;
    rok_f64_acos = acos(x): f64;
    rok_f64_atan = atan(x): f64;
    rok_f64_sinh = sinh(x): f64;
    rok_f64_cosh = cosh(x): f64;
    rok_f64_tanh = tanh(x): f64;
    rok_f64_asinh = asinh(x): f64;
    rok_f64_acosh = acosh(x): f64;
    rok_f64_atanh = atanh(x): f64;
    rok_f64_pow = pow(x, y): f64;
    rok_f64_atan2 = atan2(x, y): f64;
    rok_f64_hypot = hypot(x, y): f64;
    rok_f64_mul_add = fma(x, y, z): f64;

    rok_f32_round = roundf(x): f32;
    rok_f32_trunc = truncf(x): f32;
    rok_f32_cbrt = cbrtf(x): f32;
    rok_f32_exp2 = exp2f(x): f32;
    rok_f32_exp_m1 = expm1f(x): f32;
    rok_f32_ln_1p = log1pf(x): f32;
    rok_f32_log2 = log2f(x): f32;
    rok_f32_asinh = asinhf(x): f32;
    rok_f32_acosh = acoshf(x): f32;
    rok_f32_atanh = atanhf(x): f32;
    rok_f32_pow = powf(x, y): f32;
    rok_f32_mul_add = fmaf(x, y, z): f32;
}

// `llvm.powi`, which `core` cannot reach: compiler-builtins' functions
// that it lowers to (repeated multiplication, no libm call).
unsafe extern "C" {
    fn __powidf2(a: f64, b: i32) -> f64;
    fn __powisf2(a: f32, b: i32) -> f32;
}
#[unsafe(no_mangle)]
pub extern "C" fn rok_f64_powi(x: f64, n: i32) -> f64 {
    // SAFETY: pure on its arguments.
    unsafe { __powidf2(x, n) }
}
#[unsafe(no_mangle)]
pub extern "C" fn rok_f32_powi(x: f32, n: i32) -> f32 {
    // SAFETY: pure on its arguments.
    unsafe { __powisf2(x, n) }
}

// Through `hypot`: exact in `f64`, so rounding it gives `hypotf`'s
// result (the UCRT has only `_hypotf`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_f32_hypot(x: f32, y: f32) -> f32 {
    rok_f64_hypot(f64::from(x), f64::from(y)) as f32
}

// `min` / `max` ignore a NaN operand, as C's `fmin` / `fmax`.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f64_abs(x: f64) -> f64 {
    x.abs()
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f64_copysign(x: f64, y: f64) -> f64 {
    x.copysign(y)
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f64_min(x: f64, y: f64) -> f64 {
    x.min(y)
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f64_max(x: f64, y: f64) -> f64 {
    x.max(y)
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f32_abs(x: f32) -> f32 {
    x.abs()
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f32_copysign(x: f32, y: f32) -> f32 {
    x.copysign(y)
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f32_min(x: f32, y: f32) -> f32 {
    x.min(y)
}
#[unsafe(no_mangle)]
pub const extern "C" fn rok_f32_max(x: f32, y: f32) -> f32 {
    x.max(y)
}
