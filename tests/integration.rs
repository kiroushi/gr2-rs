//! End-to-end tests for GR2 parsing against real .model files.
//!
//! Set `GR2_TEST_DIR` to a directory containing .model files.
//! All tests are `#[ignore]`d so `cargo test` passes without external files.
//!
//! Run with: GR2_TEST_DIR=/path/to/models cargo test -- --ignored

use gr2_rs::element::Value;
use gr2_rs::reader::Gr2File;
use std::path::PathBuf;

fn test_dir() -> PathBuf {
    std::env::var("GR2_TEST_DIR")
        .expect("GR2_TEST_DIR not set — point it at a directory containing .model files")
        .into()
}

fn collect_model_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_model_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "model") {
            out.push(path);
        }
    }
}

fn model_files() -> Vec<PathBuf> {
    let dir = test_dir();
    let mut files = Vec::new();
    collect_model_files(&dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .model files found in {}", dir.display());
    files
}

fn first_model() -> PathBuf {
    model_files().into_iter().next().unwrap()
}

#[test]
#[ignore]
fn parse_single_model() {
    let path = first_model();
    let gr2 = Gr2File::load(&path)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    assert!(gr2.header.version == 6 || gr2.header.version == 7);
    assert!(gr2.header.num_sections > 0);
    assert!(!gr2.flat.is_empty());
    assert_eq!(gr2.sections.len(), gr2.header.num_sections as usize);
}

#[test]
#[ignore]
fn parse_all_models() {
    let files = model_files();
    let mut success = 0;
    let mut failures = Vec::new();

    for path in &files {
        match Gr2File::load(path) {
            Ok(_) => success += 1,
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} files failed to parse:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
    eprintln!("successfully parsed {success} .model files");
}

#[test]
#[ignore]
fn sections_have_expected_properties() {
    let path = first_model();
    let gr2 = Gr2File::load(&path).unwrap();

    assert!(
        gr2.sections.iter().any(|s| s.header.has_data()),
        "at least one section should contain data"
    );

    // Base addresses must be non-decreasing
    for w in gr2.sections.windows(2) {
        assert!(
            w[0].base_address <= w[1].base_address,
            "section base addresses must be non-decreasing"
        );
    }

    // Flat buffer size = sum of decompressed section sizes
    let total: usize = gr2.sections.iter().map(|s| s.header.uncompressed_size as usize).sum();
    assert_eq!(gr2.flat.len(), total);
}

#[test]
#[ignore]
fn root_type_resolves() {
    let path = first_model();
    let gr2 = Gr2File::load(&path).unwrap();

    if gr2.header.root_type.is_valid() {
        let offset = gr2
            .resolve_ref(gr2.header.root_type)
            .expect("valid root_type ref should resolve");
        let members = gr2
            .walk_struct_def(offset)
            .expect("root type should be a valid struct definition");
        assert!(!members.is_empty(), "root type should have at least one member");

        for m in &members {
            assert!(!m.name.is_empty(), "member names should not be empty");
        }
    }
}

#[test]
#[ignore]
fn root_node_has_from_file_name() {
    let path = first_model();
    let gr2 = Gr2File::load(&path).unwrap();

    let type_off = gr2
        .resolve_ref(gr2.header.root_type)
        .expect("root type should resolve");
    let members = gr2.walk_struct_def(type_off).expect("root type should parse");

    let has_ffn = members.iter().any(|m| m.name == "FromFileName");
    if !has_ffn {
        eprintln!(
            "note: {} lacks FromFileName field (may be expected for non-D2R files)",
            path.display()
        );
        return;
    }

    let node_off = gr2
        .resolve_ref(gr2.header.root_node)
        .expect("root node should resolve");
    assert!(
        node_off < gr2.flat.len(),
        "root node offset should be within flat buffer"
    );
}

#[test]
#[ignore]
fn decompressed_data_is_deterministic() {
    let path = first_model();
    let gr2_a = Gr2File::load(&path).unwrap();
    let gr2_b = Gr2File::load(&path).unwrap();
    assert_eq!(gr2_a.flat, gr2_b.flat, "parsing the same file twice must produce identical flat buffers");
}

// --- Oodle1-specific integration tests ---
// Set GR2_TEST_DIR_OODLE1 to a directory containing .model files that use Oodle1 compression.

fn oodle1_test_dir() -> Option<PathBuf> {
    std::env::var("GR2_TEST_DIR_OODLE1").ok().map(PathBuf::from)
}

fn oodle1_model_files() -> Option<Vec<PathBuf>> {
    let dir = oodle1_test_dir()?;
    let mut files = Vec::new();
    collect_model_files(&dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .model files found in {}", dir.display());
    Some(files)
}

#[test]
#[ignore]
fn parse_oodle1_models() {
    let Some(files) = oodle1_model_files() else {
        eprintln!("skipping: GR2_TEST_DIR_OODLE1 not set");
        return;
    };
    let mut success = 0;
    let mut failures = Vec::new();

    for path in &files {
        match Gr2File::load(path) {
            Ok(_) => success += 1,
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} Oodle1 files failed to parse:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
    eprintln!("successfully parsed {success} Oodle1 .model files");
}

#[test]
#[ignore]
fn extract_root_on_real_file() {
    let path = first_model();
    let gr2 = Gr2File::load(&path)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    let fields = gr2
        .extract_root()
        .unwrap_or_else(|e| panic!("failed to extract root: {e}"));
    assert!(!fields.is_empty(), "root node should have at least one field");

    for f in &fields {
        assert!(!f.name.is_empty(), "extracted field names should not be empty");
    }
}

#[test]
#[ignore]
fn validate_crc_on_real_file() {
    let path = first_model();
    let data = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    Gr2File::validate_crc(&data)
        .unwrap_or_else(|e| panic!("CRC validation failed for {}: {e}", path.display()));
}

#[test]
#[ignore]
fn oodle1_sections_decompress_to_expected_size() {
    let Some(files) = oodle1_model_files() else {
        eprintln!("skipping: GR2_TEST_DIR_OODLE1 not set");
        return;
    };
    let path = &files[0];
    let gr2 = Gr2File::load(path)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    // Verify flat buffer size matches sum of decompressed section sizes
    let total: usize = gr2.sections.iter().map(|s| s.header.uncompressed_size as usize).sum();
    assert_eq!(gr2.flat.len(), total,
        "flat buffer size should equal sum of decompressed section sizes");
}

#[test]
#[ignore]
fn resolve_variant_array_vertices() {
    let path = first_model();
    let gr2 = Gr2File::load(&path)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    let fields = gr2.extract_root().unwrap();

    // Find Meshes array
    let meshes_field = fields.iter().find(|f| f.name == "Meshes");
    let Some(meshes_field) = meshes_field else {
        eprintln!("note: {} has no Meshes field, skipping", path.display());
        return;
    };

    // Resolve the Meshes array — need its type from the root type def
    let root_type_off = gr2.resolve_ref(gr2.header.root_type).unwrap();
    let root_members = gr2.walk_struct_def(root_type_off).unwrap();
    let meshes_member = root_members.iter().find(|m| m.name == "Meshes").unwrap();

    let meshes = match gr2.resolve_array(&meshes_field.value, meshes_member.children_ptr as usize) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("note: {} Meshes array is empty or unresolvable", path.display());
            return;
        }
    };

    assert!(!meshes.is_empty(), "model should have at least one mesh");

    // For each mesh, find PrimaryVertexData (Reference), resolve it,
    // then find Vertices (ReferenceToVariantArray) and resolve that.
    let mesh_type_members = gr2.walk_struct_def(meshes_member.children_ptr as usize).unwrap();
    let pvd_member = mesh_type_members.iter().find(|m| m.name == "PrimaryVertexData");
    let Some(pvd_member) = pvd_member else {
        eprintln!("note: mesh type has no PrimaryVertexData field");
        return;
    };

    for (i, mesh) in meshes.iter().enumerate() {
        let mesh_name = mesh.iter()
            .find(|f| f.name == "Name")
            .and_then(|f| if let Value::String(Some(s)) = &f.value { Some(s.as_str()) } else { None })
            .unwrap_or("<unnamed>");

        let pvd_field = mesh.iter().find(|f| f.name == "PrimaryVertexData");
        let Some(pvd_field) = pvd_field else { continue };

        let pvd = match gr2.resolve_value(&pvd_field.value, pvd_member.children_ptr as usize) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let vertices_field = pvd.iter().find(|f| f.name == "Vertices");
        let Some(vertices_field) = vertices_field else { continue };

        if let Value::ReferenceToVariantArray { type_offset: Some(_), count, data_offset: Some(_) } = &vertices_field.value {
            let verts = gr2.resolve_variant_array(&vertices_field.value)
                .unwrap_or_else(|e| panic!("failed to resolve vertices for mesh {i} ({mesh_name}): {e}"));
            assert_eq!(verts.len(), *count as usize);
            eprintln!("mesh {i} ({mesh_name}): {count} vertices, {} components each", verts[0].len());
        }
    }
}

#[test]
#[ignore]
fn resolve_variant_array_all_models() {
    let files = model_files();
    let mut total_meshes = 0;
    let mut total_verts = 0;
    let mut failures = Vec::new();

    for path in &files {
        let gr2 = match Gr2File::load(path) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let fields = match gr2.extract_root() {
            Ok(f) => f,
            Err(_) => continue,
        };

        let meshes_field = match fields.iter().find(|f| f.name == "Meshes") {
            Some(f) => f,
            None => continue,
        };

        let root_type_off = match gr2.resolve_ref(gr2.header.root_type) {
            Some(o) => o,
            None => continue,
        };
        let root_members = match gr2.walk_struct_def(root_type_off) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let meshes_member = match root_members.iter().find(|m| m.name == "Meshes") {
            Some(m) => m,
            None => continue,
        };

        let meshes = match gr2.resolve_array(&meshes_field.value, meshes_member.children_ptr as usize) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let mesh_type_members = match gr2.walk_struct_def(meshes_member.children_ptr as usize) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let pvd_member = match mesh_type_members.iter().find(|m| m.name == "PrimaryVertexData") {
            Some(m) => m,
            None => continue,
        };

        for mesh in &meshes {
            total_meshes += 1;

            let pvd_field = match mesh.iter().find(|f| f.name == "PrimaryVertexData") {
                Some(f) => f,
                None => continue,
            };
            let pvd = match gr2.resolve_value(&pvd_field.value, pvd_member.children_ptr as usize) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let vertices_field = match pvd.iter().find(|f| f.name == "Vertices") {
                Some(f) => f,
                None => continue,
            };

            if let Value::ReferenceToVariantArray { type_offset: Some(_), count, data_offset: Some(_) } = &vertices_field.value {
                match gr2.resolve_variant_array(&vertices_field.value) {
                    Ok(verts) => {
                        assert_eq!(verts.len(), *count as usize);
                        total_verts += verts.len();
                    }
                    Err(e) => {
                        failures.push(format!("{}: {e}", path.display()));
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} vertex extraction failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("resolved vertices for {total_meshes} meshes ({total_verts} total vertices) across {} files", files.len());
}
