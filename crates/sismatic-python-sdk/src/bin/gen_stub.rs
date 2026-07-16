//! Write the Python type stub from the instruction catalogs.
//!
//! Run from anywhere in the workspace: `cargo run -p sismatic-python --bin
//! gen_stub`. The output path is anchored to this crate's directory (via
//! `CARGO_MANIFEST_DIR`) so the generator and the freshness test in `stub.rs`
//! always target the same file regardless of the caller's working directory.

use std::path::Path;

const STUB_REL_PATH: &str = "python/sismatic/__init__.pyi";

fn main() -> std::io::Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(STUB_REL_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, sismatic::stub::generate())?;
    println!("wrote {}", path.display());
    Ok(())
}
