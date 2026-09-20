//! Arithmetic whose best instruction depends on the target

/// `a * b + c`, as one instruction where the target has FMA
///
/// `mul_add` is a single fused multiply-add with FMA available and a call into
/// libm without it, which is far slower than the two instructions it replaces.
/// The package build targets baseline x86-64 deliberately, so the choice is
/// made at compile time rather than assumed
///
/// LLVM will not fuse `a * b + c` on its own: fusing rounds once instead of
/// twice, which changes the result, so it waits to be asked
#[inline(always)]
#[must_use]
pub fn fma(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(target_feature = "fma")]
    {
        a.mul_add(b, c)
    }
    #[cfg(not(target_feature = "fma"))]
    {
        a * b + c
    }
}
