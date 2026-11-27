//! `include/wasm_backend.h` is checked in so that hosts need no Rust
//! tooling to read it. This keeps it in step with the functions the
//! library exports.

use std::path::Path;

#[test]
fn header_is_up_to_date() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).unwrap();
    let mut generated = Vec::new();
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen failed")
        .write(&mut generated);
    let generated = String::from_utf8(generated).unwrap();

    let path = crate_dir.join("include/wasm_backend.h");
    if std::env::var_os("UPDATE_HEADER").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    // Git may check the file out with other line endings.
    let checked_in = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        checked_in == generated,
        "include/wasm_backend.h is out of date: run `UPDATE_HEADER=1 cargo test -p wasm-backend-ffi --test header` and commit the result"
    );
}
