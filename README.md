# gr2-rs

A Rust parser for RAD Game Tools' [Granny 2](https://www.radgametools.com/granny.html) (`.gr2` / `.model`) 3D asset files.

## Features

- Parses GR2 v6 and v7 files (LE32/LE64)
- BitKnit decompression (types 3 and 4) — the lossless codec used by Granny2
- Relocation resolution (including BitKnit-compressed relocation tables)
- Type system traversal — walks the self-describing struct definitions embedded in GR2 files
- Root node data reading (strings, integers, floats, pointers, arrays, transforms)

## Usage

```
cargo run --release -- path/to/file.model
```

The CLI dumps the file header, section table, type definitions, and root node fields.

### As a library

```rust
use gr2_rs::reader::Gr2File;

let gr2 = Gr2File::load(std::path::Path::new("character.model")).unwrap();

// Inspect sections
for (i, sect) in gr2.sections.iter().enumerate() {
    println!("[{i}] {} -> {} bytes", sect.header.compression, sect.header.uncompressed_size);
}

// Walk the root type definition
if let Some(type_offset) = gr2.resolve_ref(gr2.header.root_type) {
    let members = gr2.walk_struct_def(type_offset).unwrap();
    for m in &members {
        println!("{}: {}", m.name, m.member_type);
    }
}
```

## Supported formats

| Format | Status |
|--------|--------|
| Little-Endian 32-bit (v6/v7) | Parses |
| Little-Endian 64-bit (v7) | Parses |
| Big-Endian 32/64-bit | Magic detected, not implemented |
| BitKnit compression (type 3/4) | Full decompression |
| Oodle compression (type 1/2) | Not implemented |

## Building

```
cargo build --release
```

Requires Rust 2024 edition (1.85+).

## References

- [Knit](https://github.com/neptuwunium/Knit) (C#) — Granny2 BitKnit decompression reference
- [lslib](https://github.com/Norbyte/lslib) — GR2 format structures and type system
- [opengr2](https://github.com/arves100/opengr2) — GR2 header parsing reference

## License

MIT
