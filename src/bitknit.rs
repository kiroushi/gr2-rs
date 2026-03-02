/// BitKnit decompression for Granny2 compression type 3/4.
///
/// Ported from neptuwunium/Knit (C#), derived from eiz/pybg3 (Rust/C).
/// BitKnit is a dual-state interleaved rANS + LZ codec by RAD Game Tools.
///
/// Key insight: the Granny2 BitKnit stream format differs from Oodle's BitKnit.
/// The stream is consumed as u16 words and the initial rANS state setup uses
/// big-endian word ordering. This implementation follows the Granny2 variant.
///
/// **Note on endianness**: The BitKnit compressed stream is always LE u16 words
/// regardless of the GR2 file's endianness. The decompressed data preserves the
/// file's native byte order — BitKnit is a byte-level codec.

use std::fmt;

const BITKNIT_MAGIC: u16 = 0x75B1;

// ---------------------------------------------------------------------------
// Frequency table (rANS CDF acceleration)
// ---------------------------------------------------------------------------

struct FrequencyTable {
    frequency_bits: u32,
    vocab_size: usize,
    lookup_shift: u32,
    sums: Vec<u16>,
    lookup: Vec<u16>,
}

impl FrequencyTable {
    fn new(frequency_bits: u32, vocab_size: usize, lookup_bits: u32) -> Self {
        FrequencyTable {
            frequency_bits,
            vocab_size,
            lookup_shift: frequency_bits - lookup_bits,
            sums: vec![0; vocab_size + 1],
            lookup: vec![0; 1 << lookup_bits],
        }
    }

    fn find_symbol(&self, code: u32) -> u16 {
        let mut sym = self.lookup[(code >> self.lookup_shift) as usize];
        while (sym as usize) < self.vocab_size
            && code >= self.sums[sym as usize + 1] as u32
        {
            sym += 1;
        }
        sym.min(self.vocab_size.saturating_sub(1) as u16)
    }

    fn finish_update(&mut self) {
        let mut code = 0u32;
        let mut sym = 0u16;
        let total = 1u32 << self.frequency_bits;
        let step = 1u32 << self.lookup_shift;
        let mut next = self.sums[1] as u32;
        while code < total {
            if code < next {
                self.lookup[(code >> self.lookup_shift) as usize] = sym;
                code += step;
            } else {
                sym += 1;
                if sym as usize >= self.vocab_size {
                    break;
                }
                next = self.sums[sym as usize + 1] as u32;
            }
        }
    }

    #[inline]
    fn frequency(&self, sym: u16) -> u16 {
        self.sums[sym as usize + 1] - self.sums[sym as usize]
    }

    #[inline]
    fn sum_below(&self, sym: u16) -> u16 {
        self.sums[sym as usize]
    }
}

// ---------------------------------------------------------------------------
// Deferred adaptive probability model
// ---------------------------------------------------------------------------

struct AdaptiveModel {
    cdf: FrequencyTable,
    freq_accum: Vec<u16>,
    adaptation_interval: u32,
    freq_incr: u16,
    last_freq_incr: u16,
    counter: u32,
}

impl AdaptiveModel {
    fn new(
        adaptation_interval: u32,
        vocab_size: usize,
        num_min_probable: usize,
        frequency_bits: u32,
        lookup_bits: u32,
    ) -> Self {
        let num_equiprobable = vocab_size - num_min_probable;
        let total_sum = 1u32 << frequency_bits;
        let freq_incr =
            ((total_sum - vocab_size as u32) / adaptation_interval) as u16;
        let last_freq_incr = (1 + total_sum - vocab_size as u32
            - freq_incr as u32 * adaptation_interval) as u16;

        let mut cdf = FrequencyTable::new(frequency_bits, vocab_size, lookup_bits);

        // Initialize CDF: equiprobable symbols get evenly-spaced sums,
        // min-probable symbols get single-unit spacing at the end.
        for i in 0..num_equiprobable {
            cdf.sums[i] = ((total_sum - num_min_probable as u32) * i as u32
                / num_equiprobable as u32) as u16;
        }
        for i in num_equiprobable..=vocab_size {
            cdf.sums[i] = (total_sum - vocab_size as u32 + i as u32) as u16;
        }
        cdf.finish_update();

        AdaptiveModel {
            cdf,
            freq_accum: vec![1; vocab_size],
            adaptation_interval,
            freq_incr,
            last_freq_incr,
            counter: 0,
        }
    }

