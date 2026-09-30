//! Arithmetic whose best instruction depends on the target

/// `a * b + c`, as one instruction where the target has FMA
///
/// `mul_add` is a single fused multiply-add where the target has FMA and a
/// call into libm where it does not, which is far slower than the two
/// instructions it replaces - so the form is chosen at compile time
///
/// LLVM will not fuse `a * b + c` on its own: fusing rounds once instead of
/// twice, which changes the result, so it waits to be asked
#[expect(clippy::inline_always, reason = "one instruction; an outlined call costs more than it")]
#[inline(always)]
#[must_use]
pub const fn fma(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(target_feature = "fma")]
    {
        a.mul_add(b, c)
    }
    #[cfg(not(target_feature = "fma"))]
    {
        a * b + c
    }
}

/// `a` to `b` by `t`, fused: `(b - a) * t + a` rounds once
#[expect(clippy::inline_always, reason = "one instruction; an outlined call costs more than it")]
#[inline(always)]
#[must_use]
pub const fn lerp(a: f32, b: f32, t: f32) -> f32 {
    fma(b - a, t, a)
}
