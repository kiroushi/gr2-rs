use gr2_rs::element::{Field, Value};
use gr2_rs::format::*;
use gr2_rs::reader::Gr2File;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let show_types = args.iter().any(|a| a == "--types");
    let path_arg = args.iter().skip(1).find(|a| !a.starts_with('-'));

    let path_str = path_arg.unwrap_or_else(|| {
        eprintln!("Usage: gr2-rs [--types] <file.model>");
        std::process::exit(1);
    });

    let path = Path::new(path_str);
    eprintln!("Loading: {}", path.display());

    let file_data = std::fs::read(path).expect("failed to read file");
    let gr2 = match Gr2File::parse(&file_data) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    println!("=== GR2 Header ===");
    println!(
        "  Format: {:?} {:?}",
        gr2.magic.format.endianness, gr2.magic.format.pointer_width
    );
    println!("  {}", gr2.header);
    println!(
        "  Root type: section {} offset 0x{:x}",
        gr2.header.root_type.section, gr2.header.root_type.offset
    );
    println!(
        "  Root node: section {} offset 0x{:x}",
        gr2.header.root_node.section, gr2.header.root_node.offset
    );

    println!("\n=== Sections ===");
    for (i, sect) in gr2.sections.iter().enumerate() {
        let sh = &sect.header;
        println!(
            "  [{i}] {}: {} -> {} bytes, base=0x{:x}, relocs={}, marshal={}",
            sh.compression,
            sh.compressed_size,
            sh.uncompressed_size,
            sect.base_address,
            sh.num_relocations,
            sh.num_mixed_marshalling
        );
    }

    println!("\n=== Flat buffer: {} bytes ===", gr2.flat.len());

    // Optionally dump type definitions
    if show_types {
        if let Some(type_offset) = gr2.resolve_ref(gr2.header.root_type) {
            println!("\n=== Root Type Definition (at 0x{type_offset:x}) ===");
            match gr2.walk_struct_def(type_offset) {
                Ok(members) => {
                    for m in &members {
                        let arr = if m.array_size > 0 {
                            format!("[{}]", m.array_size)
                        } else {
                            String::new()
                        };
                        let child = if m.children_ptr > 0 {
                            format!(" -> children@0x{:x}", m.children_ptr)
                        } else {
                            String::new()
                        };
                        println!("  {:30} : {}{}{}", m.name, m.member_type, arr, child);
                    }

                    for m in &members {
                        dump_child_type(&gr2, m, 1);
                    }
                }
                Err(e) => eprintln!("  Error reading type: {e}"),
            }
        }
    }

    // Extract and print root node data
    println!("\n=== Root Node ===");
    match gr2.extract_root() {
        Ok(fields) => {
            for f in &fields {
                print_field(f, 1);
            }
        }
        Err(e) => eprintln!("  Error extracting root: {e}"),
    }
}

fn dump_child_type(gr2: &Gr2File, m: &gr2_rs::reader::MemberDef, depth: usize) {
    if depth > 3 {
        return;
    }
    if m.children_ptr == 0 || m.children_ptr >= gr2.flat.len() {
        return;
    }
    if !matches!(
        m.member_type,
        MemberType::Inline
            | MemberType::Reference
            | MemberType::ReferenceToArray
            | MemberType::ArrayOfReferences
    ) {
        return;
    }

    let indent = "  ".repeat(depth + 1);
    if let Ok(children) = gr2.walk_struct_def(m.children_ptr) {
        if !children.is_empty() {
            println!("{indent}[{}]:", m.name);
            for c in &children {
                let arr = if c.array_size > 0 {
                    format!("[{}]", c.array_size)
                } else {
                    String::new()
                };
                println!("{indent}  {:30} : {}{}", c.name, c.member_type, arr);
            }
            for c in &children {
                dump_child_type(gr2, c, depth + 1);
            }
        }
    }
}

fn print_field(field: &Field, depth: usize) {
    let indent = "  ".repeat(depth);
    match &field.value {
        Value::Int8(v) => println!("{indent}{:30} = {v}", field.name),
        Value::UInt8(v) => println!("{indent}{:30} = {v}", field.name),
        Value::Int16(v) => println!("{indent}{:30} = {v}", field.name),
        Value::UInt16(v) => println!("{indent}{:30} = {v}", field.name),
        Value::Int32(v) => println!("{indent}{:30} = {v}", field.name),
        Value::UInt32(v) => println!("{indent}{:30} = {v}", field.name),
        Value::Real16(v) => println!("{indent}{:30} = 0x{v:04x} (f16)", field.name),
        Value::Real32(v) => println!("{indent}{:30} = {v}", field.name),
        Value::String(Some(s)) => println!("{indent}{:30} = \"{s}\"", field.name),
        Value::String(None) => println!("{indent}{:30} = null", field.name),
        Value::Reference { offset } => match offset {
            Some(o) => println!("{indent}{:30} -> 0x{o:x}", field.name),
            None => println!("{indent}{:30} -> null", field.name),
        },
        Value::ReferenceToArray { count, offset } => match offset {
            Some(o) => println!("{indent}{:30} -> [{count} items] @ 0x{o:x}", field.name),
            None => println!("{indent}{:30} -> [{count} items] null", field.name),
        },
        Value::ArrayOfReferences { count, offset } => match offset {
            Some(o) => println!("{indent}{:30} -> [{count} refs] @ 0x{o:x}", field.name),
            None => println!("{indent}{:30} -> [{count} refs] null", field.name),
        },
        Value::VariantReference { type_offset, data_offset } => {
            println!(
                "{indent}{:30} -> variant(type={}, data={})",
                field.name,
                fmt_ptr(*type_offset),
                fmt_ptr(*data_offset),
            );
        }
        Value::ReferenceToVariantArray { type_offset, count, data_offset } => {
            println!(
                "{indent}{:30} -> variant_array(type={}, count={count}, data={})",
                field.name,
                fmt_ptr(*type_offset),
                fmt_ptr(*data_offset),
            );
        }
        Value::Transform { flags, translation, .. } => {
            println!(
                "{indent}{:30} = Transform(flags=0x{flags:x}, t={translation:?})",
                field.name
            );
        }
        Value::EmptyReference => {
            println!("{indent}{:30} = <empty ref>", field.name);
        }
        Value::Struct(fields) => {
            println!("{indent}{}:", field.name);
            for f in fields {
                print_field(f, depth + 1);
            }
        }
        Value::Array(items) => {
            if items.iter().all(|v| matches!(v, Value::Struct(_))) {
                println!("{indent}{} [{} structs]:", field.name, items.len());
                for (i, item) in items.iter().enumerate() {
                    if let Value::Struct(fields) = item {
                        println!("{indent}  [{i}]:");
                        for f in fields {
                            print_field(f, depth + 2);
                        }
                    }
                }
            } else {
                let vals: Vec<String> = items.iter().map(|v| format!("{v:?}")).collect();
                println!("{indent}{:30} = [{}]", field.name, vals.join(", "));
            }
        }
    }
}

fn fmt_ptr(ptr: Option<usize>) -> String {
    match ptr {
        Some(o) => format!("0x{o:x}"),
        None => "null".into(),
    }
}
