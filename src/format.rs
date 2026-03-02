/// GR2 (Granny 2) binary file format structures.
///
/// Reference: lslib by Norbyte, opengr2 by arves100, Knit by neptuwunium.

use std::fmt;

// ---------------------------------------------------------------------------
// Magic signatures (16 bytes each) — determine endianness + pointer width
// ---------------------------------------------------------------------------

pub const MAGIC_LE32_V7: [u8; 16] = [
    0x29, 0xDE, 0x6C, 0xC0, 0xBA, 0xA4, 0x53, 0x2B,
    0x25, 0xF5, 0xB7, 0xA5, 0xF6, 0x66, 0xE2, 0xEE,
];
pub const MAGIC_LE32_V6: [u8; 16] = [
    0xB8, 0x67, 0xB0, 0xCA, 0xF8, 0x6D, 0xB1, 0x0F,
    0x84, 0x72, 0x8C, 0x7E, 0x5E, 0x19, 0x00, 0x1E,
];
pub const MAGIC_BE32: [u8; 16] = [
    0x0E, 0x11, 0x95, 0xB5, 0x6A, 0xA5, 0xB5, 0x4B,
    0xEB, 0x28, 0x28, 0x50, 0x25, 0x78, 0xB3, 0x04,
];
pub const MAGIC_LE64: [u8; 16] = [
    0xE5, 0x9B, 0x49, 0x5E, 0x6F, 0x63, 0x1F, 0x14,
    0x1E, 0x13, 0xEB, 0xA9, 0x90, 0xBE, 0xED, 0xC4,
];
pub const MAGIC_BE64: [u8; 16] = [
    0x31, 0x95, 0xD4, 0xE3, 0x20, 0xDC, 0x4F, 0x62,
    0xCC, 0x36, 0xD0, 0x3A, 0xB1, 0x82, 0xFF, 0x89,
];

// ---------------------------------------------------------------------------
// Format identification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerWidth {
    P32,
    P64,
}