    fn observe_symbol(&mut self, sym: u16) {
        let idx = sym as usize;
        if idx >= self.freq_accum.len() {
            return; // corrupted symbol — ignore silently
        }
        self.freq_accum[idx] += self.freq_incr;
        self.counter = (self.counter + 1) % self.adaptation_interval;
        if self.counter == 0 {
            self.freq_accum[idx] += self.last_freq_incr;
            let mut sum = 0i32;
            for i in 0..self.cdf.vocab_size {
                sum += self.freq_accum[i] as i32;
                let old = self.cdf.sums[i + 1] as i32;
                // C# uses uint subtraction (wraps) + long division + ushort cast,
                // which is equivalent to floor division by 2 (NOT truncation).
                // Arithmetic right shift on i32 gives floor division.
                self.cdf.sums[i + 1] = (old + ((sum - old) >> 1)) as u16;
                self.freq_accum[i] = 1;
            }
            self.cdf.finish_update();
        }
    }
}

// ---------------------------------------------------------------------------
// rANS state
// ---------------------------------------------------------------------------

struct RansState {
    bits: u32,
}

impl RansState {
    fn new() -> Self {
        RansState { bits: 0x10000 }
    }

    #[inline]
    fn pop_bits(&mut self, stream: &[u16], pos: &mut usize, nbits: u32) -> u32 {
        let sym = self.bits & ((1u32 << nbits) - 1);
        self.bits >>= nbits;
        self.maybe_refill(stream, pos);
        sym
    }

    #[inline]
    fn pop_cdf(&mut self, stream: &[u16], pos: &mut usize, cdf: &FrequencyTable) -> u16 {
        let code = self.bits & ((1u32 << cdf.frequency_bits) - 1);
        let sym = cdf.find_symbol(code);
        let freq = cdf.frequency(sym) as u32;
        let sum_below = cdf.sum_below(sym) as u32;
        // C# uses u32 wrapping arithmetic; we use u64 to avoid debug-mode overflow
        // panics. The truncation to u32 gives identical results.
        let raw = (self.bits >> cdf.frequency_bits) as u64 * freq as u64
            + code as u64 - sum_below as u64;
        self.bits = raw as u32;
        self.maybe_refill(stream, pos);
        sym
    }

