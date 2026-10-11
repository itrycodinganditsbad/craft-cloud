//! Parameter sets and the cut-point search.

/// A chunker parameter set. Only [`FASTCDC_V1`] is used for real files; tests use small sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Recorded in every manifest, e.g. `fastcdc-v1`.
    pub name: &'static str,
    /// BLAKE3 key-derivation context the gear table is generated from.
    pub gear_context: &'static str,
    /// No chunk is shorter, except the last one of a file.
    pub min: usize,
    /// Target average; the stricter mask applies before it, the looser one after.
    pub avg: usize,
    /// No chunk is longer.
    pub max: usize,
    /// Number of hash bits that must be zero before `avg` (log2(avg) + normalisation level).
    pub bits_before_avg: u32,
    /// Number of hash bits that must be zero after `avg` (log2(avg) − normalisation level).
    pub bits_after_avg: u32,
}

/// The parameter set for every file Craft Cloud stores: 256 KiB min, 1 MiB average, 4 MiB max,
/// normalisation level 1.
pub const FASTCDC_V1: Params = Params {
    name: "fastcdc-v1",
    gear_context: "craft-cloud 2026-10-11 fastcdc-v1 gear table",
    min: 256 * 1024,
    avg: 1024 * 1024,
    max: 4 * 1024 * 1024,
    bits_before_avg: 21,
    bits_after_avg: 19,
};

/// Why a parameter set is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamsError {
    #[error("sizes must satisfy 0 < min < avg < max")]
    BadSizes,
    #[error("mask bits must be 1 to 63")]
    BadBits,
}

/// A ready-to-use chunker: the parameters plus the generated gear table and masks.
#[derive(Debug, Clone)]
pub struct Chunker {
    params: Params,
    gear: Box<[u64; 256]>,
    mask_before_avg: u64,
    mask_after_avg: u64,
}

impl Chunker {
    pub fn new(params: Params) -> Result<Self, ParamsError> {
        if !(0 < params.min && params.min < params.avg && params.avg < params.max) {
            return Err(ParamsError::BadSizes);
        }
        Ok(Self {
            params,
            gear: gear_table(params.gear_context),
            mask_before_avg: top_bits_mask(params.bits_before_avg)?,
            mask_after_avg: top_bits_mask(params.bits_after_avg)?,
        })
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// The gear table: 256 pseudo-random words, one per byte value.
    pub fn gear(&self) -> &[u64; 256] {
        &self.gear
    }

    /// Length of the first chunk of `data`.
    ///
    /// `data` must be either at least `max` bytes long or the whole rest of the file; then the result is
    /// the same however the file was read. Returns 0 only for empty `data`.
    pub fn cut_point(&self, data: &[u8]) -> usize {
        let Params { min, avg, max, .. } = self.params;
        let len = data.len();
        if len <= min {
            return len;
        }
        let end = len.min(max);
        let normal = len.min(avg);
        let mut hash: u64 = 0;
        // Bytes before `min` are skipped entirely (the FastCDC "cut-point skipping" optimisation).
        for (i, &byte) in data.iter().enumerate().take(end).skip(min) {
            hash = (hash << 1).wrapping_add(self.gear[usize::from(byte)]);
            let mask = if i < normal { self.mask_before_avg } else { self.mask_after_avg };
            if hash & mask == 0 {
                return i + 1;
            }
        }
        end
    }
}

/// The top `bits` bits set. The gear hash shifts left, so its top bits depend on the most recent bytes
/// (up to 64 of them) and are the best mixed.
fn top_bits_mask(bits: u32) -> Result<u64, ParamsError> {
    if !(1..=63).contains(&bits) {
        return Err(ParamsError::BadBits);
    }
    u64::MAX.checked_shl(64 - bits).ok_or(ParamsError::BadBits)
}

/// 256 words from the BLAKE3 extendable output under `context`, read as little-endian `u64`s.
fn gear_table(context: &str) -> Box<[u64; 256]> {
    let mut xof = blake3::Hasher::new_derive_key(context).finalize_xof();
    let mut table = Box::new([0u64; 256]);
    let mut word = [0u8; 8];
    for slot in table.iter_mut() {
        xof.fill(&mut word);
        *slot = u64::from_le_bytes(word);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Params =
        Params { name: "test-small", gear_context: "craft-cloud test small", min: 64, avg: 256, max: 1024, bits_before_avg: 9, bits_after_avg: 7 };

    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn rejects_bad_params() {
        assert_eq!(Chunker::new(Params { min: 0, ..SMALL }).unwrap_err(), ParamsError::BadSizes);
        assert_eq!(Chunker::new(Params { avg: 64, ..SMALL }).unwrap_err(), ParamsError::BadSizes);
        assert_eq!(Chunker::new(Params { max: 256, ..SMALL }).unwrap_err(), ParamsError::BadSizes);
        assert_eq!(Chunker::new(Params { bits_before_avg: 0, ..SMALL }).unwrap_err(), ParamsError::BadBits);
        assert_eq!(Chunker::new(Params { bits_after_avg: 64, ..SMALL }).unwrap_err(), ParamsError::BadBits);
        assert!(Chunker::new(FASTCDC_V1).is_ok());
    }

    #[test]
    fn masks_have_the_top_bits_set() {
        let c = Chunker::new(SMALL).unwrap();
        assert_eq!(c.mask_before_avg, 0xff80_0000_0000_0000);
        assert_eq!(c.mask_after_avg, 0xfe00_0000_0000_0000);
    }

    #[test]
    fn short_input_is_one_chunk() {
        let c = Chunker::new(SMALL).unwrap();
        assert_eq!(c.cut_point(&[]), 0);
        assert_eq!(c.cut_point(&[7]), 1);
        assert_eq!(c.cut_point(&noise(64, 1)), 64);
    }

    #[test]
    fn cut_points_stay_within_min_and_max() {
        let c = Chunker::new(SMALL).unwrap();
        let data = noise(200_000, 2);
        let mut rest = data.as_slice();
        while !rest.is_empty() {
            let cut = c.cut_point(rest);
            assert!(cut >= 1);
            assert!(cut <= SMALL.max);
            if cut < rest.len() {
                assert!(cut > SMALL.min, "only the last chunk may be short");
            }
            rest = &rest[cut..];
        }
    }

    #[test]
    fn constant_data_is_cut_at_max() {
        let c = Chunker::new(SMALL).unwrap();
        assert_eq!(c.cut_point(&vec![0u8; 5000]), SMALL.max);
    }
}
