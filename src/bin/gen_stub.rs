//! Write the Python type stub from the instruction catalogs.
//!
//! Run from the crate root: `cargo run --bin gen_stub`. The output path is
//! fixed to the package's stub file so the generator and the freshness test in
//! `stub.rs` always target the same location.

use std::path::Path;

const STUB_PATH: &str = "python/sismatic/__init__.pyi";

fn main() -> std::io::Result<()> {
    let path = Path::new(STUB_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, sismatic::stub::generate())?;
    println!("wrote {STUB_PATH}");
    Ok(())
}