    #[inline]
    fn maybe_refill(&mut self, stream: &[u16], pos: &mut usize) {
        if self.bits < 0x10000 {
            if *pos < stream.len() {
                self.bits = (self.bits << 16) | stream[*pos] as u32;
                *pos += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8-entry LRU distance cache (packed nibble register)
// ---------------------------------------------------------------------------

struct LruCache {
    entries: [u32; 8],
    order: u32,
}

impl LruCache {
    fn new() -> Self {
        LruCache {
            entries: [1; 8],
            order: 0x76543210,
        }
    }

    fn insert(&mut self, value: u32) {
        let idx7 = (self.order >> 28) as usize;
        let idx6 = ((self.order >> 24) & 0xF) as usize;
        self.entries[idx7] = self.entries[idx6];
        self.entries[idx6] = value;
    }

    fn hit(&mut self, index: u32) -> u32 {
        let slot = (self.order >> (index * 4)) & 0xF;
        // C# relies on wrapping: 16u << 28 = 0 in uint32, then 0 - 1 = 0xFFFFFFFF
        let rotate_mask = (16u32.wrapping_shl(index * 4)).wrapping_sub(1);
        let rotated = ((self.order << 4) | slot) & rotate_mask;
        self.order = (self.order & !rotate_mask) | rotated;
        self.entries[slot as usize]
    }
}

// ---------------------------------------------------------------------------
// BitKnit decoder
// ---------------------------------------------------------------------------

struct Decoder {
    command_models: [AdaptiveModel; 4],
    cache_ref_models: [AdaptiveModel; 4],
    offset_model: AdaptiveModel,
    offset_cache: LruCache,
    delta_offset: usize,
}

impl Decoder {
    fn new() -> Self {
        Decoder {
            command_models: [
                AdaptiveModel::new(1024, 300, 36, 15, 10),
                AdaptiveModel::new(1024, 300, 36, 15, 10),
                AdaptiveModel::new(1024, 300, 36, 15, 10),
                AdaptiveModel::new(1024, 300, 36, 15, 10),
            ],
            cache_ref_models: [
                AdaptiveModel::new(1024, 40, 0, 15, 10),
                AdaptiveModel::new(1024, 40, 0, 15, 10),
                AdaptiveModel::new(1024, 40, 0, 15, 10),
                AdaptiveModel::new(1024, 40, 0, 15, 10),
            ],
            offset_model: AdaptiveModel::new(1024, 21, 0, 15, 10),
            offset_cache: LruCache::new(),
            delta_offset: 1,
        }
    }

    #[inline]
    fn pop_bits(
        stream: &[u16],
        pos: &mut usize,
        nbits: u32,
        s1: &mut RansState,
        s2: &mut RansState,
    ) -> u32 {
        let result = s1.pop_bits(stream, pos, nbits);
        std::mem::swap(s1, s2);
        result
    }

    #[inline]
    fn pop_model(
        stream: &[u16],
        pos: &mut usize,
        model: &mut AdaptiveModel,
        s1: &mut RansState,
        s2: &mut RansState,
    ) -> u32 {
        let sym = s1.pop_cdf(stream, pos, &model.cdf);
        model.observe_symbol(sym);
        std::mem::swap(s1, s2);
        sym as u32
    }

    fn decode_initial_state(
        stream: &[u16],
        pos: &mut usize,
        s1: &mut RansState,
        s2: &mut RansState,
    ) {
        let init_0 = pop_u16(stream, pos) as u32;
        let init_1 = pop_u16(stream, pos) as u32;
        let mut merged = RansState {
            bits: (init_0 << 16) | init_1,
        };

        let split = merged.pop_bits(stream, pos, 4);

        s1.bits = merged.bits >> split;
        s1.maybe_refill(stream, pos);

        s2.bits = (merged.bits << 16) | pop_u16(stream, pos) as u32;
        s2.bits &= (1u32 << (16 + split)) - 1;
        s2.bits |= 1u32 << (16 + split);
    }

    fn decode_quantum(
        &mut self,
        stream: &[u16],
        pos: &mut usize,
        dst: &mut [u8],
        offset: &mut usize,
    ) -> Result<(), Error> {
        let boundary = dst.len().min((*offset & 0xFFFF_0000) + 0x10000);

        // Uncompressed quantum: first u16 is 0
        if *pos < stream.len() && stream[*pos] == 0 {
            *pos += 1;
            let copy_len = ((stream.len() - *pos) * 2).min(boundary - *offset);
            let src_bytes = u16_slice_to_bytes(&stream[*pos..]);
            let n = copy_len.min(src_bytes.len());
            dst[*offset..*offset + n].copy_from_slice(&src_bytes[..n]);
            *offset += copy_len;
            *pos += copy_len / 2;
            return Ok(());
        }

        let mut s1 = RansState::new();
        let mut s2 = RansState::new();
        Self::decode_initial_state(stream, pos, &mut s1, &mut s2);

        // First byte of the entire stream is a raw literal (not delta-coded)
        if *offset == 0 {
            dst[0] = Self::pop_bits(stream, pos, 8, &mut s1, &mut s2) as u8;
            *offset = 1;
        }

        while *offset < boundary {
            let model_idx = *offset % 4;

            let command = Self::pop_model(
                stream, pos, &mut self.command_models[model_idx], &mut s1, &mut s2,
            );

            if command >= 256 {
                self.decode_copy(stream, pos, command, &mut s1, &mut s2, dst, offset);
            } else {
                let prev = if *offset >= self.delta_offset {
                    dst[*offset - self.delta_offset]
                } else {
                    0
                };
                dst[*offset] = (command as u8).wrapping_add(prev);
                *offset += 1;
            }
        }

        // Quantum integrity: one rANS state should be at the refill threshold
        if s1.bits != 0x10000 && s2.bits != 0x10000 {
            return Err(Error::InvalidStream("rANS stream corrupted at quantum end"));
        }

        Ok(())
    }

    fn decode_copy(
        &mut self,
        stream: &[u16],
        pos: &mut usize,
        command: u32,
        s1: &mut RansState,
        s2: &mut RansState,
        dst: &mut [u8],
        offset: &mut usize,
    ) {
        let model_idx = *offset % 4;

        let copy_length = if command < 288 {
            command - 254
        } else {
            let nb = command - 287;
            let extra = Self::pop_bits(stream, pos, nb, s1, s2);
            (1u32 << nb) + extra + 32
        };

        let cache_ref = Self::pop_model(
            stream, pos, &mut self.cache_ref_models[model_idx], s1, s2,
        );

        let copy_offset = if cache_ref < 8 {
            self.offset_cache.hit(cache_ref)
        } else {
            let offset_length =
                Self::pop_model(stream, pos, &mut self.offset_model, s1, s2);
            let nb = offset_length % 16;
            let mut offset_bits = Self::pop_bits(stream, pos, nb, s1, s2);
            if offset_length >= 16 {
                let raw = pop_u16(stream, pos);
                offset_bits = (offset_bits << 16) | raw as u32;
            }
            let dist =
                (32u32 << offset_length) + (offset_bits << 5) - 32 + (cache_ref - 7);
            self.offset_cache.insert(dist);
            dist
        };

        self.delta_offset = copy_offset as usize;

        // Copy match bytes — may cross quantum boundary (matches C# behavior)
        for _ in 0..copy_length {
            if *offset >= dst.len() {
                break;
            }
            if (copy_offset as usize) <= *offset {
                dst[*offset] = dst[*offset - copy_offset as usize];
            } else {
                dst[*offset] = 0;
            }
            *offset += 1;
        }
    }
}

#[inline]
fn pop_u16(stream: &[u16], pos: &mut usize) -> u16 {
    if *pos < stream.len() {
        let v = stream[*pos];
        *pos += 1;
        v
    } else {
        0
    }
}

fn u16_slice_to_bytes(s: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for &w in s {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- FrequencyTable (via AdaptiveModel) ----

    #[test]
    fn frequency_table_initial_cdf_sum() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        assert_eq!(m.cdf.sums[300] as u32, 1 << 15,
            "CDF total must equal 2^freq_bits");
    }

    #[test]
    fn frequency_table_monotonic() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        for i in 0..300 {
            assert!(m.cdf.sums[i] < m.cdf.sums[i + 1],
                "CDF must be strictly monotonic: sums[{i}]={} >= sums[{}]={}",
                m.cdf.sums[i], i + 1, m.cdf.sums[i + 1]);
        }
    }

    #[test]
    fn frequency_table_lookup_agrees_with_linear_scan() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        // Spot-check every 127th code value across the full range
        for code in (0..32768u32).step_by(127) {
            let sym_fast = m.cdf.find_symbol(code);
            let mut sym_linear = 0u16;
            while (sym_linear as usize) < 300
                && code >= m.cdf.sums[sym_linear as usize + 1] as u32
            {
                sym_linear += 1;
            }
            assert_eq!(sym_fast, sym_linear, "lookup mismatch at code={code}");
        }
    }

    #[test]
    fn frequency_table_boundary_symbols() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        assert_eq!(m.cdf.find_symbol(0), 0);
        assert_eq!(m.cdf.find_symbol(32767), 299);
    }

    // ---- AdaptiveModel ----

    #[test]
    fn adaptive_model_initial_cdf_valid() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        assert_eq!(m.cdf.sums[0], 0);
        assert_eq!(m.cdf.sums[m.cdf.vocab_size] as u32, 1 << 15);
        for i in 0..m.cdf.vocab_size {
            assert!(m.cdf.sums[i] < m.cdf.sums[i + 1]);
        }
    }

