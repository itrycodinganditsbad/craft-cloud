//! Shared test helpers. Each test binary uses only some of them.
#![allow(dead_code)]

use std::io::{self, Read};

/// Deterministic pseudo-random bytes (xorshift64), the same on every platform and forever.
pub fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A reader that hands out at most `step` bytes per call and fails with `Interrupted` every third call.
pub struct Dribble<'a> {
    pub data: &'a [u8],
    pub step: usize,
    pub calls: usize,
}

impl Read for Dribble<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls.is_multiple_of(3) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let n = self.step.min(out.len()).min(self.data.len());
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}
