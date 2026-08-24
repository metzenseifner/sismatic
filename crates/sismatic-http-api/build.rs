//! Compress the Scalar bundle once, at build time, so the binary carries a
//! megabyte instead of four.
//!
//! The bundle is a 4 MB JavaScript file and it is the largest single thing in
//! `sismatic-server` by a wide margin — about a quarter of the binary. It is
//! also ordinary minified JavaScript, which deflates to a little over a quarter
//! of its size, and the compressed form is *also* what an HTTP client wants: a
//! browser asks for `gzip` on every request it makes. So compressing here pays
//! twice, and the uncompressed bytes need never exist in the binary at all.
//!
//! # Why a build script rather than a compressed embed
//!
//! `rust-embed`, which is how [`scalar_api_reference`] hands the bundle over,
//! has a `compression` feature that would do something similar. Turning it on
//! means enabling a feature *on a dependency of a dependency* and having cargo's
//! feature unification rewrite the code `scalar_api_reference`'s derive
//! generates — a crate that was not written with that feature in mind. It also
//! drags in `zstd-sys`, which is a C library built by `cc`, for an algorithm it
//! would not even use. Measured on this bundle, that route also lands about
//! 500 KB heavier than this one, because the compression level is not ours to
//! pick.
//!
//! This is the same idea with the blast radius removed: one dependency asked for
//! bytes, one compressor, one file in `OUT_DIR`, and `scalar_api_reference`
//! demoted to a build-dependency so nothing it brings with it reaches the
//! shipped binary.
//!
//! # What comes out
//!
//! `$OUT_DIR/scalar.js.gz` — gzip framing rather than raw deflate, because the
//! bytes are served as-is under `Content-Encoding: gzip` and that is the coding
//! every client already understands. `src/openapi.rs` includes it.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    // The only input. Without this, cargo re-runs the script whenever *any*
    // file in the package changes, which is every edit to every route. The
    // bundle is not on the filesystem here to be watched — it arrives through a
    // build-dependency, and cargo already re-runs a build script whose
    // dependencies changed, so a bumped `scalar_api_reference` is picked up
    // without being named.
    println!("cargo::rerun-if-changed=build.rs");

    let js = scalar_api_reference::get_asset("scalar.js")
        .expect("the Scalar bundle embedded in scalar_api_reference");

    // `best` rather than the default: this runs once per build of one crate and
    // the difference is tens of milliseconds, against bytes that are paid for in
    // every binary and on every page load for the life of the release.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(&js)
        .expect("compressing the Scalar bundle");
    let gzipped = encoder.finish().expect("finishing the gzip stream");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("scalar.js.gz");
    std::fs::write(&out, gzipped).expect("writing the compressed bundle");
}