impl PointerWidth {
    pub fn size(self) -> usize {
        match self {
            PointerWidth::P32 => 4,
            PointerWidth::P64 => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatId {
    pub endianness: Endianness,
    pub pointer_width: PointerWidth,
}

pub fn identify_magic(sig: &[u8; 16]) -> Option<FormatId> {
    match sig {
        s if s == &MAGIC_LE32_V7 || s == &MAGIC_LE32_V6 => Some(FormatId {
            endianness: Endianness::Little,
            pointer_width: PointerWidth::P32,
        }),
        s if s == &MAGIC_BE32 => Some(FormatId {
            endianness: Endianness::Big,
            pointer_width: PointerWidth::P32,
        }),
        s if s == &MAGIC_LE64 => Some(FormatId {
            endianness: Endianness::Little,
            pointer_width: PointerWidth::P64,
        }),
        s if s == &MAGIC_BE64 => Some(FormatId {
            endianness: Endianness::Big,
            pointer_width: PointerWidth::P64,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Magic block (0x20 = 32 bytes at file offset 0)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct MagicBlock {
    pub format: FormatId,
    pub headers_size: u32,
    pub header_format: u32, // 0 = uncompressed
}

// ---------------------------------------------------------------------------
// Header (follows Magic at offset 0x20)
// v6: 0x38 bytes, v7: 0x48 bytes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Header {
    pub version: u32,
    pub file_size: u32,
    pub crc: u32,
    pub sections_offset: u32, // offset from header start to section headers
    pub num_sections: u32,
    pub root_type: SectionRef,
    pub root_node: SectionRef,
    pub tag: u32,
    pub extra_tags: [u32; 4],
    pub string_table_crc: u32, // v7 only
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GR2 v{}: {} bytes, {} sections, tag=0x{:08X}",
            self.version, self.file_size, self.num_sections, self.tag
        )
    }
}

// ---------------------------------------------------------------------------
// Section reference (8 bytes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct SectionRef {
    pub section: u32,
    pub offset: u32,
}

impl SectionRef {
    pub const INVALID: Self = SectionRef {
        section: 0xFFFF_FFFF,
        offset: 0,
    };

    pub fn is_valid(self) -> bool {
        self.section != 0xFFFF_FFFF
    }
}

// ---------------------------------------------------------------------------
// Section header (0x2C = 44 bytes each)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SectionHeader {
    pub compression: Compression,
    pub offset_in_file: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub alignment: u32,
    pub first_16bit: u32, // Oodle stop param
    pub first_8bit: u32,  // Oodle stop param
    pub relocations_offset: u32,
    pub num_relocations: u32,
    pub mixed_marshalling_offset: u32,
    pub num_mixed_marshalling: u32,
}

impl SectionHeader {
    pub fn has_data(&self) -> bool {
        self.uncompressed_size > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Oodle0,
    Oodle1,
    BitKnit1,
    BitKnit2,
    Unknown(u32),
}

impl From<u32> for Compression {
    fn from(v: u32) -> Self {
        match v {
            0 => Compression::None,
            1 => Compression::Oodle0,
            2 => Compression::Oodle1,
            3 => Compression::BitKnit1,
            4 => Compression::BitKnit2,
            n => Compression::Unknown(n),
        }
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Compression::None => write!(f, "none"),
            Compression::Oodle0 => write!(f, "oodle0"),
            Compression::Oodle1 => write!(f, "oodle1"),
            Compression::BitKnit1 => write!(f, "bitknit1"),
            Compression::BitKnit2 => write!(f, "bitknit2"),
            Compression::Unknown(n) => write!(f, "unknown({n})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Relocation entry (12 bytes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Relocation {
    pub offset_in_section: u32,
    pub target_section: u32,
    pub target_offset: u32,
}

// ---------------------------------------------------------------------------
// Mixed marshalling entry (16 bytes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct MixedMarshalling {
    pub count: u32,
    pub offset_in_section: u32,
    pub type_ref: SectionRef,
}

// ---------------------------------------------------------------------------
// GR2 type system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemberType {
    None = 0,
    Inline = 1,
    Reference = 2,
    ReferenceToArray = 3,
    ArrayOfReferences = 4,
    VariantReference = 5,
    // 6 unused
    ReferenceToVariantArray = 7,
    String = 8,
    Transform = 9,
    Real32 = 10,
    Int8 = 11,
    UInt8 = 12,
    BinormalInt8 = 13,
    NormalUInt8 = 14,
    Int16 = 15,
    UInt16 = 16,
    BinormalInt16 = 17,
    NormalUInt16 = 18,
    Int32 = 19,
    UInt32 = 20,
    Real16 = 21,
    EmptyReference = 22,
}

impl TryFrom<u32> for MemberType {
    type Error = u32;

    fn try_from(v: u32) -> Result<Self, u32> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Inline),
            2 => Ok(Self::Reference),
            3 => Ok(Self::ReferenceToArray),
            4 => Ok(Self::ArrayOfReferences),
            5 => Ok(Self::VariantReference),
            7 => Ok(Self::ReferenceToVariantArray),
            8 => Ok(Self::String),
            9 => Ok(Self::Transform),
            10 => Ok(Self::Real32),
            11 => Ok(Self::Int8),
            12 => Ok(Self::UInt8),
            13 => Ok(Self::BinormalInt8),
            14 => Ok(Self::NormalUInt8),
            15 => Ok(Self::Int16),
            16 => Ok(Self::UInt16),
            17 => Ok(Self::BinormalInt16),
            18 => Ok(Self::NormalUInt16),
            19 => Ok(Self::Int32),
            20 => Ok(Self::UInt32),
            21 => Ok(Self::Real16),
            22 => Ok(Self::EmptyReference),
            _ => Err(v),
        }
    }
}

impl MemberType {
    /// Size in bytes for this member type given the file's pointer width.
    pub fn size(self, ptr: PointerWidth) -> usize {
        let p = ptr.size(); // 4 or 8
        match self {
            Self::None | Self::Inline => 0,
            Self::Int8 | Self::UInt8 | Self::BinormalInt8 | Self::NormalUInt8 => 1,
            Self::Int16 | Self::UInt16 | Self::BinormalInt16 | Self::NormalUInt16 | Self::Real16 => {
                2
            }
            Self::Real32 | Self::Int32 | Self::UInt32 => 4,
            Self::Reference | Self::String | Self::EmptyReference => p,
            Self::VariantReference => p * 2,           // type_ptr + data_ptr
            Self::ReferenceToArray | Self::ArrayOfReferences => 4 + p, // count(u32) + ptr
            Self::ReferenceToVariantArray => p + 4 + p, // type_ptr + count(u32) + data_ptr
            Self::Transform => 68,                     // flags(4) + vec3(12) + quat(16) + mat3x3(36)
        }
    }
}

impl fmt::Display for MemberType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// Endian-aware binary reading helpers
// ---------------------------------------------------------------------------

pub(crate) fn rd_u16(data: &[u8], off: usize, endian: Endianness) -> u16 {
    let bytes = [data[off], data[off + 1]];
    match endian {
        Endianness::Little => u16::from_le_bytes(bytes),
        Endianness::Big => u16::from_be_bytes(bytes),
    }
}

pub(crate) fn rd_i16(data: &[u8], off: usize, endian: Endianness) -> i16 {
    let bytes = [data[off], data[off + 1]];
    match endian {
        Endianness::Little => i16::from_le_bytes(bytes),
        Endianness::Big => i16::from_be_bytes(bytes),
    }
}

