/// GR2 file reader — loads, decompresses, and resolves a Granny2 file.
///
/// Supports all four format variants: LE32, LE64, BE32, BE64.
/// Oodle0 (compression type 1) is not supported — no open-source implementation exists.
///
/// **BE mixed marshalling**: Not implemented. The parser correctly traverses types
/// and resolves pointers in BE files, but raw data field values remain in BE byte
/// order. Callers must handle byte-swapping for data fields manually.

use crate::bitknit;
use crate::element::{Field, Value};
use crate::format::*;
use crate::oodle1;
use std::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    UnknownMagic,
    Truncated,
    UnsupportedVersion(u32),
    UnsupportedCompression(Compression),
    UnsupportedHeaderFormat(u32),
    SectionOutOfBounds { section: usize },
    BitKnit(bitknit::Error),
    Oodle1(oodle1::Error),
    InvalidRelocation(String),
    InvalidType(String),
    CrcMismatch { expected: u32, computed: u32 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::UnknownMagic => write!(f, "unknown GR2 magic signature"),
            Error::Truncated => write!(f, "file truncated"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported GR2 version {v} (need 6 or 7)"),
            Error::UnsupportedCompression(c) => write!(f, "unsupported compression: {c}"),
            Error::UnsupportedHeaderFormat(v) => {
                write!(f, "unsupported header format {v} (only 0 = uncompressed)")
            }
            Error::SectionOutOfBounds { section } => {
                write!(f, "section {section} out of bounds")
            }
            Error::BitKnit(e) => write!(f, "{e}"),
            Error::Oodle1(e) => write!(f, "{e}"),
            Error::InvalidRelocation(s) => write!(f, "invalid relocation: {s}"),
            Error::InvalidType(s) => write!(f, "invalid type: {s}"),
            Error::CrcMismatch { expected, computed } => {
                write!(f, "CRC mismatch: expected 0x{expected:08X}, computed 0x{computed:08X}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::BitKnit(e) => Some(e),
            Error::Oodle1(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<bitknit::Error> for Error {
    fn from(e: bitknit::Error) -> Self {
        Error::BitKnit(e)
    }
}

impl From<oodle1::Error> for Error {
    fn from(e: oodle1::Error) -> Self {
        Error::Oodle1(e)
    }
}

/// Write a pointer value into a byte slice with the given endianness and width.
fn write_ptr(
    flat: &mut [u8],
    off: usize,
    value: u64,
    endian: Endianness,
    ptr_width: PointerWidth,
) {
    match (ptr_width, endian) {
        (PointerWidth::P64, Endianness::Little) => {
            flat[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }
        (PointerWidth::P64, Endianness::Big) => {
            flat[off..off + 8].copy_from_slice(&value.to_be_bytes());
        }
        (PointerWidth::P32, Endianness::Little) => {
            flat[off..off + 4].copy_from_slice(&(value as u32).to_le_bytes());
        }
        (PointerWidth::P32, Endianness::Big) => {
            flat[off..off + 4].copy_from_slice(&(value as u32).to_be_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// CRC32 — standard CRC-32 (polynomial 0xEDB88320, reflected)
// ---------------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

#[cfg(test)]
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    crc ^ 0xFFFF_FFFF
}

/// Header struct size (not including the 0x20 magic block).
const HEADER_SIZE_V6: usize = 0x38;
const HEADER_SIZE_V7: usize = 0x48;
const MAGIC_SIZE: usize = 0x20;

/// Compute the file CRC.
///
/// The CRC covers bytes from `MAGIC_SIZE + header_size` to `file_size` using
/// standard CRC-32 (init=0xFFFFFFFF, final XOR=0xFFFFFFFF, polynomial 0xEDB88320).
/// This covers section headers + section data, excluding the magic block and main header.
fn compute_file_crc(data: &[u8], version: u32, file_size: u32) -> u32 {
    let header_size = if version >= 7 { HEADER_SIZE_V7 } else { HEADER_SIZE_V6 };
    let start = MAGIC_SIZE + header_size;
    let end = (file_size as usize).min(data.len());
    if start >= end {
        return 0xFFFF_FFFFu32 ^ 0xFFFF_FFFFu32; // CRC of empty data
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in &data[start..end] {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Section — decompressed section metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Section {
    pub header: SectionHeader,
    /// Absolute offset of this section's data within the flat buffer.
    pub base_address: usize,
}

// ---------------------------------------------------------------------------
// Gr2File — the loaded, decompressed, relocated file
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Gr2File {
    pub magic: MagicBlock,
    pub header: Header,
    pub sections: Vec<Section>,
    /// Flat buffer containing all decompressed section data concatenated.
    pub flat: Vec<u8>,
    pub endianness: Endianness,
    pub pointer_width: PointerWidth,
}

/// Minimum file size to contain magic block (0x20) + header through extra_tags (0x38).
const MIN_FILE_SIZE: usize = 0x58;

/// Maximum decompressed section size (256 MB). Prevents unbounded allocation from
/// corrupted/malicious `uncompressed_size` values in section headers.
const MAX_SECTION_SIZE: usize = 256 * 1024 * 1024;

/// Maximum element count for array resolution. Prevents unbounded allocation from
/// corrupted `count` fields in ReferenceToArray/ArrayOfReferences values.
const MAX_ARRAY_COUNT: u32 = 16 * 1024 * 1024;

impl Gr2File {
    pub fn load(path: &std::path::Path) -> Result<Self, Error> {
        let file_data = std::fs::read(path)?;
        Self::parse(&file_data)
    }

    /// Validate the file CRC against the stored header CRC.
    ///
    /// Takes raw file bytes (before decompression) since the CRC covers the
    /// on-disk format. This is a separate call from `parse()` so that modded
    /// files with intentionally altered data can still be loaded.
    pub fn validate_crc(data: &[u8]) -> Result<(), Error> {
        if data.len() < MIN_FILE_SIZE {
            return Err(Error::Truncated);
        }
        let mut sig = [0u8; 16];
        sig.copy_from_slice(&data[..16]);
        let format = identify_magic(&sig).ok_or(Error::UnknownMagic)?;
        let endian = format.endianness;
        let version = rd_u32(data, 0x20, endian);
        let file_size = rd_u32(data, 0x24, endian);
        let expected = rd_u32(data, 0x28, endian);
        let computed = compute_file_crc(data, version, file_size);
        if expected != computed {
            return Err(Error::CrcMismatch { expected, computed });
        }
        Ok(())
    }

    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        // --- Upfront bounds check ---
        if data.len() < MIN_FILE_SIZE {
            if data.len() >= 16 {
                let mut sig = [0u8; 16];
                sig.copy_from_slice(&data[..16]);
                if identify_magic(&sig).is_none() {
                    return Err(Error::UnknownMagic);
                }
            }
            return Err(Error::Truncated);
        }

        // --- Magic block (0x00..0x20) ---
        let mut sig = [0u8; 16];
        sig.copy_from_slice(&data[..16]);
        let format = identify_magic(&sig).ok_or(Error::UnknownMagic)?;

        let endian = format.endianness;
        let ptr_width = format.pointer_width;

        let headers_size = rd_u32(data, 0x10, endian);
        let header_format = rd_u32(data, 0x14, endian);

        if header_format != 0 {
            return Err(Error::UnsupportedHeaderFormat(header_format));
        }

        let magic = MagicBlock {
            format,
            headers_size,
            header_format,
        };

        // --- Header (0x20..) ---
        let version = rd_u32(data, 0x20, endian);
        if version != 6 && version != 7 {
            return Err(Error::UnsupportedVersion(version));
        }

        // v7 needs an extra 4 bytes for string_table_crc at 0x58
        if version >= 7 && data.len() < 0x5C {
            return Err(Error::Truncated);
        }

        let header = Header {
            version,
            file_size: rd_u32(data, 0x24, endian),
            crc: rd_u32(data, 0x28, endian),
            sections_offset: rd_u32(data, 0x2C, endian),
            num_sections: rd_u32(data, 0x30, endian),
            root_type: SectionRef {
                section: rd_u32(data, 0x34, endian),
                offset: rd_u32(data, 0x38, endian),
            },
            root_node: SectionRef {
                section: rd_u32(data, 0x3C, endian),
                offset: rd_u32(data, 0x40, endian),
            },
            tag: rd_u32(data, 0x44, endian),
            extra_tags: [
                rd_u32(data, 0x48, endian),
                rd_u32(data, 0x4C, endian),
                rd_u32(data, 0x50, endian),
                rd_u32(data, 0x54, endian),
            ],
            string_table_crc: if version >= 7 {
                rd_u32(data, 0x58, endian)
            } else {
                0
            },
        };

        // --- Section headers ---
        let sect_base = 0x20 + header.sections_offset as usize;
        let num_sections = header.num_sections as usize;

        let sect_end = sect_base
            .checked_add(num_sections.checked_mul(44).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        if sect_end > data.len() {
            return Err(Error::Truncated);
        }

        let mut section_headers = Vec::with_capacity(num_sections);

        for i in 0..num_sections {
            let off = sect_base + i * 44;
            section_headers.push(SectionHeader {
                compression: Compression::from(rd_u32(data, off, endian)),
                offset_in_file: rd_u32(data, off + 4, endian),
                compressed_size: rd_u32(data, off + 8, endian),
                uncompressed_size: rd_u32(data, off + 12, endian),
                alignment: rd_u32(data, off + 16, endian),
                first_16bit: rd_u32(data, off + 20, endian),
                first_8bit: rd_u32(data, off + 24, endian),
                relocations_offset: rd_u32(data, off + 28, endian),
                num_relocations: rd_u32(data, off + 32, endian),
                mixed_marshalling_offset: rd_u32(data, off + 36, endian),
                num_mixed_marshalling: rd_u32(data, off + 40, endian),
            });
        }

        // --- Decompress sections ---
        let mut sections = Vec::with_capacity(num_sections);
        let mut flat = Vec::new();

        for (i, sh) in section_headers.iter().enumerate() {
            let base_address = flat.len();
            if sh.uncompressed_size > 0 {
                let decompressed = Self::decompress_section(data, sh, i, endian)?;
                flat.extend_from_slice(&decompressed);
            }

            sections.push(Section {
                header: sh.clone(),
                base_address,
            });
        }

        // --- Apply relocations ---
        Self::apply_relocations(
            data,
            &section_headers,
            &sections,
            &mut flat,
            endian,
            ptr_width,
        )?;

        Ok(Gr2File {
            magic,
            header,
            sections,
            flat,
            endianness: endian,
            pointer_width: ptr_width,
        })
    }

    fn decompress_section(
        file_data: &[u8],
        sh: &SectionHeader,
        section_idx: usize,
        endian: Endianness,
    ) -> Result<Vec<u8>, Error> {
        let offset = sh.offset_in_file as usize;
        let comp_size = sh.compressed_size as usize;
        let decomp_size = sh.uncompressed_size as usize;

        if decomp_size > MAX_SECTION_SIZE {
            return Err(Error::InvalidType(format!(
                "section {section_idx}: uncompressed size {decomp_size} exceeds maximum \
                 ({MAX_SECTION_SIZE})"
            )));
        }

        match sh.compression {
            Compression::None => {
                let end = offset + decomp_size;
                if end > file_data.len() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("section {section_idx}: data past end of file"),
                    )));
                }
                Ok(file_data[offset..end].to_vec())
            }
            Compression::BitKnit1 | Compression::BitKnit2 => {
                let end = offset + comp_size;
                if end > file_data.len() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("section {section_idx}: compressed data past end of file"),
                    )));
                }
                let compressed = &file_data[offset..end];
                let mut decompressed = vec![0u8; decomp_size];
                bitknit::decompress(compressed, &mut decompressed)?;
                Ok(decompressed)
            }
            Compression::Oodle1 => {
                let end = offset + comp_size;
                if end > file_data.len() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("section {section_idx}: compressed data past end of file"),
                    )));
                }
                let compressed = &file_data[offset..end];
                let mut decompressed = vec![0u8; decomp_size];
                oodle1::decompress(
                    compressed,
                    &mut decompressed,
                    sh.first_16bit,
                    sh.first_8bit,
                    endian,
                )?;
                Ok(decompressed)
            }
            other => Err(Error::UnsupportedCompression(other)),
        }
    }

    fn apply_relocations(
        file_data: &[u8],
        section_headers: &[SectionHeader],
        sections: &[Section],
        flat: &mut [u8],
        endian: Endianness,
        ptr_width: PointerWidth,
    ) -> Result<(), Error> {
        let ptr_size = ptr_width.size();

        for (sect_idx, sh) in section_headers.iter().enumerate() {
            if sh.num_relocations == 0 {
                continue;
            }

            let relocations = Self::read_relocations(file_data, sh, sect_idx, endian)?;
            let sect_base = sections[sect_idx].base_address;

            for reloc in &relocations {
                let target_sect = reloc.target_section as usize;
                if target_sect >= sections.len() {
                    return Err(Error::InvalidRelocation(format!(
                        "section {sect_idx}: relocation targets section {target_sect} \
                         (only {} exist)",
                        sections.len()
                    )));
                }

                let target_addr =
                    (sections[target_sect].base_address + reloc.target_offset as usize) as u64;
                let write_addr = sect_base + reloc.offset_in_section as usize;

                if write_addr + ptr_size > flat.len() {
                    return Err(Error::InvalidRelocation(format!(
                        "section {sect_idx}: relocation write at {write_addr} past flat buffer"
                    )));
                }
                write_ptr(flat, write_addr, target_addr, endian, ptr_width);
            }
        }
        Ok(())
    }

    fn read_relocations(
        file_data: &[u8],
        sh: &SectionHeader,
        section_idx: usize,
        endian: Endianness,
    ) -> Result<Vec<Relocation>, Error> {
        let count = sh.num_relocations as usize;
        if count == 0 {
            return Ok(Vec::new());
        }

        let offset = sh.relocations_offset as usize;

        // For BitKnit2 (compression type 4), relocation data is itself BitKnit-compressed.
        // It starts with a u32 compressed size, then the compressed data.
        if sh.compression == Compression::BitKnit2 {
            if offset + 4 > file_data.len() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("section {section_idx}: relocation header past EOF"),
                )));
            }
            let comp_size = rd_u32(file_data, offset, endian) as usize;
            let comp_end = offset + 4 + comp_size;
            if comp_end > file_data.len() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("section {section_idx}: compressed relocation data past EOF"),
                )));
            }
            let comp_data = &file_data[offset + 4..comp_end];
            let decomp_size = count * 12;
            let mut decomp = vec![0u8; decomp_size];
            bitknit::decompress(comp_data, &mut decomp)?;

            // Decompressed relocation data preserves the file's native byte order —
            // BitKnit is a byte-level codec that doesn't reorder bytes.
            let mut relocs = Vec::with_capacity(count);
            for i in 0..count {
                let off = i * 12;
                relocs.push(Relocation {
                    offset_in_section: rd_u32(&decomp, off, endian),
                    target_section: rd_u32(&decomp, off + 4, endian),
                    target_offset: rd_u32(&decomp, off + 8, endian),
                });
            }
            Ok(relocs)
        } else {
            // Uncompressed relocations — bounds check the entire range
            let reloc_end = offset + count * 12;
            if reloc_end > file_data.len() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("section {section_idx}: uncompressed relocation data past EOF"),
                )));
            }

            let mut relocs = Vec::with_capacity(count);
            for i in 0..count {
                let off = offset + i * 12;
                relocs.push(Relocation {
                    offset_in_section: rd_u32(file_data, off, endian),
                    target_section: rd_u32(file_data, off + 4, endian),
                    target_offset: rd_u32(file_data, off + 8, endian),
                });
            }
            Ok(relocs)
        }
    }

    // --- Public accessors ---

    /// Read a null-terminated UTF-8 string from the flat buffer at the given offset.
    ///
    /// Returns `None` if offset is 0 (null pointer convention), out of bounds,
    /// or the bytes are not valid UTF-8.
    pub fn read_string(&self, offset: usize) -> Option<&str> {
        if offset == 0 || offset >= self.flat.len() {
            return None;
        }
        let start = offset;
        let mut end = start;
        while end < self.flat.len() && self.flat[end] != 0 {
            end += 1;
        }
        std::str::from_utf8(&self.flat[start..end]).ok()
    }

    /// Dereference a SectionRef into a flat buffer offset.
    pub fn resolve_ref(&self, r: SectionRef) -> Option<usize> {
        if !r.is_valid() {
            return None;
        }
        let sect = r.section as usize;
        if sect >= self.sections.len() {
            return None;
        }
        Some(self.sections[sect].base_address + r.offset as usize)
    }

    /// Read a relocated pointer at the given flat offset.
    /// Width depends on file's pointer format (4 bytes for P32, 8 bytes for P64).
    /// Returns the flat buffer offset it points to.
    pub fn read_ptr(&self, offset: usize) -> Option<usize> {
        let p = self.pointer_width.size();
        if offset + p > self.flat.len() {
            return None;
        }
        let addr = match self.pointer_width {
            PointerWidth::P64 => rd_u64(&self.flat, offset, self.endianness) as usize,
            PointerWidth::P32 => rd_u32(&self.flat, offset, self.endianness) as usize,
        };
        if addr == 0 || addr >= self.flat.len() {
            None
        } else {
            Some(addr)
        }
    }

    /// Read a u32 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_u32(&self, offset: usize) -> Option<u32> {
        if offset + 4 > self.flat.len() {
            return None;
        }
        Some(rd_u32(&self.flat, offset, self.endianness))
    }

    /// Read an i32 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_i32(&self, offset: usize) -> Option<i32> {
        if offset + 4 > self.flat.len() {
            return None;
        }
        Some(rd_i32(&self.flat, offset, self.endianness))
    }

    /// Read an f32 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_f32(&self, offset: usize) -> Option<f32> {
        if offset + 4 > self.flat.len() {
            return None;
        }
        Some(rd_f32(&self.flat, offset, self.endianness))
    }

    /// Read a u8 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_u8(&self, offset: usize) -> Option<u8> {
        self.flat.get(offset).copied()
    }

    /// Read an i8 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_i8(&self, offset: usize) -> Option<i8> {
        self.flat.get(offset).map(|&b| b as i8)
    }

    /// Read a u16 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_u16(&self, offset: usize) -> Option<u16> {
        if offset + 2 > self.flat.len() {
            return None;
        }
        Some(rd_u16(&self.flat, offset, self.endianness))
    }

    /// Read an i16 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_i16(&self, offset: usize) -> Option<i16> {
        if offset + 2 > self.flat.len() {
            return None;
        }
        Some(rd_i16(&self.flat, offset, self.endianness))
    }

    /// Read a u64 from the flat buffer. Returns `None` if out of bounds.
    pub fn read_u64(&self, offset: usize) -> Option<u64> {
        if offset + 8 > self.flat.len() {
            return None;
        }
        Some(rd_u64(&self.flat, offset, self.endianness))
    }

    /// Get the decompressed data for a section by index.
    pub fn section_data(&self, index: usize) -> Option<&[u8]> {
        let sect = self.sections.get(index)?;
        let size = sect.header.uncompressed_size as usize;
        if size == 0 {
            return Some(&[]);
        }
        self.flat.get(sect.base_address..sect.base_address + size)
    }

    // --- Type system traversal ---

    /// Size in bytes of a MemberDefinition record for this file's pointer width.
    pub fn member_def_size(&self) -> usize {
        match self.pointer_width {
            // type(4) + name_ptr(4) + children_ptr(4) + array_size(4) + extra(12) + unknown(4)
            PointerWidth::P32 => 32,
            // type(4) + name_ptr(8) + children_ptr(8) + array_size(4) + extra(12) + unknown(8)
            PointerWidth::P64 => 44,
        }
    }

    /// Read a MemberDefinition from the flat buffer at the given offset.
    pub fn read_member_def(&self, offset: usize) -> Result<MemberDef, Error> {
        let mds = self.member_def_size();
        if offset + mds > self.flat.len() {
            return Err(Error::InvalidType(format!(
                "member definition at 0x{offset:x} extends past flat buffer"
            )));
        }

        let endian = self.endianness;
        let p = self.pointer_width.size();

        let type_val = rd_u32(&self.flat, offset, endian);
        let member_type = MemberType::try_from(type_val).map_err(|v| {
            Error::InvalidType(format!("unknown member type {v} at offset 0x{offset:x}"))
        })?;

        // P64: type(4) + name_ptr(8) + children_ptr(8) + array_size(4) + ...
        // P32: type(4) + name_ptr(4) + children_ptr(4) + array_size(4) + ...
        let name_ptr = match self.pointer_width {
            PointerWidth::P64 => rd_u64(&self.flat, offset + 4, endian) as usize,
            PointerWidth::P32 => rd_u32(&self.flat, offset + 4, endian) as usize,
        };
        let children_ptr = match self.pointer_width {
            PointerWidth::P64 => rd_u64(&self.flat, offset + 4 + p, endian) as usize,
            PointerWidth::P32 => rd_u32(&self.flat, offset + 4 + p, endian) as usize,
        };
        let array_size = rd_u32(&self.flat, offset + 4 + 2 * p, endian);

        let name = if name_ptr > 0 && name_ptr < self.flat.len() {
            self.read_string(name_ptr).unwrap_or("<invalid>")
        } else {
            ""
        };

        Ok(MemberDef {
            member_type,
            name: name.to_string(),
            children_ptr,
            array_size,
        })
    }

    // --- Struct extraction ---

    const MAX_EXTRACT_DEPTH: usize = 32;

    /// Extract a struct's data as a list of typed `Field` values.
    ///
    /// `type_offset` points to the struct's type definition in the flat buffer.
    /// `data_offset` points to the struct's instance data in the flat buffer.
    pub fn extract_struct(&self, type_offset: usize, data_offset: usize) -> Result<Vec<Field>, Error> {
        self.extract_struct_inner(type_offset, data_offset, 0)
    }

    /// Extract the root node's data using header.root_type / header.root_node.
    pub fn extract_root(&self) -> Result<Vec<Field>, Error> {
        let type_offset = self
            .resolve_ref(self.header.root_type)
            .ok_or_else(|| Error::InvalidType("root_type ref is invalid".into()))?;
        let data_offset = self
            .resolve_ref(self.header.root_node)
            .ok_or_else(|| Error::InvalidType("root_node ref is invalid".into()))?;
        self.extract_struct(type_offset, data_offset)
    }

    /// Resolve a reference-like `Value` into its target struct fields.
    ///
    /// Works with `Value::Reference`, `Value::VariantReference`, and
    /// `Value::ReferenceToArray` / `Value::ArrayOfReferences` (returns the
    /// first element). For array references, use `resolve_array` instead.
    ///
    /// Returns `Err` if the value is not a resolvable reference type, the
    /// offset is null, or the type definition is missing.
    pub fn resolve_value(
        &self,
        value: &Value,
        type_offset: usize,
    ) -> Result<Vec<Field>, Error> {
        match value {
            Value::Reference { offset: Some(data_off) } => {
                self.extract_struct(type_offset, *data_off)
            }
            Value::Reference { offset: None } => {
                Err(Error::InvalidType("null reference".into()))
            }
            Value::VariantReference {
                type_offset: Some(toff),
                data_offset: Some(doff),
            } => self.extract_struct(*toff, *doff),
            Value::VariantReference { .. } => {
                Err(Error::InvalidType("null variant reference".into()))
            }
            Value::ReferenceToArray { offset: Some(data_off), .. }
            | Value::ArrayOfReferences { offset: Some(data_off), .. } => {
                self.extract_struct(type_offset, *data_off)
            }
            Value::ReferenceToArray { offset: None, .. }
            | Value::ArrayOfReferences { offset: None, .. } => {
                Err(Error::InvalidType("null array reference".into()))
            }
            _ => Err(Error::InvalidType(format!(
                "value is not a resolvable reference: {value:?}"
            ))),
        }
    }

    /// Resolve a `Value::ReferenceToArray` into a `Vec` of struct element lists.
    ///
    /// Each element is extracted as a `Vec<Field>`.
    pub fn resolve_array(
        &self,
        value: &Value,
        type_offset: usize,
    ) -> Result<Vec<Vec<Field>>, Error> {
        let (count, data_off) = match value {
            Value::ReferenceToArray {
                count,
                offset: Some(off),
            }
            | Value::ArrayOfReferences {
                count,
                offset: Some(off),
            } => (*count, *off),
            Value::ReferenceToArray { offset: None, .. }
            | Value::ArrayOfReferences { offset: None, .. } => {
                return Err(Error::InvalidType("null array reference".into()));
            }
            _ => {
                return Err(Error::InvalidType(format!(
                    "value is not a ReferenceToArray or ArrayOfReferences: {value:?}"
                )));
            }
        };
        if count > MAX_ARRAY_COUNT {
            return Err(Error::InvalidType(format!(
                "array count {count} exceeds maximum ({MAX_ARRAY_COUNT})"
            )));
        }
        let elem_size = self.struct_data_size(type_offset)?;
        let mut results = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let fields = self.extract_struct(type_offset, data_off + i * elem_size)?;
            results.push(fields);
        }
        Ok(results)
    }

    /// Compute the byte size of one struct instance given its type definition.
    pub fn struct_data_size(&self, type_offset: usize) -> Result<usize, Error> {
        let members = self.walk_struct_def(type_offset)?;
        self.compute_struct_size(&members, 0)
    }

    fn compute_struct_size(&self, members: &[MemberDef], depth: usize) -> Result<usize, Error> {
        if depth > Self::MAX_EXTRACT_DEPTH {
            return Err(Error::InvalidType(format!(
                "struct size computation depth exceeded {}",
                Self::MAX_EXTRACT_DEPTH
            )));
        }
        let ptr = self.pointer_width;
        let mut total = 0usize;
        for m in members {
            let count = m.array_size.max(1) as usize;
            if m.member_type == MemberType::Inline {
                if m.children_ptr > 0 {
                    let children = self.walk_struct_def(m.children_ptr)?;
                    total += self.compute_struct_size(&children, depth + 1)? * count;
                }
            } else {
                total += m.member_type.size(ptr) * count;
            }
        }
        Ok(total)
    }

    fn extract_struct_inner(
        &self,
        type_offset: usize,
        data_offset: usize,
        depth: usize,
    ) -> Result<Vec<Field>, Error> {
        if depth > Self::MAX_EXTRACT_DEPTH {
            return Err(Error::InvalidType(format!(
                "struct extraction depth exceeded {} (possible cycle at type offset 0x{:x})",
                Self::MAX_EXTRACT_DEPTH, type_offset
            )));
        }

        let members = self.walk_struct_def(type_offset)?;
        let ptr = self.pointer_width;
        let p = ptr.size();
        let mut fields = Vec::with_capacity(members.len());
        let mut off = data_offset;

        for m in &members {
            let count = m.array_size.max(1) as usize;

            // Pre-compute the byte advance for this member (avoids redundant
            // walk_struct_def calls for Inline members).
            let advance = if m.member_type == MemberType::Inline {
                if m.children_ptr > 0 {
                    let children = self.walk_struct_def(m.children_ptr)?;
                    self.compute_struct_size(&children, depth + 1)? * count
                } else {
                    0
                }
            } else {
                m.member_type.size(ptr) * count
            };
            let inline_elem_size = if m.member_type == MemberType::Inline && count > 1 {
                advance / count
            } else {
                0
            };

            let value = match m.member_type {
                MemberType::Int8 | MemberType::BinormalInt8 => {
                    self.extract_scalar_array(off, count, 1, |o| {
                        self.read_i8(o).map(Value::Int8)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::UInt8 | MemberType::NormalUInt8 => {
                    self.extract_scalar_array(off, count, 1, |o| {
                        self.read_u8(o).map(Value::UInt8)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::Int16 | MemberType::BinormalInt16 => {
                    self.extract_scalar_array(off, count, 2, |o| {
                        self.read_i16(o).map(Value::Int16)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::UInt16 | MemberType::NormalUInt16 => {
                    self.extract_scalar_array(off, count, 2, |o| {
                        self.read_u16(o).map(Value::UInt16)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::Real16 => {
                    self.extract_scalar_array(off, count, 2, |o| {
                        self.read_u16(o).map(Value::Real16)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::Int32 => {
                    self.extract_scalar_array(off, count, 4, |o| {
                        self.read_i32(o).map(Value::Int32)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::UInt32 => {
                    self.extract_scalar_array(off, count, 4, |o| {
                        self.read_u32(o).map(Value::UInt32)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::Real32 => {
                    self.extract_scalar_array(off, count, 4, |o| {
                        self.read_f32(o).map(Value::Real32)
                            .ok_or_else(|| Error::Truncated)
                    })?
                }
                MemberType::String => {
                    let str_ptr = self.read_ptr(off);
                    let s = str_ptr.and_then(|o| self.read_string(o)).map(String::from);
                    Value::String(s)
                }
                MemberType::Reference => {
                    Value::Reference { offset: self.read_ptr(off) }
                }
                MemberType::ReferenceToArray => {
                    let arr_count = self.read_u32(off).ok_or(Error::Truncated)?;
                    let arr_ptr = self.read_ptr(off + 4);
                    Value::ReferenceToArray { count: arr_count, offset: arr_ptr }
                }
                MemberType::ArrayOfReferences => {
                    let arr_count = self.read_u32(off).ok_or(Error::Truncated)?;
                    let arr_ptr = self.read_ptr(off + 4);
                    Value::ArrayOfReferences { count: arr_count, offset: arr_ptr }
                }
                MemberType::VariantReference => {
                    let type_ptr = self.read_ptr(off);
                    let data_ptr = self.read_ptr(off + p);
                    Value::VariantReference { type_offset: type_ptr, data_offset: data_ptr }
                }
                MemberType::ReferenceToVariantArray => {
                    let type_ptr = self.read_ptr(off);
                    let va_count = self.read_u32(off + p).ok_or(Error::Truncated)?;
                    let data_ptr = self.read_ptr(off + p + 4);
                    Value::ReferenceToVariantArray {
                        type_offset: type_ptr,
                        count: va_count,
                        data_offset: data_ptr,
                    }
                }
                MemberType::Transform => {
                    let flags = self.read_u32(off).ok_or(Error::Truncated)?;
                    let mut translation = [0.0f32; 3];
                    let mut rotation = [0.0f32; 4];
                    let mut scale_shear = [[0.0f32; 3]; 3];
                    for i in 0..3 {
                        translation[i] = self.read_f32(off + 4 + i * 4).ok_or(Error::Truncated)?;
                    }
                    for i in 0..4 {
                        rotation[i] = self.read_f32(off + 16 + i * 4).ok_or(Error::Truncated)?;
                    }
                    for row in 0..3 {
                        for col in 0..3 {
                            scale_shear[row][col] =
                                self.read_f32(off + 32 + (row * 3 + col) * 4).ok_or(Error::Truncated)?;
                        }
                    }
                    Value::Transform { flags, translation, rotation, scale_shear }
                }
                MemberType::EmptyReference => Value::EmptyReference,
                MemberType::Inline => {
                    if m.children_ptr == 0 {
                        Value::Struct(vec![])
                    } else if count == 1 {
                        let inner = self.extract_struct_inner(m.children_ptr, off, depth + 1)?;
                        Value::Struct(inner)
                    } else {
                        let mut items = Vec::with_capacity(count);
                        for i in 0..count {
                            let inner = self.extract_struct_inner(
                                m.children_ptr,
                                off + i * inline_elem_size,
                                depth + 1,
                            )?;
                            items.push(Value::Struct(inner));
                        }
                        Value::Array(items)
                    }
                }
                MemberType::None => unreachable!("walk_struct_def filters None"),
            };

            fields.push(Field {
                name: m.name.clone(),
                member_type: m.member_type,
                value,
            });

            off += advance;
        }

        Ok(fields)
    }

    fn extract_scalar_array(
        &self,
        off: usize,
        count: usize,
        elem_size: usize,
        read_fn: impl Fn(usize) -> Result<Value, Error>,
    ) -> Result<Value, Error> {
        if count == 1 {
            read_fn(off)
        } else {
            let mut vals = Vec::with_capacity(count);
            for i in 0..count {
                vals.push(read_fn(off + i * elem_size)?);
            }
            Ok(Value::Array(vals))
        }
    }

    /// Walk a struct definition starting at `offset`, yielding all member definitions
    /// until a None terminator is found.
    pub fn walk_struct_def(&self, offset: usize) -> Result<Vec<MemberDef>, Error> {
        let mut members = Vec::new();
        let mut off = offset;
        let member_size = self.member_def_size();

        loop {
            if off + member_size > self.flat.len() {
                break;
            }
            let m = self.read_member_def(off)?;
            if m.member_type == MemberType::None {
                break;
            }
            members.push(m);
            off += member_size;
        }
        Ok(members)
    }
}

#[derive(Debug, Clone)]
pub struct MemberDef {
    pub member_type: MemberType,
    pub name: String,
    pub children_ptr: usize,
    pub array_size: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gr2(flat: Vec<u8>) -> Gr2File {
        make_gr2_full(flat, vec![], Endianness::Little, PointerWidth::P64)
    }

    fn make_gr2_be(flat: Vec<u8>) -> Gr2File {
        make_gr2_full(flat, vec![], Endianness::Big, PointerWidth::P64)
    }

    fn make_gr2_p32(flat: Vec<u8>) -> Gr2File {
        make_gr2_full(flat, vec![], Endianness::Little, PointerWidth::P32)
    }

    fn make_gr2_be_p32(flat: Vec<u8>) -> Gr2File {
        make_gr2_full(flat, vec![], Endianness::Big, PointerWidth::P32)
    }

    fn make_gr2_with_sections(flat: Vec<u8>, sections: Vec<Section>) -> Gr2File {
        make_gr2_full(flat, sections, Endianness::Little, PointerWidth::P64)
    }

    fn make_gr2_full(
        flat: Vec<u8>,
        sections: Vec<Section>,
        endianness: Endianness,
        pointer_width: PointerWidth,
    ) -> Gr2File {
        Gr2File {
            magic: MagicBlock {
                format: FormatId {
                    endianness,
                    pointer_width,
                },
                headers_size: 0,
                header_format: 0,
            },
            header: Header {
                version: 7,
                file_size: 0,
                crc: 0,
                sections_offset: 0,
                num_sections: sections.len() as u32,
                root_type: SectionRef::INVALID,
                root_node: SectionRef::INVALID,
                tag: 0,
                extra_tags: [0; 4],
                string_table_crc: 0,
            },
            sections,
            flat,
            endianness,
            pointer_width,
        }
    }

    fn dummy_section(base_address: usize, size: usize) -> Section {
        Section {
            header: SectionHeader {
                compression: Compression::None,
                offset_in_file: 0,
                compressed_size: 0,
                uncompressed_size: size as u32,
                alignment: 0,
                first_16bit: 0,
                first_8bit: 0,
                relocations_offset: 0,
                num_relocations: 0,
                mixed_marshalling_offset: 0,
                num_mixed_marshalling: 0,
            },
            base_address,
        }
    }

    /// Build a minimal valid GR2 file (version 6, 0 sections) for a given format.
    fn make_minimal_file(magic: &[u8; 16], endian: Endianness) -> Vec<u8> {
        let mut data = vec![0u8; MIN_FILE_SIZE];
        data[..16].copy_from_slice(magic);
        // header_format at 0x14 = 0 (already zero)
        // Version 6 at 0x20
        let v6 = match endian {
            Endianness::Little => 6u32.to_le_bytes(),
            Endianness::Big => 6u32.to_be_bytes(),
        };
        data[0x20..0x24].copy_from_slice(&v6);
        // root_type = INVALID (0xFFFFFFFF) at 0x34
        let invalid = match endian {
            Endianness::Little => 0xFFFF_FFFFu32.to_le_bytes(),
            Endianness::Big => 0xFFFF_FFFFu32.to_be_bytes(),
        };
        data[0x34..0x38].copy_from_slice(&invalid);
        // root_node = INVALID at 0x3C
        data[0x3C..0x40].copy_from_slice(&invalid);
        // sections_offset at 0x2C — point past all header data
        let sect_off = match endian {
            Endianness::Little => 0x38u32.to_le_bytes(), // 0x20 + 0x38 = 0x58 = MIN_FILE_SIZE
            Endianness::Big => 0x38u32.to_be_bytes(),
        };
        data[0x2C..0x30].copy_from_slice(&sect_off);
        // num_sections = 0 at 0x30 (already zero)
        data
    }

    // ---- parse errors ----

    #[test]
    fn parse_unknown_magic() {
        let data = vec![0u8; 16];
        assert!(matches!(Gr2File::parse(&data), Err(Error::UnknownMagic)));
    }

    #[test]
    fn parse_truncated_after_magic() {
        let mut data = vec![0u8; 32];
        data[..16].copy_from_slice(&MAGIC_LE64);
        assert!(matches!(Gr2File::parse(&data), Err(Error::Truncated)));
    }

    // ---- format acceptance ----

    #[test]
    fn parse_accepts_be64() {
        let data = make_minimal_file(&MAGIC_BE64, Endianness::Big);
        let gr2 = Gr2File::parse(&data).expect("BE64 should be accepted");
        assert_eq!(gr2.endianness, Endianness::Big);
        assert_eq!(gr2.pointer_width, PointerWidth::P64);
        assert_eq!(gr2.header.version, 6);
    }

    #[test]
    fn parse_accepts_be32() {
        let data = make_minimal_file(&MAGIC_BE32, Endianness::Big);
        let gr2 = Gr2File::parse(&data).expect("BE32 should be accepted");
        assert_eq!(gr2.endianness, Endianness::Big);
        assert_eq!(gr2.pointer_width, PointerWidth::P32);
    }

    #[test]
    fn parse_accepts_le32() {
        let data = make_minimal_file(&MAGIC_LE32_V7, Endianness::Little);
        let gr2 = Gr2File::parse(&data).expect("LE32 should be accepted");
        assert_eq!(gr2.endianness, Endianness::Little);
        assert_eq!(gr2.pointer_width, PointerWidth::P32);
    }

    #[test]
    fn parse_accepts_le64() {
        let data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        let gr2 = Gr2File::parse(&data).expect("LE64 should be accepted");
        assert_eq!(gr2.endianness, Endianness::Little);
        assert_eq!(gr2.pointer_width, PointerWidth::P64);
    }

    // ---- read_string ----

    #[test]
    fn read_string_null_terminated() {
        let mut flat = vec![0u8; 16];
        flat[1] = b'h';
        flat[2] = b'e';
        flat[3] = b'l';
        flat[4] = b'l';
        flat[5] = b'o';
        flat[6] = 0;
        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_string(1), Some("hello"));
    }

    #[test]
    fn read_string_at_zero_returns_none() {
        let gr2 = make_gr2(vec![0u8; 16]);
        assert_eq!(gr2.read_string(0), None);
    }

    #[test]
    fn read_string_out_of_bounds_returns_none() {
        let gr2 = make_gr2(vec![0u8; 16]);
        assert_eq!(gr2.read_string(16), None);
        assert_eq!(gr2.read_string(100), None);
    }

    #[test]
    fn read_string_invalid_utf8() {
        let mut flat = vec![0u8; 16];
        flat[1] = 0xFF;
        flat[2] = 0xFE;
        flat[3] = 0x00;
        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_string(1), None);
    }

    // ---- read_ptr ----

    #[test]
    fn read_ptr_null() {
        let gr2 = make_gr2(vec![0u8; 16]);
        assert_eq!(gr2.read_ptr(0), None);
    }

    #[test]
    fn read_ptr_out_of_bounds() {
        let mut flat = vec![0u8; 16];
        flat[0..8].copy_from_slice(&100u64.to_le_bytes());
        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_ptr(0), None);
    }

    #[test]
    fn read_ptr_insufficient_data() {
        let gr2 = make_gr2(vec![0u8; 4]);
        assert_eq!(gr2.read_ptr(0), None);
    }

    #[test]
    fn read_ptr_valid() {
        let mut flat = vec![0u8; 32];
        flat[0..8].copy_from_slice(&16u64.to_le_bytes());
        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_ptr(0), Some(16));
    }

    #[test]
    fn read_ptr_p32() {
        let mut flat = vec![0u8; 32];
        flat[0..4].copy_from_slice(&16u32.to_le_bytes());
        let gr2 = make_gr2_p32(flat);
        assert_eq!(gr2.read_ptr(0), Some(16));
    }

    #[test]
    fn read_ptr_p32_insufficient_data() {
        let gr2 = make_gr2_p32(vec![0u8; 2]);
        assert_eq!(gr2.read_ptr(0), None);
    }

    // ---- resolve_ref ----

    #[test]
    fn resolve_ref_invalid() {
        let gr2 = make_gr2(vec![0u8; 16]);
        assert_eq!(gr2.resolve_ref(SectionRef::INVALID), None);
    }

    #[test]
    fn resolve_ref_section_oob() {
        let gr2 = make_gr2(vec![0u8; 16]);
        let r = SectionRef {
            section: 5,
            offset: 0,
        };
        assert_eq!(gr2.resolve_ref(r), None);
    }

    #[test]
    fn resolve_ref_valid() {
        let sections = vec![dummy_section(200, 100)];
        let gr2 = make_gr2_with_sections(vec![0u8; 300], sections);
        let r = SectionRef {
            section: 0,
            offset: 50,
        };
        assert_eq!(gr2.resolve_ref(r), Some(250));
    }

    // ---- flat buffer accessors ----

    #[test]
    fn flat_buffer_accessors() {
        let mut flat = vec![0u8; 32];
        flat[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        flat[4..8].copy_from_slice(&(-42i32).to_le_bytes());
        flat[8..12].copy_from_slice(&1.5f32.to_le_bytes());
        flat[16..24].copy_from_slice(&0x0102030405060708u64.to_le_bytes());

        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_u32(0), Some(0xDEADBEEF));
        assert_eq!(gr2.read_i32(4), Some(-42));
        assert_eq!(gr2.read_f32(8), Some(1.5));
        assert_eq!(gr2.read_u64(16), Some(0x0102030405060708));
    }

    #[test]
    fn flat_buffer_accessors_be() {
        let mut flat = vec![0u8; 32];
        flat[0..4].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        flat[4..8].copy_from_slice(&(-42i32).to_be_bytes());
        flat[8..12].copy_from_slice(&1.5f32.to_be_bytes());
        flat[16..24].copy_from_slice(&0x0102030405060708u64.to_be_bytes());

        let gr2 = make_gr2_be(flat);
        assert_eq!(gr2.read_u32(0), Some(0xDEADBEEF));
        assert_eq!(gr2.read_i32(4), Some(-42));
        assert_eq!(gr2.read_f32(8), Some(1.5));
        assert_eq!(gr2.read_u64(16), Some(0x0102030405060708));
    }

    #[test]
    fn flat_buffer_accessors_oob() {
        let gr2 = make_gr2(vec![0u8; 4]);
        assert_eq!(gr2.read_u32(0), Some(0));
        assert_eq!(gr2.read_u32(1), None);
        assert_eq!(gr2.read_i32(1), None);
        assert_eq!(gr2.read_f32(1), None);
        assert_eq!(gr2.read_u64(0), None);
    }

    // ---- read_u32_be ----

    #[test]
    fn read_u32_be() {
        let mut flat = vec![0u8; 8];
        flat[0..4].copy_from_slice(&0xCAFEBABEu32.to_be_bytes());
        let gr2 = make_gr2_be(flat);
        assert_eq!(gr2.read_u32(0), Some(0xCAFEBABE));
    }

    // ---- section_data ----

    #[test]
    fn section_data_returns_slice() {
        let flat = vec![0xAA; 100];
        let sections = vec![dummy_section(0, 40), dummy_section(40, 60)];
        let gr2 = make_gr2_with_sections(flat, sections);
        assert_eq!(gr2.section_data(0).unwrap().len(), 40);
        assert_eq!(gr2.section_data(1).unwrap().len(), 60);
        assert!(gr2.section_data(2).is_none());
    }

    // ---- member_def_size ----

    #[test]
    fn member_def_size_p64() {
        let gr2 = make_gr2(vec![]);
        assert_eq!(gr2.member_def_size(), 44);
    }

    #[test]
    fn member_def_size_p32() {
        let gr2 = make_gr2_p32(vec![]);
        assert_eq!(gr2.member_def_size(), 32);
    }

    // ---- read_member_def_p32 ----

    #[test]
    fn read_member_def_p32() {
        // P32: type(4) + name_ptr(4) + children_ptr(4) + array_size(4) + extra(12) + unknown(4) = 32
        let mut flat = vec![0u8; 64];
        // Place a string "test" at offset 40
        flat[40] = b't';
        flat[41] = b'e';
        flat[42] = b's';
        flat[43] = b't';
        flat[44] = 0;

        // MemberDef at offset 0: type = Real32 (10)
        flat[0..4].copy_from_slice(&10u32.to_le_bytes());
        // name_ptr = 40 at offset 4
        flat[4..8].copy_from_slice(&40u32.to_le_bytes());
        // children_ptr = 0 at offset 8 (already zero)
        // array_size = 3 at offset 12
        flat[12..16].copy_from_slice(&3u32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        let m = gr2.read_member_def(0).unwrap();
        assert_eq!(m.member_type, MemberType::Real32);
        assert_eq!(m.name, "test");
        assert_eq!(m.children_ptr, 0);
        assert_eq!(m.array_size, 3);
    }

    // ---- error source ----

    #[test]
    fn error_source_chains() {
        use std::error::Error as StdError;

        let io_err = Error::Io(std::io::Error::new(std::io::ErrorKind::Other, "test"));
        assert!(io_err.source().is_some());

        let bk_err = Error::BitKnit(bitknit::Error::InvalidMagic);
        assert!(bk_err.source().is_some());

        let oo_err = Error::Oodle1(oodle1::Error::InputTruncated);
        assert!(oo_err.source().is_some());

        let other_err = Error::UnknownMagic;
        assert!(other_err.source().is_none());
    }

    // ---- write_ptr round-trips ----

    #[test]
    fn write_ptr_le_p64_roundtrip() {
        let mut buf = vec![0u8; 8];
        let val = 0x0102030405060708u64;
        write_ptr(&mut buf, 0, val, Endianness::Little, PointerWidth::P64);
        assert_eq!(rd_u64(&buf, 0, Endianness::Little), val);
    }

    #[test]
    fn write_ptr_be_p64_roundtrip() {
        let mut buf = vec![0u8; 8];
        let val = 0x0102030405060708u64;
        write_ptr(&mut buf, 0, val, Endianness::Big, PointerWidth::P64);
        assert_eq!(rd_u64(&buf, 0, Endianness::Big), val);
    }

    #[test]
    fn write_ptr_le_p32_roundtrip() {
        let mut buf = vec![0u8; 4];
        let val = 0xDEADBEEFu64;
        write_ptr(&mut buf, 0, val, Endianness::Little, PointerWidth::P32);
        assert_eq!(rd_u32(&buf, 0, Endianness::Little), 0xDEADBEEF);
    }

    #[test]
    fn write_ptr_be_p32_roundtrip() {
        let mut buf = vec![0u8; 4];
        let val = 0xCAFEBABEu64;
        write_ptr(&mut buf, 0, val, Endianness::Big, PointerWidth::P32);
        assert_eq!(rd_u32(&buf, 0, Endianness::Big), 0xCAFEBABE);
    }

    #[test]
    fn write_ptr_p32_truncation() {
        let mut buf = vec![0u8; 4];
        let val = 0x1_DEADBEEF_u64; // exceeds u32::MAX
        write_ptr(&mut buf, 0, val, Endianness::Little, PointerWidth::P32);
        assert_eq!(rd_u32(&buf, 0, Endianness::Little), 0xDEADBEEF);
    }

    // ---- read_ptr BE+P32 ----

    #[test]
    fn read_ptr_be_p32() {
        let mut flat = vec![0u8; 32];
        flat[0..4].copy_from_slice(&16u32.to_be_bytes());
        let gr2 = make_gr2_be_p32(flat);
        assert_eq!(gr2.read_ptr(0), Some(16));
    }

    // ---- read_member_def P64 LE and BE P32 ----

    #[test]
    fn read_member_def_p64_le() {
        // P64: type(4) + name_ptr(8) + children_ptr(8) + array_size(4) + extra(12) + unknown(8) = 44
        let mut flat = vec![0u8; 128];
        // Place a string "xyz" at offset 80
        flat[80] = b'x';
        flat[81] = b'y';
        flat[82] = b'z';
        flat[83] = 0;

        // type = UInt32 (20) at offset 0
        flat[0..4].copy_from_slice(&20u32.to_le_bytes());
        // name_ptr (u64) = 80 at offset 4
        flat[4..12].copy_from_slice(&80u64.to_le_bytes());
        // children_ptr (u64) = 0 at offset 12 (already zero)
        // array_size = 7 at offset 20
        flat[20..24].copy_from_slice(&7u32.to_le_bytes());

        let gr2 = make_gr2(flat);
        let m = gr2.read_member_def(0).unwrap();
        assert_eq!(m.member_type, MemberType::UInt32);
        assert_eq!(m.name, "xyz");
        assert_eq!(m.children_ptr, 0);
        assert_eq!(m.array_size, 7);
    }

    #[test]
    fn read_member_def_be_p32() {
        // P32 BE: type(4) + name_ptr(4) + children_ptr(4) + array_size(4) + ... = 32
        let mut flat = vec![0u8; 64];
        // Place a string "ab" at offset 48
        flat[48] = b'a';
        flat[49] = b'b';
        flat[50] = 0;

        // type = Int16 (15) at offset 0, BE
        flat[0..4].copy_from_slice(&15u32.to_be_bytes());
        // name_ptr = 48 at offset 4, BE
        flat[4..8].copy_from_slice(&48u32.to_be_bytes());
        // children_ptr = 0 at offset 8 (already zero)
        // array_size = 2 at offset 12, BE
        flat[12..16].copy_from_slice(&2u32.to_be_bytes());

        let gr2 = make_gr2_be_p32(flat);
        let m = gr2.read_member_def(0).unwrap();
        assert_eq!(m.member_type, MemberType::Int16);
        assert_eq!(m.name, "ab");
        assert_eq!(m.children_ptr, 0);
        assert_eq!(m.array_size, 2);
    }

    // ---- walk_struct_def ----

    #[test]
    fn walk_struct_def_two_members() {
        // P32: each MemberDef = 32 bytes, need 2 real + 1 None terminator = 96 bytes + string space
        let mut flat = vec![0u8; 128];
        // String "a" at offset 100
        flat[100] = b'a';
        flat[101] = 0;
        // String "b" at offset 104
        flat[104] = b'b';
        flat[105] = 0;

        // Member 0 at offset 0: type=Real32(10), name_ptr=100, array_size=1
        flat[0..4].copy_from_slice(&10u32.to_le_bytes());
        flat[4..8].copy_from_slice(&100u32.to_le_bytes());
        flat[12..16].copy_from_slice(&1u32.to_le_bytes());

        // Member 1 at offset 32: type=UInt8(12), name_ptr=104, array_size=4
        flat[32..36].copy_from_slice(&12u32.to_le_bytes());
        flat[36..40].copy_from_slice(&104u32.to_le_bytes());
        flat[44..48].copy_from_slice(&4u32.to_le_bytes());

        // Member 2 at offset 64: type=None(0) — terminator (already zero)

        let gr2 = make_gr2_p32(flat);
        let members = gr2.walk_struct_def(0).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].member_type, MemberType::Real32);
        assert_eq!(members[0].name, "a");
        assert_eq!(members[0].array_size, 1);
        assert_eq!(members[1].member_type, MemberType::UInt8);
        assert_eq!(members[1].name, "b");
        assert_eq!(members[1].array_size, 4);
    }

    #[test]
    fn walk_struct_def_truncated() {
        // Buffer too small for a single member def (P32=32 bytes, give 16)
        let flat = vec![0u8; 16];
        let gr2 = make_gr2_p32(flat);
        let members = gr2.walk_struct_def(0).unwrap();
        assert!(members.is_empty());
    }

    // ---- rd_u32 / rd_u64 direct ----

    #[test]
    fn rd_u32_le_vs_be_direct() {
        let bytes = [0x01, 0x02, 0x03, 0x04];
        let le = rd_u32(&bytes, 0, Endianness::Little);
        let be = rd_u32(&bytes, 0, Endianness::Big);
        assert_eq!(le, 0x04030201);
        assert_eq!(be, 0x01020304);
        assert_ne!(le, be);
    }

    #[test]
    fn rd_u64_le_vs_be_direct() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let le = rd_u64(&bytes, 0, Endianness::Little);
        let be = rd_u64(&bytes, 0, Endianness::Big);
        assert_eq!(le, 0x0807060504030201);
        assert_eq!(be, 0x0102030405060708);
        assert_ne!(le, be);
    }

    // ---- parse error paths ----

    #[test]
    fn parse_unsupported_header_format() {
        let mut data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        // Set header_format at 0x14 to 1
        data[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            Gr2File::parse(&data),
            Err(Error::UnsupportedHeaderFormat(1))
        ));
    }

    #[test]
    fn parse_unsupported_version() {
        let mut data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        // Set version at 0x20 to 5
        data[0x20..0x24].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(
            Gr2File::parse(&data),
            Err(Error::UnsupportedVersion(5))
        ));
    }

    #[test]
    fn parse_truncated_section_headers() {
        let mut data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        // Set num_sections=1 at 0x30, but don't add any room for section headers
        data[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(Gr2File::parse(&data), Err(Error::Truncated)));
    }

    // ---- read_u8 / read_i8 ----

    #[test]
    fn read_u8_i8_basic() {
        let flat = vec![0x00, 0x7F, 0x80, 0xFF];
        let gr2 = make_gr2(flat);
        assert_eq!(gr2.read_u8(0), Some(0));
        assert_eq!(gr2.read_u8(1), Some(0x7F));
        assert_eq!(gr2.read_u8(2), Some(0x80));
        assert_eq!(gr2.read_u8(3), Some(0xFF));
        assert_eq!(gr2.read_i8(0), Some(0));
        assert_eq!(gr2.read_i8(1), Some(127));
        assert_eq!(gr2.read_i8(2), Some(-128));
        assert_eq!(gr2.read_i8(3), Some(-1));
    }

    #[test]
    fn read_u8_oob() {
        let gr2 = make_gr2(vec![0x42]);
        assert_eq!(gr2.read_u8(0), Some(0x42));
        assert_eq!(gr2.read_u8(1), None);
        assert_eq!(gr2.read_i8(1), None);
    }

    // ---- read_u16 / read_i16 ----

    #[test]
    fn read_u16_i16_le_be() {
        let mut flat = vec![0u8; 8];
        flat[0..2].copy_from_slice(&0x1234u16.to_le_bytes());
        let gr2_le = make_gr2(flat.clone());
        assert_eq!(gr2_le.read_u16(0), Some(0x1234));

        flat[0..2].copy_from_slice(&0x1234u16.to_be_bytes());
        let gr2_be = make_gr2_be(flat.clone());
        assert_eq!(gr2_be.read_u16(0), Some(0x1234));

        // Signed
        flat[4..6].copy_from_slice(&(-1000i16).to_le_bytes());
        let gr2_le = make_gr2(flat.clone());
        assert_eq!(gr2_le.read_i16(4), Some(-1000));

        flat[4..6].copy_from_slice(&(-1000i16).to_be_bytes());
        let gr2_be = make_gr2_be(flat);
        assert_eq!(gr2_be.read_i16(4), Some(-1000));
    }

    #[test]
    fn read_u16_i16_oob() {
        let gr2 = make_gr2(vec![0x42]);
        assert_eq!(gr2.read_u16(0), None);
        assert_eq!(gr2.read_i16(0), None);
    }

    // ---- struct extraction ----

    /// Build a P32 LE flat buffer with type defs and data for extraction tests.
    /// Returns (gr2, type_offset, data_offset).
    fn make_extraction_gr2(
        type_defs: &[(MemberType, &str, u32)], // (type, name, array_size)
        data: &[u8],
    ) -> (Gr2File, usize, usize) {
        // Layout: [type_defs at 0] [strings] [data]
        // P32 member def = 32 bytes each, plus None terminator
        let num_members = type_defs.len();
        let type_region = (num_members + 1) * 32; // +1 for None terminator
        let string_region_start = type_region;
        let mut strings: Vec<(usize, &str)> = Vec::new();

        // Collect string offsets
        let mut str_off = string_region_start;
        for (_, name, _) in type_defs {
            strings.push((str_off, name));
            str_off += name.len() + 1; // null terminated
        }
        let data_offset = str_off;
        let total = data_offset + data.len();

        let mut flat = vec![0u8; total];

        // Write strings
        for (off, name) in &strings {
            for (i, b) in name.bytes().enumerate() {
                flat[*off + i] = b;
            }
            flat[*off + name.len()] = 0;
        }

        // Write member defs
        for (i, (mtype, _, array_size)) in type_defs.iter().enumerate() {
            let off = i * 32;
            flat[off..off + 4].copy_from_slice(&(*mtype as u32).to_le_bytes());
            flat[off + 4..off + 8].copy_from_slice(&(strings[i].0 as u32).to_le_bytes());
            // children_ptr at off+8 = 0
            flat[off + 12..off + 16].copy_from_slice(&array_size.to_le_bytes());
        }
        // None terminator already zero

        // Write data
        flat[data_offset..data_offset + data.len()].copy_from_slice(data);

        let gr2 = make_gr2_p32(flat);
        (gr2, 0, data_offset)
    }

    #[test]
    fn extract_struct_scalars() {
        // Int32 + UInt32 + Real32
        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&(-42i32).to_le_bytes());
        data[4..8].copy_from_slice(&100u32.to_le_bytes());
        data[8..12].copy_from_slice(&1.5f32.to_le_bytes());

        let (gr2, toff, doff) = make_extraction_gr2(
            &[
                (MemberType::Int32, "a", 0),
                (MemberType::UInt32, "b", 0),
                (MemberType::Real32, "c", 0),
            ],
            &data,
        );
        let fields = gr2.extract_struct(toff, doff).unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].value, Value::Int32(-42));
        assert_eq!(fields[1].value, Value::UInt32(100));
        assert_eq!(fields[2].value, Value::Real32(1.5));
    }

    #[test]
    fn extract_struct_string() {
        // String member pointing to a string elsewhere in flat buffer
        // We need the pointer in the data region to point to a valid string
        // For P32, String is a 4-byte pointer
        let (mut gr2, toff, doff) = make_extraction_gr2(
            &[(MemberType::String, "name", 0)],
            &[0; 4], // placeholder for pointer
        );
        // Place a string "hello" somewhere and point the data to it
        let str_off = gr2.flat.len();
        gr2.flat.extend_from_slice(b"hello\0");
        // Write the pointer in the data region
        let ptr_bytes = (str_off as u32).to_le_bytes();
        gr2.flat[doff..doff + 4].copy_from_slice(&ptr_bytes);

        let fields = gr2.extract_struct(toff, doff).unwrap();
        assert_eq!(fields[0].value, Value::String(Some("hello".into())));
    }

    #[test]
    fn extract_struct_string_null() {
        let (gr2, toff, doff) = make_extraction_gr2(
            &[(MemberType::String, "name", 0)],
            &[0; 4], // null pointer
        );
        let fields = gr2.extract_struct(toff, doff).unwrap();
        assert_eq!(fields[0].value, Value::String(None));
    }

    #[test]
    fn extract_struct_inline() {
        // Build manually: outer struct has one Inline member whose children_ptr
        // points to an inner type def (Int32 "x", Int32 "y")
        let mut flat = vec![0u8; 512];

        // Inner type def at offset 200: two Int32 members + None
        let inner_type_off = 200;
        // String "x" at 300, "y" at 302
        flat[300] = b'x'; flat[301] = 0;
        flat[302] = b'y'; flat[303] = 0;

        // Inner member 0: Int32, name_ptr=300, array_size=1
        flat[inner_type_off..inner_type_off + 4].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[inner_type_off + 4..inner_type_off + 8].copy_from_slice(&300u32.to_le_bytes());
        flat[inner_type_off + 12..inner_type_off + 16].copy_from_slice(&1u32.to_le_bytes());

        // Inner member 1: Int32, name_ptr=302, array_size=1
        flat[inner_type_off + 32..inner_type_off + 36].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[inner_type_off + 36..inner_type_off + 40].copy_from_slice(&302u32.to_le_bytes());
        flat[inner_type_off + 44..inner_type_off + 48].copy_from_slice(&1u32.to_le_bytes());
        // None terminator at inner_type_off + 64 (already zero)

        // Outer type def at offset 0: Inline, children_ptr = inner_type_off
        // String "pos" at 310
        flat[310] = b'p'; flat[311] = b'o'; flat[312] = b's'; flat[313] = 0;
        flat[0..4].copy_from_slice(&(MemberType::Inline as u32).to_le_bytes());
        flat[4..8].copy_from_slice(&310u32.to_le_bytes());
        flat[8..12].copy_from_slice(&(inner_type_off as u32).to_le_bytes());
        flat[12..16].copy_from_slice(&1u32.to_le_bytes());
        // None terminator at offset 32 (already zero)

        // Data at offset 400: two i32 values
        let data_off = 400;
        flat[data_off..data_off + 4].copy_from_slice(&10i32.to_le_bytes());
        flat[data_off + 4..data_off + 8].copy_from_slice(&20i32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        let fields = gr2.extract_struct(0, data_off).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "pos");
        if let Value::Struct(inner) = &fields[0].value {
            assert_eq!(inner.len(), 2);
            assert_eq!(inner[0].name, "x");
            assert_eq!(inner[0].value, Value::Int32(10));
            assert_eq!(inner[1].name, "y");
            assert_eq!(inner[1].value, Value::Int32(20));
        } else {
            panic!("expected Struct, got {:?}", fields[0].value);
        }
    }

    #[test]
    fn extract_struct_array_size() {
        // Real32 with array_size=3
        let mut data = vec![0u8; 12];
        data[0..4].copy_from_slice(&1.0f32.to_le_bytes());
        data[4..8].copy_from_slice(&2.0f32.to_le_bytes());
        data[8..12].copy_from_slice(&3.0f32.to_le_bytes());

        let (gr2, toff, doff) = make_extraction_gr2(
            &[(MemberType::Real32, "vec", 3)],
            &data,
        );
        let fields = gr2.extract_struct(toff, doff).unwrap();
        assert_eq!(fields[0].name, "vec");
        if let Value::Array(vals) = &fields[0].value {
            assert_eq!(vals.len(), 3);
            assert_eq!(vals[0], Value::Real32(1.0));
            assert_eq!(vals[1], Value::Real32(2.0));
            assert_eq!(vals[2], Value::Real32(3.0));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn extract_struct_reference_lazy() {
        // Reference member (P32: 4 bytes pointer)
        let mut data = vec![0u8; 4];
        // Null pointer
        data[0..4].copy_from_slice(&0u32.to_le_bytes());

        let (gr2, toff, doff) = make_extraction_gr2(
            &[(MemberType::Reference, "ref", 0)],
            &data,
        );
        let fields = gr2.extract_struct(toff, doff).unwrap();
        assert_eq!(fields[0].value, Value::Reference { offset: None });
    }

    #[test]
    fn extract_struct_transform() {
        // Transform: flags(4) + translation(12) + rotation(16) + scale_shear(36) = 68
        let mut data = vec![0u8; 68];
        data[0..4].copy_from_slice(&7u32.to_le_bytes()); // flags
        // translation = (1, 2, 3)
        data[4..8].copy_from_slice(&1.0f32.to_le_bytes());
        data[8..12].copy_from_slice(&2.0f32.to_le_bytes());
        data[12..16].copy_from_slice(&3.0f32.to_le_bytes());
        // rotation = (0, 0, 0, 1) — identity quat
        data[28..32].copy_from_slice(&1.0f32.to_le_bytes());
        // scale_shear = identity (diagonal at [0][0]=32, [1][1]=48, [2][2]=64)
        data[32..36].copy_from_slice(&1.0f32.to_le_bytes());
        data[48..52].copy_from_slice(&1.0f32.to_le_bytes());
        data[64..68].copy_from_slice(&1.0f32.to_le_bytes());

        let (gr2, toff, doff) = make_extraction_gr2(
            &[(MemberType::Transform, "xform", 0)],
            &data,
        );
        let fields = gr2.extract_struct(toff, doff).unwrap();
        if let Value::Transform { flags, translation, rotation, scale_shear } = &fields[0].value {
            assert_eq!(*flags, 7);
            assert_eq!(*translation, [1.0, 2.0, 3.0]);
            assert_eq!(*rotation, [0.0, 0.0, 0.0, 1.0]);
            assert_eq!(*scale_shear, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        } else {
            panic!("expected Transform");
        }
    }

    #[test]
    fn extract_struct_depth_limit() {
        // Create a self-referential Inline that would recurse infinitely
        let mut flat = vec![0u8; 128];
        // String "s" at 100
        flat[100] = b's'; flat[101] = 0;

        // Type def at 0: Inline pointing to itself (children_ptr = 0)
        flat[0..4].copy_from_slice(&(MemberType::Inline as u32).to_le_bytes());
        flat[4..8].copy_from_slice(&100u32.to_le_bytes()); // name
        flat[8..12].copy_from_slice(&0u32.to_le_bytes()); // children_ptr = 0 (self)
        flat[12..16].copy_from_slice(&1u32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        // children_ptr=0 means it points back to offset 0 (itself)
        // But with children_ptr == 0, our code treats it as "no children" and returns empty struct
        // Instead, let's make children_ptr point to a valid self-referential type
        // Actually the simpler test: just verify extract_struct_inner rejects depth > 32
        let result = gr2.extract_struct_inner(0, 64, Gr2File::MAX_EXTRACT_DEPTH + 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("depth exceeded"));
    }

    #[test]
    fn extract_root_invalid_refs() {
        let gr2 = make_gr2(vec![0u8; 16]);
        let result = gr2.extract_root();
        assert!(result.is_err());
    }

    // ---- CRC32 ----

    #[test]
    fn crc32_known_vectors() {
        // Standard CRC-32 test vectors
        assert_eq!(crc32(b""), 0x00000000);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b"a"), 0xE8B7BE43);
    }

    #[test]
    fn crc32_table_sanity() {
        assert_eq!(CRC32_TABLE[0], 0);
        // Entry 1 = polynomial 0xEDB88320 after 8 iterations starting from 1
        assert_eq!(CRC32_TABLE[1], 0x77073096);
        assert_ne!(CRC32_TABLE[255], 0);
        // All 256 entries should be distinct
        let mut seen = std::collections::HashSet::new();
        for &v in &CRC32_TABLE {
            seen.insert(v);
        }
        assert_eq!(seen.len(), 256);
    }

    #[test]
    fn validate_crc_truncated() {
        let data = vec![0u8; 16]; // Less than MIN_FILE_SIZE
        assert!(matches!(Gr2File::validate_crc(&data), Err(Error::Truncated)));
    }

    #[test]
    fn validate_crc_matching_roundtrip() {
        // Build a minimal v6 file with some section data to CRC
        let mut data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        // Add some section data after the headers
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE]);
        let file_size = data.len() as u32;
        // Set file_size at 0x24
        data[0x24..0x28].copy_from_slice(&file_size.to_le_bytes());
        // Compute CRC and store it
        let version = 6u32;
        let computed = compute_file_crc(&data, version, file_size);
        data[0x28..0x2C].copy_from_slice(&computed.to_le_bytes());
        assert!(Gr2File::validate_crc(&data).is_ok());
    }

    #[test]
    fn validate_crc_mismatch() {
        let mut data = make_minimal_file(&MAGIC_LE64, Endianness::Little);
        // Add some section data and set file_size correctly
        data.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        let file_size = data.len() as u32;
        data[0x24..0x28].copy_from_slice(&file_size.to_le_bytes());
        // Store an obviously wrong CRC
        data[0x28..0x2C].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        let result = Gr2File::validate_crc(&data);
        assert!(matches!(result, Err(Error::CrcMismatch { expected: 0xDEADBEEF, .. })));
    }

    // ---- resolve_value / resolve_array ----

    #[test]
    fn resolve_value_reference() {
        // Build: a type def for a struct with one Int32 "val",
        // and data for that struct, then a Reference pointing to it.
        let mut flat = vec![0u8; 256];

        // String "val" at 200
        flat[200] = b'v'; flat[201] = b'a'; flat[202] = b'l'; flat[203] = 0;

        // Type def at offset 100: Int32, name_ptr=200, array_size=1
        flat[100..104].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[104..108].copy_from_slice(&200u32.to_le_bytes());
        flat[112..116].copy_from_slice(&1u32.to_le_bytes());
        // None terminator at 132 (already zero)

        // Data at offset 160: i32 = 42
        flat[160..164].copy_from_slice(&42i32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        let ref_val = Value::Reference { offset: Some(160) };
        let fields = gr2.resolve_value(&ref_val, 100).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].value, Value::Int32(42));
    }

    #[test]
    fn resolve_value_null_reference() {
        let gr2 = make_gr2(vec![0u8; 16]);
        let ref_val = Value::Reference { offset: None };
        assert!(gr2.resolve_value(&ref_val, 0).is_err());
    }

    #[test]
    fn resolve_value_not_reference() {
        let gr2 = make_gr2(vec![0u8; 16]);
        let val = Value::Int32(5);
        assert!(gr2.resolve_value(&val, 0).is_err());
    }

    #[test]
    fn resolve_array_basic() {
        // Type def for Int32 "n", data for 3 elements
        let mut flat = vec![0u8; 256];
        flat[200] = b'n'; flat[201] = 0;

        flat[100..104].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[104..108].copy_from_slice(&200u32.to_le_bytes());
        flat[112..116].copy_from_slice(&1u32.to_le_bytes());
        // None terminator at 132

        // 3 x i32 at offset 160
        flat[160..164].copy_from_slice(&10i32.to_le_bytes());
        flat[164..168].copy_from_slice(&20i32.to_le_bytes());
        flat[168..172].copy_from_slice(&30i32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        let arr_val = Value::ReferenceToArray { count: 3, offset: Some(160) };
        let results = gr2.resolve_array(&arr_val, 100).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0][0].value, Value::Int32(10));
        assert_eq!(results[1][0].value, Value::Int32(20));
        assert_eq!(results[2][0].value, Value::Int32(30));
    }

    #[test]
    fn compute_struct_size_circular_inline() {
        // Inline member whose children_ptr points back to the same type def
        let mut flat = vec![0u8; 128];
        flat[100] = b's'; flat[101] = 0;

        // Type def at offset 32: Inline, children_ptr = 32 (self-referential)
        flat[32..36].copy_from_slice(&(MemberType::Inline as u32).to_le_bytes());
        flat[36..40].copy_from_slice(&100u32.to_le_bytes()); // name
        flat[40..44].copy_from_slice(&32u32.to_le_bytes());  // children_ptr = self
        flat[44..48].copy_from_slice(&1u32.to_le_bytes());   // array_size
        // None terminator at 64 (already zero)

        let gr2 = make_gr2_p32(flat);
        // Should return an error, not stack overflow
        let result = gr2.struct_data_size(32);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depth exceeded"));
    }

    #[test]
    fn struct_data_size_with_inline() {
        // Outer: Inline(inner) where inner has Int32 + Int32 = 8 bytes
        let mut flat = vec![0u8; 512];

        let inner_type_off = 200;
        flat[300] = b'a'; flat[301] = 0;
        flat[302] = b'b'; flat[303] = 0;

        flat[inner_type_off..inner_type_off + 4].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[inner_type_off + 4..inner_type_off + 8].copy_from_slice(&300u32.to_le_bytes());
        flat[inner_type_off + 12..inner_type_off + 16].copy_from_slice(&1u32.to_le_bytes());
        flat[inner_type_off + 32..inner_type_off + 36].copy_from_slice(&(MemberType::Int32 as u32).to_le_bytes());
        flat[inner_type_off + 36..inner_type_off + 40].copy_from_slice(&302u32.to_le_bytes());
        flat[inner_type_off + 44..inner_type_off + 48].copy_from_slice(&1u32.to_le_bytes());

        // Outer type at 0: one Inline member
        flat[310] = b'c'; flat[311] = 0;
        flat[0..4].copy_from_slice(&(MemberType::Inline as u32).to_le_bytes());
        flat[4..8].copy_from_slice(&310u32.to_le_bytes());
        flat[8..12].copy_from_slice(&(inner_type_off as u32).to_le_bytes());
        flat[12..16].copy_from_slice(&1u32.to_le_bytes());

        let gr2 = make_gr2_p32(flat);
        assert_eq!(gr2.struct_data_size(0).unwrap(), 8);
    }
}
