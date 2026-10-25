// SPDX-License-Identifier: Apache-2.0
//! The random draws of the tuning strategies, reproduced exactly: `std::mt19937` and Abseil's
//! `Uniform` (integer and float, closed-open) and `Bernoulli` over it.
//!
//! The strategies' results are functions of this stream: one draw out of step and every later
//! choice differs. Abseil fixes its algorithms (unlike `std::` distributions), so these are
//! transcriptions; `tests/rng.rs` pins them against a stream printed by Abseil itself.
//!
//! ⚠️ On the reference's platform `mt19937::result_type` is 64 bits wide while the engine's range
//! is 2^32, so Abseil's bit source takes ONE word for a 32-bit result and TWO (first word high) for
//! a 64-bit one; an empty range draws nothing.

/// `std::mt19937`.
#[derive(Debug, Clone)]
pub struct Mt19937 {
    state: [u32; Self::N],
    index: usize,
}

impl Mt19937 {
    const N: usize = 624;
    const M: usize = 397;

    /// `seed(value)`.
    pub fn new(seed: u32) -> Mt19937 {
        let mut state = [0u32; Self::N];
        state[0] = seed;
        for i in 1..Self::N {
            let prev = state[i - 1];
            state[i] = 1_812_433_253u32.wrapping_mul(prev ^ (prev >> 30)).wrapping_add(i as u32);
        }
        Mt19937 { state, index: Self::N }
    }

    fn twist(&mut self) {
        for i in 0..Self::N {
            let x = (self.state[i] & 0x8000_0000) | (self.state[(i + 1) % Self::N] & 0x7fff_ffff);
            let mut x_a = x >> 1;
            if x & 1 != 0 {
                x_a ^= 0x9908_b0df;
            }
            self.state[i] = self.state[(i + Self::M) % Self::N] ^ x_a;
        }
        self.index = 0;
    }

    /// One tempered word.
    pub fn word(&mut self) -> u32 {
        if self.index >= Self::N {
            self.twist();
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `FastUniformBits<uint64_t>`: two words, the first the high half.
    fn bits64(&mut self) -> u64 {
        let hi = u64::from(self.word());
        (hi << 32).wrapping_add(u64::from(self.word()))
    }

    /// `absl::Uniform<int>(g, 0, n)`: `[0, n)` through a 32-bit unsigned range; `0` without a draw
    /// when the range is empty.
    pub fn uniform_i32(&mut self, n: i32) -> i32 {
        if n <= 0 {
            return 0;
        }
        let r = (n - 1) as u32;
        let lim = r.wrapping_add(1);
        let mut bits = self.word();
        if r & lim == 0 {
            return (bits & r) as i32;
        }
        let mut product = u64::from(bits) * u64::from(lim);
        if (product as u32) < lim {
            let threshold = (u32::MAX - lim + 1) % lim;
            while (product as u32) < threshold {
                bits = self.word();
                product = u64::from(bits) * u64::from(lim);
            }
        }
        (product >> 32) as i32
    }

    /// `absl::Uniform<size_t>(g, 0, n)`: `[0, n)` through a 64-bit unsigned range; `0` without a
    /// draw when the range is empty.
    pub fn uniform_usize(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let r = (n - 1) as u64;
        let lim = r.wrapping_add(1);
        let mut bits = self.bits64();
        if r & lim == 0 {
            return (bits & r) as usize;
        }
        let mut product = u128::from(bits) * u128::from(lim);
        if (product as u64) < lim {
            let threshold = (u64::MAX - lim + 1) % lim;
            while (product as u64) < threshold {
                bits = self.bits64();
                product = u128::from(bits) * u128::from(lim);
            }
        }
        (product >> 64) as usize
    }

    /// `absl::Uniform<float>(g, 0, 1)`: 64 bits made a float in `[0, 1)` by their leading zeros
    /// (exponent) and the next 23 bits (mantissa), redrawn while the result is not below 1.
    pub fn uniform_f32(&mut self) -> f32 {
        loop {
            let sample = real_from_bits(self.bits64());
            let res = 0.0f32 + sample * 1.0f32;
            if res < 1.0 {
                return res;
            }
        }
    }

    /// `absl::Bernoulli(g, p)`: a 32-bit word against `p * 2^32`, the tie resolved by recursing on
    /// what `p` leaves over.
    pub fn bernoulli(&mut self, mut p: f64) -> bool {
        const P32: f64 = 4_294_967_296.0;
        loop {
            let c = (p * P32) as i64 as u64;
            let v = u64::from(self.word());
            if v != c {
                return v < c;
            }
            let q = c as f64 / P32;
            let here = (p - q) * P32;
            if here == 0.0 {
                return false;
            }
            p = here;
        }
    }
}

/// `GenerateRealFromBits<float, positive, include zero>`.
fn real_from_bits(bits: u64) -> f32 {
    if bits == 0 {
        return 0.0;
    }
    const K_EXP: u32 = f32::MANTISSA_DIGITS - 1;
    let clz = bits.leading_zeros();
    let exp = (f32::MAX_EXP - 2) - clz as i32;
    let b = (bits << clz) >> (63 - K_EXP);
    f32::from_bits(((exp as u32) << K_EXP) | (b as u32 & ((1 << K_EXP) - 1)))
}