pub(crate) fn rd_u32(data: &[u8], off: usize, endian: Endianness) -> u32 {
    let bytes = [data[off], data[off + 1], data[off + 2], data[off + 3]];
    match endian {
        Endianness::Little => u32::from_le_bytes(bytes),
        Endianness::Big => u32::from_be_bytes(bytes),
    }
}

pub(crate) fn rd_i32(data: &[u8], off: usize, endian: Endianness) -> i32 {
    let bytes = [data[off], data[off + 1], data[off + 2], data[off + 3]];
    match endian {
        Endianness::Little => i32::from_le_bytes(bytes),
        Endianness::Big => i32::from_be_bytes(bytes),
    }
}

pub(crate) fn rd_u64(data: &[u8], off: usize, endian: Endianness) -> u64 {
    let bytes = [
        data[off], data[off + 1], data[off + 2], data[off + 3],
        data[off + 4], data[off + 5], data[off + 6], data[off + 7],
    ];
    match endian {
        Endianness::Little => u64::from_le_bytes(bytes),
        Endianness::Big => u64::from_be_bytes(bytes),
    }
}

pub(crate) fn rd_f32(data: &[u8], off: usize, endian: Endianness) -> f32 {
    let bytes = [data[off], data[off + 1], data[off + 2], data[off + 3]];
    match endian {
        Endianness::Little => f32::from_le_bytes(bytes),
        Endianness::Big => f32::from_be_bytes(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_all_known_magics() {
        let cases = [
            (&MAGIC_LE32_V7, Endianness::Little, PointerWidth::P32),
            (&MAGIC_LE32_V6, Endianness::Little, PointerWidth::P32),
            (&MAGIC_BE32, Endianness::Big, PointerWidth::P32),
            (&MAGIC_LE64, Endianness::Little, PointerWidth::P64),
            (&MAGIC_BE64, Endianness::Big, PointerWidth::P64),
        ];
        for (magic, exp_endian, exp_ptr) in cases {
            let id = identify_magic(magic).expect("known magic should identify");
            assert_eq!(id.endianness, exp_endian);
            assert_eq!(id.pointer_width, exp_ptr);
        }
    }

    #[test]
    fn identify_unknown_magic() {
        assert!(identify_magic(&[0; 16]).is_none());
        assert!(identify_magic(&[0xFF; 16]).is_none());
    }

    #[test]
    fn compression_from_u32() {
        assert_eq!(Compression::from(0), Compression::None);
        assert_eq!(Compression::from(1), Compression::Oodle0);
        assert_eq!(Compression::from(2), Compression::Oodle1);
        assert_eq!(Compression::from(3), Compression::BitKnit1);
        assert_eq!(Compression::from(4), Compression::BitKnit2);
        assert_eq!(Compression::from(99), Compression::Unknown(99));
        assert_eq!(Compression::from(u32::MAX), Compression::Unknown(u32::MAX));
    }

    #[test]
    fn compression_display() {
        assert_eq!(Compression::None.to_string(), "none");
        assert_eq!(Compression::Oodle0.to_string(), "oodle0");
        assert_eq!(Compression::Oodle1.to_string(), "oodle1");
        assert_eq!(Compression::BitKnit1.to_string(), "bitknit1");
        assert_eq!(Compression::BitKnit2.to_string(), "bitknit2");
        assert_eq!(Compression::Unknown(42).to_string(), "unknown(42)");
    }

    #[test]
    fn member_type_try_from_exhaustive() {
        assert_eq!(MemberType::try_from(0), Ok(MemberType::None));
        assert_eq!(MemberType::try_from(1), Ok(MemberType::Inline));
        assert_eq!(MemberType::try_from(2), Ok(MemberType::Reference));
        assert_eq!(MemberType::try_from(3), Ok(MemberType::ReferenceToArray));
        assert_eq!(MemberType::try_from(4), Ok(MemberType::ArrayOfReferences));
        assert_eq!(MemberType::try_from(5), Ok(MemberType::VariantReference));
        // Gap at 6
        assert!(MemberType::try_from(6).is_err());
        assert_eq!(MemberType::try_from(7), Ok(MemberType::ReferenceToVariantArray));
        assert_eq!(MemberType::try_from(8), Ok(MemberType::String));
        assert_eq!(MemberType::try_from(9), Ok(MemberType::Transform));
        assert_eq!(MemberType::try_from(10), Ok(MemberType::Real32));
        assert_eq!(MemberType::try_from(11), Ok(MemberType::Int8));
        assert_eq!(MemberType::try_from(12), Ok(MemberType::UInt8));
        assert_eq!(MemberType::try_from(13), Ok(MemberType::BinormalInt8));
        assert_eq!(MemberType::try_from(14), Ok(MemberType::NormalUInt8));
        assert_eq!(MemberType::try_from(15), Ok(MemberType::Int16));
        assert_eq!(MemberType::try_from(16), Ok(MemberType::UInt16));
        assert_eq!(MemberType::try_from(17), Ok(MemberType::BinormalInt16));
        assert_eq!(MemberType::try_from(18), Ok(MemberType::NormalUInt16));
        assert_eq!(MemberType::try_from(19), Ok(MemberType::Int32));
        assert_eq!(MemberType::try_from(20), Ok(MemberType::UInt32));
        assert_eq!(MemberType::try_from(21), Ok(MemberType::Real16));
        assert_eq!(MemberType::try_from(22), Ok(MemberType::EmptyReference));
        // Out of range
        assert_eq!(MemberType::try_from(23), Err(23));
        assert_eq!(MemberType::try_from(u32::MAX), Err(u32::MAX));
    }

    #[test]
    fn member_type_size_p64() {
        let p = PointerWidth::P64;
        assert_eq!(MemberType::None.size(p), 0);
        assert_eq!(MemberType::Inline.size(p), 0);
        // 1-byte types
        assert_eq!(MemberType::Int8.size(p), 1);
        assert_eq!(MemberType::UInt8.size(p), 1);
        assert_eq!(MemberType::BinormalInt8.size(p), 1);
        assert_eq!(MemberType::NormalUInt8.size(p), 1);
        // 2-byte types
        assert_eq!(MemberType::Int16.size(p), 2);
        assert_eq!(MemberType::UInt16.size(p), 2);
        assert_eq!(MemberType::BinormalInt16.size(p), 2);
        assert_eq!(MemberType::NormalUInt16.size(p), 2);
        assert_eq!(MemberType::Real16.size(p), 2);
        // 4-byte types
        assert_eq!(MemberType::Real32.size(p), 4);
        assert_eq!(MemberType::Int32.size(p), 4);
        assert_eq!(MemberType::UInt32.size(p), 4);
        // 64-bit pointer types
        assert_eq!(MemberType::Reference.size(p), 8);
        assert_eq!(MemberType::String.size(p), 8);
        assert_eq!(MemberType::EmptyReference.size(p), 8);
        // Compound types
        assert_eq!(MemberType::VariantReference.size(p), 16);
        assert_eq!(MemberType::ReferenceToArray.size(p), 12);
        assert_eq!(MemberType::ArrayOfReferences.size(p), 12);
        assert_eq!(MemberType::ReferenceToVariantArray.size(p), 20);
        assert_eq!(MemberType::Transform.size(p), 68);
    }

    #[test]
    fn member_type_size_p32() {
        let p = PointerWidth::P32;
        // Fixed-size types are the same
        assert_eq!(MemberType::None.size(p), 0);
        assert_eq!(MemberType::Inline.size(p), 0);
        assert_eq!(MemberType::Int8.size(p), 1);
        assert_eq!(MemberType::Real32.size(p), 4);
        assert_eq!(MemberType::Transform.size(p), 68);
        // Pointer-dependent types
        assert_eq!(MemberType::Reference.size(p), 4);
        assert_eq!(MemberType::String.size(p), 4);
        assert_eq!(MemberType::EmptyReference.size(p), 4);
        assert_eq!(MemberType::VariantReference.size(p), 8);       // 2 * 4
        assert_eq!(MemberType::ReferenceToArray.size(p), 8);       // 4 + 4
        assert_eq!(MemberType::ArrayOfReferences.size(p), 8);      // 4 + 4
        assert_eq!(MemberType::ReferenceToVariantArray.size(p), 12); // 4 + 4 + 4
    }

    #[test]
    fn section_ref_validity() {
        assert!(!SectionRef::INVALID.is_valid());
        assert!(SectionRef { section: 0, offset: 0 }.is_valid());
        assert!(SectionRef { section: 5, offset: 100 }.is_valid());
    }

    #[test]
    fn pointer_width_size() {
        assert_eq!(PointerWidth::P32.size(), 4);
        assert_eq!(PointerWidth::P64.size(), 8);
    }

    #[test]
    fn section_header_has_data() {
        let make = |size| SectionHeader {
            compression: Compression::None,
            offset_in_file: 0,
            compressed_size: 0,
            uncompressed_size: size,
            alignment: 0,
            first_16bit: 0,
            first_8bit: 0,
            relocations_offset: 0,
            num_relocations: 0,
            mixed_marshalling_offset: 0,
            num_mixed_marshalling: 0,
        };
        assert!(!make(0).has_data());
        assert!(make(1).has_data());
        assert!(make(65536).has_data());
    }
}
