//! Tiny deterministic PRNG for transforms that need arbitrary choices
//! (shuffle lines, sponge case, lorem ipsum).
//!
//! Seeded from a process-wide counter rather than the wall clock: `SystemTime`
//! is unreliable on `wasm32-unknown-unknown`, while a counter still makes
//! successive calls differ so repeated invocations don't produce identical
//! output. Randomness quality is irrelevant here — the output is decorative.

use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // SplitMix-style avalanche so consecutive seeds diverge immediately.
        let mut seed = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        seed ^= seed >> 30;
        seed = seed.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        seed ^= seed >> 27;
        Self(seed | 1)
    }

    /// xorshift64*, returning the high 32 bits.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Uniform-ish index in `0..n` (returns 0 when `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        self.next_u32() as usize % n
    }

    /// In-place Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}
