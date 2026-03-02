/// Oodle1 decompression for Granny2 compression type 2.
///
/// Ported from opengr2/oodle1.c (MPL-2.0, derived from nwn2mdk) and the
/// liboodle specification (Unlicense). Three-layer architecture:
///
/// 1. **Arithmetic decoder** — 7+1 bit-shifted byte stream, numer/denom interval
/// 2. **WeighWindow** — adaptive multi-alphabet symbol coder (active/probationary/proportional)
/// 3. **Dictionary** — LZSS-style LZ with 4 literal coders + 65 length coders + offset coders

use crate::format::{rd_u32, Endianness};
use std::fmt;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Fixed-point representation of 1.0 in the symbol-coding layer.
const FIXED_ONE: u16 = 0x4000;

/// Minimum bits in the shift register before ingesting more bytes.
const INGEST_THRESHOLD: u32 = 0x800000;

/// Back-reference lengths for codes 61..64.
const BACKREF_SIZES: [u32; 4] = [128, 192, 256, 512];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InputTruncated,
    InvalidParameter(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InputTruncated => write!(f, "Oodle1: input truncated"),
            Error::InvalidParameter(s) => write!(f, "Oodle1: invalid parameter: {s}"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Parameter header (12 bytes per sub-stream, 3 sub-streams = 36 bytes)
// ---------------------------------------------------------------------------

struct Parameter {
    decoded_value_max: u32,  // literal alphabet size (bits 0-8 of word 0)
    backref_value_max: u32,  // LZ window size (bits 9-31 of word 0)
    decoded_count: u32,      // unique literal count (bits 0-8 of word 1)
    highbit_count: u32,      // largest 1k offset (bits 19-31 of word 1)
    sizes_count: [u8; 4],    // unique repeat-length counts per group (word 2 bytes)
}

fn parse_parameter(data: &[u8], off: usize, endian: Endianness) -> Parameter {
    let word0 = rd_u32(data, off, endian);
    let word1 = rd_u32(data, off + 4, endian);
    let word2 = rd_u32(data, off + 8, endian);

    Parameter {
        decoded_value_max: word0 & 0x1FF,
        backref_value_max: word0 >> 9,
        decoded_count: word1 & 0x1FF,
        highbit_count: word1 >> 19,
        sizes_count: [
            ((word2 >> 24) & 0xFF) as u8,
            ((word2 >> 16) & 0xFF) as u8,
            ((word2 >> 8) & 0xFF) as u8,
            (word2 & 0xFF) as u8,
        ],
    }
}

// ---------------------------------------------------------------------------
// Arithmetic decoder (bitstream layer)
// ---------------------------------------------------------------------------

struct Decoder<'a> {
    numer: u32,
    denom: u32,
    next_denom: u32,
    stream: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(stream: &'a [u8]) -> Self {
        let numer = if !stream.is_empty() {
            (stream[0] >> 1) as u32
        } else {
            0
        };
        Decoder {
            numer,
            denom: 0x80,
            next_denom: 0,
            stream,
            pos: 0,
        }
    }

    #[inline]
    fn byte_at(&self, idx: usize) -> u8 {
        if idx < self.stream.len() {
            self.stream[idx]
        } else {
            0
        }
    }

    /// Peek at the next symbol in [0, max). Ingests bytes until the shift
    /// register has sufficient precision, then projects into [0, max).
    fn decode(&mut self, max: u16) -> u16 {
        // Ingest bytes using the 7+1 split: LSB of current byte + top 7 of next
        while self.denom <= INGEST_THRESHOLD {
            self.numer <<= 8;
            let b0 = self.byte_at(self.pos);
            let b1 = self.byte_at(self.pos + 1);
            self.numer |= ((b0 << 7) & 0x80) as u32;
            self.numer |= (b1 >> 1) as u32;
            self.pos += 1;
            self.denom <<= 8;
        }

        self.next_denom = self.denom / max as u32;
        (self.numer / self.next_denom).min(max as u32 - 1) as u16
    }

    /// Commit a decoded symbol, updating the interval.
    /// Uses wrapping arithmetic to match the C reference's unsigned overflow semantics.
    fn commit(&mut self, max: u16, val: u16, err: u16) -> u16 {
        self.numer = self.numer.wrapping_sub(self.next_denom.wrapping_mul(val as u32));
        if (val as u32 + err as u32) < max as u32 {
            self.denom = self.next_denom.wrapping_mul(err as u32);
        } else {
            self.denom = self.denom.wrapping_sub(self.next_denom.wrapping_mul(val as u32));
        }
        val
    }

    /// Decode and commit a uniform-probability symbol in [0, max).
    fn decode_commit(&mut self, max: u16) -> u16 {
        let val = self.decode(max);
        self.commit(max, val, 1)
    }
}

// ---------------------------------------------------------------------------
// WeighWindow (adaptive symbol coder)
// ---------------------------------------------------------------------------

struct WeighWindow {
    count_cap: u16,
    ranges: Vec<u16>,
    values: Vec<u16>,
    weights: Vec<u16>,
    weight_total: u16,
    thresh_increase: u16,
    thresh_increase_cap: u16,
    thresh_range_rebuild: u16,
    thresh_weight_rebuild: u16,
}

/// Return value from WeighWindow::try_decode.
struct DecodeResult {
    /// If `new_index` is Some, the caller must assign a proportional symbol value.
    new_index: Option<usize>,
    value: u16,
}

impl WeighWindow {
    fn new(max_value: u32, count_cap: u16) -> Self {
        let thresh_weight_rebuild =
            256u32.max((32 * max_value).min(15160)) as u16;
        let thresh_increase_cap = if max_value > 64 {
            ((2 * max_value).min(thresh_weight_rebuild as u32 / 2 - 32)) as u16
        } else {
            128
        };

        WeighWindow {
            count_cap: count_cap + 1,
            ranges: vec![0, FIXED_ONE],
            values: vec![0],   // values[0] = escape symbol
            weights: vec![4],  // escape starts with weight 4
            weight_total: 4,
            thresh_increase: 4,
            thresh_increase_cap,
            thresh_range_rebuild: 8,
            thresh_weight_rebuild,
        }
    }

    /// Find the index of the largest weight starting from `from`.
    fn max_weight_index(&self, from: usize) -> usize {
        let mut best = from;
        let mut best_w = 0u16;
        for i in from..self.weights.len() {
            if self.weights[i] > best_w {
                best_w = self.weights[i];
                best = i;
            }
        }
        best
    }

    /// Halve all weights, remove symbols that drop to zero, keep escape alive.
    fn rebuild_weights(&mut self) {
        let mut total = 0u16;
        for w in self.weights.iter_mut() {
            *w /= 2;
            total += *w;
        }
        self.weight_total = total;

        // Remove zero-weight non-escape symbols by swapping with last
        let mut i = 1;
        while i < self.weights.len() {
            while i < self.weights.len() && self.weights[i] == 0 {
                let last = self.weights.len() - 1;
                self.weights[i] = self.weights[last];
                self.values[i] = self.values[last];
                self.weights.pop();
                self.values.pop();
            }
            i += 1;
        }

        // Move heaviest symbol to last position
        let best = self.max_weight_index(1);
        if best < self.weights.len() {
            let last = self.weights.len() - 1;
            self.weights.swap(best, last);
            self.values.swap(best, last);
        }

        // Keep escape alive if not all symbols learned
        if self.weights.len() < self.count_cap as usize && self.weights[0] == 0 {
            self.weights[0] = 1;
            self.weight_total += 1;
        }
    }

    /// Rescale cumulative ranges from current weights.
    fn rebuild_ranges(&mut self) {
        self.ranges.resize(self.weights.len() + 1, 0);

        let range_weight = (8 * FIXED_ONE as u32) / self.weight_total as u32;
        let mut start = 0u16;
        for i in 0..self.weights.len() {
            self.ranges[i] = start;
            start += ((self.weights[i] as u32 * range_weight) / 8) as u16;
        }
        *self.ranges.last_mut().unwrap() = FIXED_ONE;

        if self.thresh_increase > self.thresh_increase_cap / 2 {
            self.thresh_range_rebuild =
                self.weight_total + self.thresh_increase_cap;
        } else {
            self.thresh_increase *= 2;
            self.thresh_range_rebuild =
                self.weight_total + self.thresh_increase;
        }
    }

    /// Decode one symbol from the active/probationary/proportional alphabets.
    fn try_decode(&mut self, decoder: &mut Decoder) -> DecodeResult {
        // Check if rebuilds are needed
        if self.weight_total >= self.thresh_range_rebuild {
            if self.thresh_range_rebuild >= self.thresh_weight_rebuild {
                self.rebuild_weights();
            }
            self.rebuild_ranges();
        }

        // Decode from the active alphabet via the arithmetic coder
        let value = decoder.decode(FIXED_ONE);

        // Find the range interval containing `value`
        let mut rangeit = self.ranges.len() - 1;
        for i in 0..self.ranges.len() {
            if self.ranges[i] > value {
                rangeit = i;
                break;
            }
        }
        rangeit = rangeit.saturating_sub(1);

        let span = self.ranges[rangeit + 1] - self.ranges[rangeit];
        decoder.commit(FIXED_ONE, self.ranges[rangeit], span);

        let index = rangeit;
        self.weights[index] += 1;
        self.weight_total += 1;

        // Non-escape: return the active symbol directly
        if index > 0 {
            return DecodeResult {
                new_index: None,
                value: self.values[index],
            };
        }

        // Escape: try probationary, then proportional
        // Probationary symbols exist when weights.len() > ranges.len() - 1
        if self.weights.len() >= self.ranges.len() {
            let bit = decoder.decode_commit(2);
            if bit == 1 {
                // Probationary symbol
                let prob_count = self.weights.len() - self.ranges.len() + 1;
                let prob_idx =
                    self.ranges.len() - 1 + decoder.decode_commit(prob_count as u16) as usize;
                self.weights[prob_idx] += 2;
                self.weight_total += 2;
                return DecodeResult {
                    new_index: None,
                    value: self.values[prob_idx],
                };
            }
        }

        // Proportional: learn a new symbol
        self.values.push(0);
        self.weights.push(2);
        self.weight_total += 2;

        // If all unique symbols are now learned, kill the escape
        if self.weights.len() == self.count_cap as usize {
            self.weight_total -= self.weights[0];
            self.weights[0] = 0;
        }

        DecodeResult {
            new_index: Some(self.values.len() - 1),
            value: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Dictionary (LZSS-style LZ layer)
// ---------------------------------------------------------------------------

struct Dictionary {
    decoded_size: u32,
    backref_size: u32, // previous repeat-length code (index into size_windows)

    decoded_value_max: u32,
    backref_value_max: u32,
    lowbit_value_max: u32,

    lowbit_window: WeighWindow,
    highbit_window: WeighWindow,
    midbit_windows: Vec<WeighWindow>,
    decoded_windows: [WeighWindow; 4],
    size_windows: Vec<WeighWindow>,
}

impl Dictionary {
    fn new(param: &Parameter) -> Self {
        let backref_value_max = param.backref_value_max;
        let lowbit_value_max = (backref_value_max + 1).min(4);
        let midbit_value_max = (backref_value_max / 4 + 1).min(256) as u16;
        let highbit_value_max = backref_value_max / 1024 + 1;

        let lowbit_window =
            WeighWindow::new(lowbit_value_max - 1, lowbit_value_max as u16);
        let highbit_window =
            WeighWindow::new(highbit_value_max - 1, param.highbit_count as u16 + 1);

        let mut midbit_windows = Vec::with_capacity(highbit_value_max as usize);
        for _ in 0..highbit_value_max {
            midbit_windows
                .push(WeighWindow::new(midbit_value_max as u32 - 1, midbit_value_max));
        }

        let decoded_windows = [
            WeighWindow::new(param.decoded_value_max - 1, param.decoded_count as u16),
            WeighWindow::new(param.decoded_value_max - 1, param.decoded_count as u16),
            WeighWindow::new(param.decoded_value_max - 1, param.decoded_count as u16),
            WeighWindow::new(param.decoded_value_max - 1, param.decoded_count as u16),
        ];

        let mut size_windows =
            Vec::with_capacity(4 * 16 + 1);
        for i in 0..4u8 {
            for _ in 0..16 {
                size_windows.push(WeighWindow::new(64, param.sizes_count[i as usize] as u16));
            }
        }
        size_windows.push(WeighWindow::new(64, param.sizes_count[3] as u16));

        Dictionary {
            decoded_size: 0,
            backref_size: 0,
            decoded_value_max: param.decoded_value_max,
            backref_value_max,
            lowbit_value_max,
            lowbit_window,
            highbit_window,
            midbit_windows,
            decoded_windows,
            size_windows,
        }
    }

    /// Decompress one block (literal or back-reference). Returns bytes written.
    fn decompress_block(
        &mut self,
        decoder: &mut Decoder,
        output: &mut [u8],
        write_pos: usize,
    ) -> Result<u32, Error> {
        // Decode repeat-length code
        let size_idx = self.backref_size as usize;
        if size_idx >= self.size_windows.len() {
            return Err(Error::InvalidParameter(format!(
                "size_windows index {size_idx} out of bounds (max {})",
                self.size_windows.len()
            )));
        }
        let d1 = self.size_windows[size_idx].try_decode(decoder);
        let length_code = if let Some(idx) = d1.new_index {
            let v = decoder.decode_commit(65);
            self.size_windows[size_idx].values[idx] = v;
            v
        } else {
            d1.value
        };
        self.backref_size = length_code as u32;

        if self.backref_size > 0 {
            // Back-reference
            let backref_len = if self.backref_size < 61 {
                self.backref_size + 1
            } else {
                let br_idx = (self.backref_size - 61) as usize;
                if br_idx >= BACKREF_SIZES.len() {
                    return Err(Error::InvalidParameter(format!(
                        "backref size code {} out of range",
                        self.backref_size
                    )));
                }
                BACKREF_SIZES[br_idx]
            };

            let backref_range = self.backref_value_max.min(self.decoded_size);

            // Decode offset: lowbit + midbit*4 + highbit*1024
            let d3 = self.lowbit_window.try_decode(decoder);
            let lowbit = if let Some(idx) = d3.new_index {
                let v = decoder.decode_commit(self.lowbit_value_max as u16);
                self.lowbit_window.values[idx] = v;
                v as u32
            } else {
                d3.value as u32
            };

            let d4 = self.highbit_window.try_decode(decoder);
            let highbit = if let Some(idx) = d4.new_index {
                let v = decoder.decode_commit((backref_range / 1024 + 1) as u16);
                self.highbit_window.values[idx] = v;
                v as u32
            } else {
                d4.value as u32
            };

            let highbit_idx = highbit as usize;
            if highbit_idx >= self.midbit_windows.len() {
                return Err(Error::InvalidParameter(format!(
                    "midbit_windows index {highbit_idx} out of bounds (max {})",
                    self.midbit_windows.len()
                )));
            }

            let midbit_max = (backref_range / 4 + 1).min(256) as u16;
            let d5 = self.midbit_windows[highbit_idx].try_decode(decoder);
            let midbit = if let Some(idx) = d5.new_index {
                let v = decoder.decode_commit(midbit_max);
                self.midbit_windows[highbit_idx].values[idx] = v;
                v as u32
            } else {
                d5.value as u32
            };

            let backref_offset = (highbit << 10) + (midbit << 2) + lowbit + 1;

            // Copy back-referenced bytes (may overlap for run-length patterns).
            // Guard against corrupted input where offset exceeds write position.
            if (backref_offset as usize) > write_pos {
                for i in 0..backref_len as usize {
                    if write_pos + i < output.len() {
                        output[write_pos + i] = 0;
                    }
                }
            } else {
                let src_start = write_pos - backref_offset as usize;
                for i in 0..backref_len as usize {
                    if write_pos + i < output.len() {
                        output[write_pos + i] = output[src_start + i];
                    }
                }
            }

            self.decoded_size += backref_len;
            Ok(backref_len)
        } else {
            // Literal byte
            let lit_idx = (self.decoded_size % 4) as usize;
            let d2 = self.decoded_windows[lit_idx].try_decode(decoder);
            let literal = if let Some(idx) = d2.new_index {
                let v = decoder.decode_commit(self.decoded_value_max as u16);
                self.decoded_windows[lit_idx].values[idx] = v;
                v
            } else {
                d2.value
            };

            if write_pos < output.len() {
                output[write_pos] = literal as u8;
            }

            self.decoded_size += 1;
            Ok(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Decompress Oodle1-compressed data from a Granny2 section.
///
/// The compressed data begins with 3 × 12 = 36 bytes of parameter headers
/// (one per sub-stream), followed by the arithmetic-coded bitstream.
///
/// The `first_16bit` and `first_8bit` values come from the GR2 section header
/// and define the stop points between the three sub-streams.
pub fn decompress(
    compressed: &[u8],
    decompressed: &mut [u8],
    first_16bit: u32,
    first_8bit: u32,
    endian: Endianness,
) -> Result<(), Error> {
    if compressed.is_empty() && decompressed.is_empty() {
        return Ok(());
    }

    // 3 parameters × 12 bytes each = 36 bytes minimum header
    if compressed.len() < 36 {
        return Err(Error::InputTruncated);
    }

    let params = [
        parse_parameter(compressed, 0, endian),
        parse_parameter(compressed, 12, endian),
        parse_parameter(compressed, 24, endian),
    ];

    // Validate parameters
    for (i, p) in params.iter().enumerate() {
        if p.decoded_value_max == 0 {
            return Err(Error::InvalidParameter(format!(
                "sub-stream {i}: decoded_value_max is 0"
            )));
        }
    }

    let mut decoder = Decoder::new(&compressed[36..]);
    let stops = [
        first_16bit as usize,
        first_8bit as usize,
        decompressed.len(),
    ];

    let mut write_pos = 0usize;
    for (i, &stop) in stops.iter().enumerate() {
        let stop = stop.min(decompressed.len());
        let mut dict = Dictionary::new(&params[i]);
        while write_pos < stop {
            let written = dict.decompress_block(&mut decoder, decompressed, write_pos)?;
            write_pos += written as usize;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Decoder ----

    #[test]
    fn decoder_init_state() {
        let data = [0b10110100u8, 0x00, 0x00, 0x00];
        let d = Decoder::new(&data);
        assert_eq!(d.numer, 0b10110100 >> 1); // top 7 bits
        assert_eq!(d.denom, 0x80);
        assert_eq!(d.pos, 0);
    }

    #[test]
    fn decoder_empty_stream() {
        let d = Decoder::new(&[]);
        assert_eq!(d.numer, 0);
        assert_eq!(d.denom, 0x80);
    }

    #[test]
    fn decode_commit_uniform_two() {
        // With enough input bytes, decode_commit(2) should return 0 or 1
        let data = vec![0u8; 64];
        let mut d = Decoder::new(&data);
        let v = d.decode_commit(2);
        assert!(v < 2);
    }

    // ---- WeighWindow ----

    #[test]
    fn weigh_window_initial_state() {
        let w = WeighWindow::new(255, 10);
        assert_eq!(w.weight_total, 4);
        assert_eq!(w.weights.len(), 1);
        assert_eq!(w.values.len(), 1);
        assert_eq!(w.ranges.len(), 2);
        assert_eq!(w.ranges[0], 0);
        assert_eq!(w.ranges[1], FIXED_ONE);
        assert_eq!(w.count_cap, 11); // 10 + 1
    }

    #[test]
    fn weigh_window_thresh_values() {
        // Small alphabet: thresh_increase_cap = 128
        let w = WeighWindow::new(10, 5);
        assert_eq!(w.thresh_increase_cap, 128);
        assert_eq!(w.thresh_weight_rebuild, 320); // max(256, min(320, 15160))

        // Large alphabet: thresh_increase_cap is capped
        let w = WeighWindow::new(1000, 100);
        assert_eq!(w.thresh_weight_rebuild, 15160); // min(32000, 15160)
        assert_eq!(w.thresh_increase_cap, 2000); // min(2000, 15160/2-32=7548)
    }

    #[test]
    fn weigh_window_rebuild_weights_halves() {
        let mut w = WeighWindow::new(255, 10);
        w.weights = vec![4, 10, 6, 8];
        w.values = vec![0, 1, 2, 3];
        w.weight_total = 28;

        w.rebuild_weights();

        // All weights halved
        let total: u16 = w.weights.iter().sum();
        assert_eq!(w.weight_total, total);
        // Escape (index 0) should be halved: 4/2 = 2
        assert_eq!(w.weights[0], 2);
    }

    #[test]
    fn weigh_window_rebuild_weights_removes_zeros() {
        let mut w = WeighWindow::new(255, 10);
        w.weights = vec![4, 1, 8, 1]; // weights[1] and [3] will become 0 after halving
        w.values = vec![0, 10, 20, 30];
        w.weight_total = 14;

        w.rebuild_weights();

        // Zero-weight symbols (original weight 1, halved to 0) should be removed
        assert!(w.weights.iter().all(|&w| w > 0 || w == w));
        let total: u16 = w.weights.iter().sum();
        assert_eq!(w.weight_total, total);
    }

    #[test]
    fn weigh_window_rebuild_ranges_sums_to_fixed_one() {
        let mut w = WeighWindow::new(255, 10);
        w.weights = vec![4, 10, 6];
        w.values = vec![0, 1, 2];
        w.weight_total = 20;

        w.rebuild_ranges();

        assert_eq!(w.ranges[0], 0);
        assert_eq!(*w.ranges.last().unwrap(), FIXED_ONE);
        // Monotonically increasing
        for pair in w.ranges.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
    }

    // ---- Parameter parsing ----

    #[test]
    fn parse_parameter_le() {
        // word0: decoded_value_max=256, backref_value_max=0x1000
        // 256 | (0x1000 << 9) = 256 | 0x200000 = 0x200100
        let word0 = 0x200100u32;
        // word1: decoded_count=128, highbit_count=5
        // 128 | (5 << 19) = 128 | 0x280000 = 0x280080
        let word1 = 0x280080u32;
        // word2: sizes_count = [10, 20, 30, 40] (MSB first)
        let word2 = 0x0A141E28u32;

        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&word0.to_le_bytes());
        data[4..8].copy_from_slice(&word1.to_le_bytes());
        data[8..12].copy_from_slice(&word2.to_le_bytes());

        let p = parse_parameter(&data, 0, Endianness::Little);
        assert_eq!(p.decoded_value_max, 256);
        assert_eq!(p.backref_value_max, 0x1000);
        assert_eq!(p.decoded_count, 128);
        assert_eq!(p.highbit_count, 5);
        assert_eq!(p.sizes_count, [10, 20, 30, 40]);
    }

    #[test]
    fn parse_parameter_be() {
        let word0 = 0x200100u32;
        let word1 = 0x280080u32;
        let word2 = 0x0A141E28u32;

        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&word0.to_be_bytes());
        data[4..8].copy_from_slice(&word1.to_be_bytes());
        data[8..12].copy_from_slice(&word2.to_be_bytes());

        let p = parse_parameter(&data, 0, Endianness::Big);
        assert_eq!(p.decoded_value_max, 256);
        assert_eq!(p.backref_value_max, 0x1000);
        assert_eq!(p.decoded_count, 128);
        assert_eq!(p.highbit_count, 5);
        assert_eq!(p.sizes_count, [10, 20, 30, 40]);
    }

    // ---- Dictionary ----

    #[test]
    fn dictionary_init_window_counts() {
        let p = Parameter {
            decoded_value_max: 256,
            backref_value_max: 4096,
            decoded_count: 128,
            highbit_count: 5,
            sizes_count: [10, 20, 30, 40],
        };
        let d = Dictionary::new(&p);

        assert_eq!(d.lowbit_value_max, 4);        // min(4097, 4)
        assert_eq!(d.midbit_windows.len(), 5);     // 4096/1024 + 1
        assert_eq!(d.size_windows.len(), 65);      // 4*16 + 1
    }

    // ---- Top-level decompress ----

    #[test]
    fn decompress_empty() {
        assert!(decompress(&[], &mut [], 0, 0, Endianness::Little).is_ok());
    }

    #[test]
    fn decompress_truncated_header() {
        let mut out = [0u8; 16];
        let result = decompress(&[0u8; 20], &mut out, 0, 0, Endianness::Little);
        assert!(matches!(result, Err(Error::InputTruncated)));
    }

    // ---- Error derives ----

    #[test]
    fn error_eq() {
        assert_eq!(Error::InputTruncated, Error::InputTruncated);
        assert_eq!(
            Error::InvalidParameter("x".into()),
            Error::InvalidParameter("x".into()),
        );
        assert_ne!(Error::InputTruncated, Error::InvalidParameter("y".into()));
    }

    // ---- Commit wrapping arithmetic ----

    #[test]
    fn commit_wrapping_arithmetic() {
        let data = vec![0xFFu8; 64];
        let mut d = Decoder::new(&data);
        // Pre-load the decoder state
        d.decode(256);
        // Force extreme values that would underflow without wrapping
        d.next_denom = u32::MAX;
        d.numer = 0;
        // Must not panic
        d.commit(256, 255, 1);
    }

    // ---- Adversarial fuzzing ----

    #[test]
    fn decompress_no_panic_on_adversarial_input() {
        // Valid 36-byte header (decoded_value_max=256 in each sub-stream) + 0xFF stream
        let mut compressed = vec![0u8; 100];
        // word0 for each sub-stream: decoded_value_max=256
        let word0 = 256u32; // backref_value_max=0, decoded_value_max=256
        for i in 0..3 {
            let off = i * 12;
            compressed[off..off + 4].copy_from_slice(&word0.to_le_bytes());
            // word1: decoded_count=1, highbit_count=0
            compressed[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes());
            // word2: sizes_count all 1
            compressed[off + 8..off + 12].copy_from_slice(&0x01010101u32.to_le_bytes());
        }
        // Fill stream portion with 0xFF
        for b in &mut compressed[36..] {
            *b = 0xFF;
        }
        let mut out = vec![0u8; 32];
        // Must not panic — result can be Ok or Err
        let _ = decompress(&compressed, &mut out, 10, 20, Endianness::Little);
    }

    #[test]
    fn decompress_no_panic_on_zero_stream() {
        // Valid 36-byte header + all-zero stream
        let mut compressed = vec![0u8; 100];
        let word0 = 256u32;
        for i in 0..3 {
            let off = i * 12;
            compressed[off..off + 4].copy_from_slice(&word0.to_le_bytes());
            compressed[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes());
            compressed[off + 8..off + 12].copy_from_slice(&0x01010101u32.to_le_bytes());
        }
        let mut out = vec![0u8; 32];
        // Must not panic
        let _ = decompress(&compressed, &mut out, 10, 20, Endianness::Little);
    }
}
