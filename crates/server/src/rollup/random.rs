// SPDX-License-Identifier: AGPL-3.0-only AND BSD-3-Clause
//
// Contains a port of MT19937 as Ruby carries it (ruby/missing/mt19937.c):
//
//   Copyright (C) 1997 - 2002, Makoto Matsumoto and Takuji Nishimura,
//   All rights reserved.
//
//   Redistribution and use in source and binary forms, with or without
//   modification, are permitted provided that the following conditions
//   are met:
//
//     1. Redistributions of source code must retain the above copyright
//        notice, this list of conditions and the following disclaimer.
//
//     2. Redistributions in binary form must reproduce the above copyright
//        notice, this list of conditions and the following disclaimer in the
//        documentation and/or other materials provided with the distribution.
//
//     3. The names of its contributors may not be used to endorse or promote
//        products derived from this software without specific prior written
//        permission.
//
//   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
//   "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
//   LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
//   A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE COPYRIGHT OWNER OR
//   CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
//   EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
//   PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
//   PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
//   LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
//   NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
//   SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//
// and of Array#shuffle / limited_rand from Ruby (array.c, random.c),
// Copyright (C) 1993-2013 Yukihiro Matsumoto, under the 2-clause BSD
// license (THIRD-PARTY-LICENSES, "Ruby").

//! Ruby's default random number generator, as far as tdigest 0.2.1 uses it:
//! MT19937 (ruby/missing/mt19937.c), `srand(Integer < 2**32)`
//! (rand_init -> init_genrand), and `Array#shuffle` (rb_ary_shuffle_bang with
//! RAND_UPTO -> limited_rand). Ported so a digest built here is byte-identical
//! to one built in Ruby from the same RNG state.

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UMASK: u32 = 0x8000_0000;
const LMASK: u32 = 0x7fff_ffff;

#[inline]
fn twist(u: u32, v: u32) -> u32 {
    (((u & UMASK) | (v & LMASK)) >> 1) ^ (if v & 1 != 0 { MATRIX_A } else { 0 })
}

pub struct RubyRandom {
    state: [u32; N],
    next: usize,
    left: usize,
}

impl RubyRandom {
    /// `srand(seed)` for a seed that fits in 32 bits (init_genrand).
    pub fn new(seed: u32) -> RubyRandom {
        let mut state = [0u32; N];
        state[0] = seed;
        for j in 1..N {
            state[j] = 1_812_433_253u32.wrapping_mul(state[j - 1] ^ (state[j - 1] >> 30)).wrapping_add(j as u32);
        }
        RubyRandom { state, next: N, left: 1 }
    }

    fn next_state(&mut self) {
        let s = &mut self.state;
        for p in 0..N - M {
            s[p] = s[p + M] ^ twist(s[p], s[p + 1]);
        }
        for p in N - M..N - 1 {
            s[p] = s[p + M - N] ^ twist(s[p], s[p + 1]);
        }
        s[N - 1] = s[M - 1] ^ twist(s[N - 1], s[0]);
        self.left = N;
        self.next = 0;
    }

    /// genrand_int32
    pub fn next_u32(&mut self) -> u32 {
        self.left -= 1;
        if self.left == 0 {
            self.next_state();
        }
        let mut y = self.state[self.next];
        self.next += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// limited_rand: uniform in [0, limit], by masking and rejection.
    pub fn limited(&mut self, limit: u32) -> u32 {
        if limit == 0 {
            return 0;
        }
        let mut mask = limit;
        mask |= mask >> 1;
        mask |= mask >> 2;
        mask |= mask >> 4;
        mask |= mask >> 8;
        mask |= mask >> 16;
        loop {
            let v = self.next_u32() & mask;
            if v <= limit {
                return v;
            }
        }
    }

    /// `Array#shuffle!` (Fisher-Yates from the end, j = RAND_UPTO(i)).
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        let mut i = v.len();
        assert!(i <= u32::MAX as usize, "shuffle of more than 2**32 elements");
        while i > 1 {
            let j = self.limited((i - 1) as u32) as usize;
            i -= 1;
            v.swap(i, j);
        }
    }
}
