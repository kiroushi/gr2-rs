Rust parser and decompressor for RAD Game Tools' Granny 2 (GR2) 3D asset format.

## Build and test

```
cargo build --release
cargo test
```

Zero warnings policy — release build must be clean.

## Architecture

| File | Purpose |
|------|---------|
| `src/format.rs` | GR2 binary format structures: magic signatures, headers, section headers, type system enums |
| `src/reader.rs` | File loader: decompression, relocation, type traversal, flat buffer accessors |
| `src/bitknit.rs` | BitKnit decompressor: dual-state interleaved rANS + LZ codec (Granny2 variant) |
| `src/main.rs` | CLI tool that dumps GR2 file contents |

## Key technical details

- BitKnit CDF adaptation uses **floor division** (`>> 1`), not truncation (`/ 2`). This matches C# unsigned arithmetic semantics in the reference implementation.
- Match copies may cross 64KB quantum boundaries. Do not cap copies at boundary.
- The stream is consumed as **u16 words** (LE), not raw bytes. This distinguishes Granny2 BitKnit from Oodle BitKnit.
- `pop_cdf` uses u64 intermediate arithmetic to avoid u32 overflow.
- Only LE (little-endian) files are supported. BE detection exists but parsing is unimplemented.

## Conventions

- `pub(crate)` for cross-module visibility
- Avoid unnecessary allocations — prefer slices over owned copies where lifetime allows
- Keep error types per-module (`bitknit::Error`, `reader::Error`) with `Display` impls