    #[test]
    fn adaptive_model_freq_conservation() {
        let m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        let total = 1u32 << 15;
        let lhs = m.freq_incr as u32 * m.adaptation_interval + m.last_freq_incr as u32;
        let rhs = total - 300 + 1;
        assert_eq!(lhs, rhs,
            "frequency increments must conserve total probability mass");
    }

    #[test]
    fn adaptive_model_adaptation_cycle() {
        let mut m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        let old_sums: Vec<u16> = m.cdf.sums.clone();

        for _ in 0..1024 {
            m.observe_symbol(0);
        }

        assert_eq!(m.counter, 0, "counter must reset after adaptation");
        assert!(m.freq_accum.iter().all(|&f| f == 1),
            "freq_accum must reset to 1 after adaptation");
        assert_ne!(m.cdf.sums, old_sums,
            "CDF must change after adaptation with skewed observations");
    }

    #[test]
    fn adaptive_model_floor_division() {
        // The adaptation loop uses `(sum - old) >> 1` (arithmetic right shift =
        // floor division). For odd negative deltas this differs from truncation
        // (`/ 2`) by 1. This test constructs a scenario with provably odd
        // negative deltas and verifies the correct result.
        let mut m = AdaptiveModel::new(1024, 4, 0, 15, 10);

        // Cycle 1: all observations on symbol 0 → biases CDF toward symbol 0
        for _ in 0..1024 {
            m.observe_symbol(0);
        }

        // Cycle 2: all observations on symbol 3 → creates negative deltas
        // for symbols 1 and 2 whose CDF values must decrease
        for _ in 0..1024 {
            m.observe_symbol(3);
        }

        // After cycle 2, symbol 2 has an odd negative delta:
        //   sum = 2, old = 24575, delta = -24573
        //   Floor: 24575 + (-24573 >> 1) = 24575 + (-12287) = 12288
        //   Trunc: 24575 + (-24573 / 2)  = 24575 + (-12286) = 12289
        assert_eq!(m.cdf.sums[2], 12288,
            "must use floor division (>>1), not truncation (/2)");
    }

