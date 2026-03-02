# gr2-rs

[![CI](https://github.com/kiroushi/gr2-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/kiroushi/gr2-rs/actions/workflows/ci.yml)

A zero-dependency Rust parser for RAD Game Tools' [Granny 2](https://www.radgametools.com/granny.html) (`.gr2` / `.model`) 3D asset files.

## Features

- Parses GR2 v6 and v7 files in all four format variants (LE32, LE64, BE32, BE64)
- BitKnit decompression (types 3 and 4) — dual-state interleaved rANS + LZ codec
- Oodle1 decompression (type 2) — arithmetic + WeighWindow + LZSS codec
- Relocation resolution (including BitKnit-compressed relocation tables)
- Type system traversal — walks the self-describing struct definitions embedded in GR2 files
- Typed data extraction — recursively extracts struct instances into a `Field`/`Value` element tree
- Lazy reference resolution — follow `Reference`, `ReferenceToArray`, `ArrayOfReferences`, and `VariantReference` values
- CRC-32 file validation (separate from parsing, so modded files still load)

## Usage

```
cargo run --release -- path/to/file.model
cargo run --release -- --types path/to/file.model   # also dump type definitions
```

The CLI dumps the file header, section table, and root node fields. With `--types`, it also prints the full type definition tree.

### As a library

```rust
use gr2_rs::reader::Gr2File;
use gr2_rs::element::Value;

let gr2 = Gr2File::load(std::path::Path::new("character.model"))?;

// Extract the root node as typed fields
let fields = gr2.extract_root()?;
for f in &fields {
    println!("{}: {:?}", f.name, f.value);
}

// Resolve a ReferenceToArray into its elements
for f in &fields {
    if let Value::ReferenceToArray { count, offset: Some(_) } = &f.value {
        if let Some(type_off) = gr2.resolve_ref(gr2.header.root_type) {
            let elements = gr2.resolve_array(&f.value, type_off)?;
            println!("{}: {} elements", f.name, elements.len());
        }
    }
}

// Validate file CRC (on raw bytes, before parsing)
let raw = std::fs::read("character.model")?;
Gr2File::validate_crc(&raw)?;
```

## Supported formats

| Format | Parsing | Data extraction |
|--------|---------|-----------------|
| Little-Endian 32-bit (v6/v7) | Full | Full |
| Little-Endian 64-bit (v6/v7) | Full | Full |
| Big-Endian 32-bit (v6/v7) | Full | Full (no mixed marshalling byte-swap) |
| Big-Endian 64-bit (v6/v7) | Full | Full (no mixed marshalling byte-swap) |

| Compression | Status |
|-------------|--------|
| None (0) | Supported |
| Oodle0 (1) | Not supported (no open-source implementation exists) |
| Oodle1 (2) | Full decompression |
| BitKnit1 (3) | Full decompression |
| BitKnit2 (4) | Full decompression (including compressed relocation tables) |

## Building

```
cargo build --release
cargo test
```

Zero dependencies. Requires Rust 2024 edition (1.85+).

## References

- [Knit](https://github.com/neptuwunium/Knit) (C#) — Granny2 BitKnit decompression reference
- [lslib](https://github.com/Norbyte/lslib) — GR2 format structures, type system, and CRC algorithm
- [opengr2](https://github.com/arves100/opengr2) — GR2 header parsing and Oodle1 decompression reference

## License

MIT