    #[test]
    fn adaptive_model_cdf_valid_after_adaptation() {
        let mut m = AdaptiveModel::new(1024, 300, 36, 15, 10);
        // Heavily skew toward symbol 0
        for _ in 0..1024 {
            m.observe_symbol(0);
        }
        assert_eq!(m.cdf.sums[0], 0);
        assert_eq!(m.cdf.sums[300] as u32, 1 << 15);
        for i in 0..300 {
            assert!(m.cdf.sums[i] < m.cdf.sums[i + 1],
                "CDF not monotonic after adaptation at index {i}");
        }
    }

    // ---- RansState ----

    #[test]
    fn rans_refill_from_stream() {
        let mut s = RansState { bits: 1 };
        let stream = [0xABCDu16];
        let mut pos = 0;
        s.maybe_refill(&stream, &mut pos);
        assert_eq!(s.bits, (1 << 16) | 0xABCD);
        assert_eq!(pos, 1);
    }

    #[test]
    fn rans_no_refill_when_above_threshold() {
        let mut s = RansState { bits: 0x20000 };
        let stream = [0xFFFFu16];
        let mut pos = 0;
        s.maybe_refill(&stream, &mut pos);
        assert_eq!(s.bits, 0x20000);
        assert_eq!(pos, 0);
    }

    #[test]
    fn rans_pop_bits_extracts_low_bits() {
        let mut s = RansState { bits: 0x12345678 };
        let stream = [];
        let mut pos = 0;
        let extracted = s.pop_bits(&stream, &mut pos, 8);
        assert_eq!(extracted, 0x78);
        assert_eq!(s.bits, 0x00123456);
    }

    #[test]
    fn rans_pop_cdf_uniform_two_symbols() {
        let mut cdf = FrequencyTable::new(4, 2, 2);
        cdf.sums = vec![0, 8, 16];
        cdf.finish_update();

        // code=5 is in [0,8) → symbol 0
        let mut s = RansState { bits: 0x20005 };
        let stream = [];
        let mut pos = 0;
        let sym = s.pop_cdf(&stream, &mut pos, &cdf);
        assert_eq!(sym, 0);
        // raw = (0x20005 >> 4) * 8 + 5 - 0 = 0x2000 * 8 + 5 = 0x10005
        assert_eq!(s.bits, 0x10005);

        // code=10 is in [8,16) → symbol 1
        let mut s = RansState { bits: 0x2000A };
        let mut pos = 0;
        let sym = s.pop_cdf(&stream, &mut pos, &cdf);
        assert_eq!(sym, 1);
        // raw = (0x2000A >> 4) * 8 + 10 - 8 = 0x2000 * 8 + 2 = 0x10002
        assert_eq!(s.bits, 0x10002);
    }

    // ---- LruCache ----

    #[test]
    fn lru_initial_state() {
        let c = LruCache::new();
        assert_eq!(c.entries, [1; 8]);
        assert_eq!(c.order, 0x76543210);
    }

    #[test]
    fn lru_hit_0_preserves_order() {
        let mut c = LruCache::new();
        let val = c.hit(0);
        assert_eq!(val, 1);
        assert_eq!(c.order, 0x76543210);
    }

    #[test]
    fn lru_hit_7_promotes_to_front() {
        let mut c = LruCache::new();
        c.hit(7);
        // Slot 7 moves to MRU, others shift:
        // 76543210 → 65432107
        assert_eq!(c.order, 0x65432107);
    }

    #[test]
    fn lru_order_permutation_invariant() {
        let mut c = LruCache::new();
        c.insert(42);
        c.insert(99);
        c.hit(3);
        c.hit(0);
        c.insert(7);
        c.hit(5);

        let mut nibbles: Vec<u32> = (0..8)
            .map(|i| (c.order >> (i * 4)) & 0xF)
            .collect();
        nibbles.sort();
        assert_eq!(nibbles, vec![0, 1, 2, 3, 4, 5, 6, 7],
            "order register must contain exactly one of each nibble 0-7");
    }

    #[test]
    fn lru_insert_evicts_oldest() {
        let mut c = LruCache::new();
        // order = 0x76543210 → idx7=7, idx6=6
        // insert(42): entries[7] = entries[6] (=1), entries[6] = 42
        c.insert(42);
        assert_eq!(c.entries[6], 42);
        assert_eq!(c.entries[7], 1);
    }

    #[test]
    fn lru_hit_returns_correct_entry() {
        let mut c = LruCache::new();
        c.insert(100); // entries[6] = 100
        c.insert(200); // entries[7] = entries[6] = 100, entries[6] = 200

        // Position 6 in order 0x76543210 has slot 6
        let val = c.hit(6);
        assert_eq!(val, 200);
    }

    // ---- decompress() ----

    #[test]
    fn decompress_empty_input() {
        let result = decompress(&[], &mut [0u8; 16]);
        assert!(matches!(result, Err(Error::InputTruncated)));
    }

    #[test]
    fn decompress_single_byte_input() {
        let result = decompress(&[0x42], &mut [0u8; 16]);
        assert!(matches!(result, Err(Error::InputTruncated)));
    }

    #[test]
    fn decompress_bad_magic() {
        let result = decompress(&[0x00, 0x00], &mut [0u8; 16]);
        assert!(matches!(result, Err(Error::InvalidMagic)));
    }

    #[test]
    fn decompress_zero_length_output() {
        let input = BITKNIT_MAGIC.to_le_bytes();
        let result = decompress(&input, &mut []);
        assert!(result.is_ok());
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    InvalidMagic,
    InvalidStream(&'static str),
    OutputTooSmall,
    InputTruncated,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidMagic => write!(f, "BitKnit: invalid magic (expected 0x75B1)"),
            Error::InvalidStream(s) => write!(f, "BitKnit: invalid stream: {s}"),
            Error::OutputTooSmall => write!(f, "BitKnit: output buffer too small"),
            Error::InputTruncated => write!(f, "BitKnit: input truncated"),
        }
    }
}

impl std::error::Error for Error {}

/// Decompress a Granny2 BitKnit stream.
///
/// The compressed data starts with a 2-byte magic (0x75B1), followed by
/// one or more quantums (up to 64KB each) of BitKnit-encoded data.
/// The stream is consumed as u16 words (LE on disk).
pub fn decompress(compressed: &[u8], decompressed: &mut [u8]) -> Result<(), Error> {
    if compressed.len() < 2 {
        return Err(Error::InputTruncated);
    }

    let stream: Vec<u16> = compressed
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();

    let mut pos = 0usize;

    if pos >= stream.len() || stream[pos] != BITKNIT_MAGIC {
        return Err(Error::InvalidMagic);
    }
    pos += 1;

    let mut decoder = Decoder::new();
    let mut offset = 0usize;

    while offset < decompressed.len() {
        if pos >= stream.len() {
            return Err(Error::InputTruncated);
        }
        decoder.decode_quantum(&stream, &mut pos, decompressed, &mut offset)?;
    }

    Ok(())
}
