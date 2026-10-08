//! Compress what the binary serves as static bytes, once, at build time: the
//! Scalar bundle behind `/api`, and — under the `ui` feature — the http-ui
//! frontend behind `/ui`.
//!
//! # The Scalar bundle
//!
//! The bundle is a 4 MB JavaScript file and it is the largest single thing in
//! `sismatic-server` by a wide margin — about a quarter of the binary. It is
//! also ordinary minified JavaScript, which deflates to a little over a quarter
//! of its size, and the compressed form is *also* what an HTTP client wants: a
//! browser asks for `gzip` on every request it makes. So compressing here pays
//! twice, and the uncompressed bytes need never exist in the binary at all.
//!
//! ## Why a build script rather than a compressed embed
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
//! # The frontend
//!
//! Same treatment, different source: the bytes are a directory vite wrote
//! rather than a crate's embedded asset, so they arrive as a *path* — see
//! [`dist_dir`] for the three places that path can come from and why the third
//! one is load-bearing. Everything downstream of the path is identical to the
//! bundle above: gzip at `best`, one file per asset in `OUT_DIR`, and a table
//! `src/ui.rs` includes.
//!
//! The table is generated rather than written out because vite content-hashes
//! its filenames (`index-Dh_XKfyo.js`), so any hand-maintained list rots on the
//! next `pnpm build`. Generating it also means the set of files the server can
//! serve *is* the set of files vite emitted — there is no second list to fall
//! out of step with the first.
//!
//! # What comes out
//!
//! * `$OUT_DIR/scalar.js.gz` — gzip framing rather than raw deflate, because
//!   the bytes are served as-is under `Content-Encoding: gzip` and that is the
//!   coding every client already understands. `src/openapi.rs` includes it.
//! * `$OUT_DIR/ui/**` — one file per frontend asset, gzipped unless the format
//!   is already compressed.
//! * `$OUT_DIR/ui_assets.rs` — the table over them. `src/ui.rs` includes it.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Where a caller can say which build of the frontend to embed.
///
/// Set by the flake for every derivation that compiles this crate with the `ui`
/// feature, to a `/nix/store` path — so under nix the frontend is an *input* to
/// the Rust build rather than something a developer has to remember to rebuild.
const DIST_ENV: &str = "SISMATIC_HTTP_UI_DIST";

fn main() {
    // The only input of this script itself. Without this, cargo re-runs it
    // whenever *any* file in the package changes, which is every edit to every
    // route. The Scalar bundle is not on the filesystem here to be watched — it
    // arrives through a build-dependency, and cargo already re-runs a build
    // script whose dependencies changed, so a bumped `scalar_api_reference` is
    // picked up without being named. The frontend's inputs are named by
    // `embed_ui`, which knows where they are.
    println!("cargo::rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    embed_scalar(&out);
    embed_ui(&out);
}

/// Compress the Scalar bundle into `$OUT_DIR/scalar.js.gz`.
fn embed_scalar(out: &Path) {
    let js = scalar_api_reference::get_asset("scalar.js")
        .expect("the Scalar bundle embedded in scalar_api_reference");

    let out = out.join("scalar.js.gz");
    std::fs::write(&out, gzip(&js)).expect("writing the compressed bundle");
}

/// Compress the frontend into `$OUT_DIR/ui/**` and write the table over it.
fn embed_ui(out: &Path) {
    println!("cargo::rerun-if-env-changed={DIST_ENV}");

    // The feature gates the *work*, not the generated file. Writing the table
    // unconditionally costs one gzip of a few hundred bytes and means a build
    // with the feature on can never meet an `OUT_DIR` that a build with it off
    // left without one.
    let dist = if std::env::var_os("CARGO_FEATURE_UI").is_some() {
        dist_dir()
    } else {
        None
    };

    let mut rows = Vec::new();
    match &dist {
        Some(dir) => {
            // Sorted, so the generated file is a function of the directory's
            // contents and not of the order the filesystem happened to list
            // them in — otherwise two builds of identical input produce
            // different `.rs` text and nothing downstream can be cached.
            let mut files = Vec::new();
            collect(dir, dir, &mut files);
            files.sort();
            assert!(
                files.iter().any(|relative| relative == "index.html"),
                "{} has no index.html — is it a vite dist directory?",
                dir.display()
            );
            for relative in files {
                rows.push(write_asset(out, &dir.join(&relative), &relative));
            }
        }
        None => {
            // Watch the directory the dist *would* appear in, when there is one
            // to watch. Without this a developer's first `pnpm build` would not
            // rebuild anything: the branch below names no input, so cargo would
            // have no reason to re-run this script and the placeholder would
            // stay embedded until something else forced a rebuild. Creating
            // `http-ui/dist` bumps `http-ui`'s own mtime, which is what makes
            // the parent the right thing to watch.
            //
            // Guarded on existence rather than emitted unconditionally, because
            // cargo cannot stat a path that is not there and re-runs the script
            // on every build when it cannot — which would re-compress the 4 MB
            // Scalar bundle every time. In the packaged crate, which is the
            // build that gets here on purpose, there is nothing to watch and so
            // nothing to re-run for.
            let sibling = PathBuf::from(
                std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"),
            )
            .join("../../http-ui");
            if sibling.is_dir() {
                println!("cargo::rerun-if-changed={}", sibling.display());
            }

            // No dist, and this is not a failure: see `dist_dir`. A page that
            // says so beats a 404 that leaves an operator guessing whether the
            // route is wrong or the build was.
            println!(
                "cargo::warning=no frontend to embed (set {DIST_ENV} or run `pnpm build` in \
                 http-ui/); /ui will serve a placeholder"
            );
            rows.push(write_bytes(
                out,
                "index.html",
                PLACEHOLDER.as_bytes(),
                "text/html; charset=utf-8",
            ));
        }
    }

    // No flag beside the table saying which of the two branches above ran: the
    // table *is* the answer, because only a real dist carries vite's hashed
    // `assets/` output. `ui::tests::a_build_given_a_frontend_embeds_it` reads it
    // that way rather than trusting a generated boolean.
    let table = format!(
        "// @generated by build.rs — do not edit.\n\
         pub(crate) static ASSETS: &[Asset] = &[\n{}];\n",
        rows.concat()
    );
    std::fs::write(out.join("ui_assets.rs"), table).expect("writing the frontend asset table");
}

/// Which build of the frontend to embed, in the order the three answers are
/// tried.
///
/// 1. `SISMATIC_HTTP_UI_DIST`, which is how the flake hands over a frontend it
///    built itself. This is the answer every CI build and every release
///    artifact takes.
/// 2. `http-ui/dist` beside this crate, so `cargo build` on a laptop picks up
///    whatever `pnpm build` last wrote without any environment to remember.
/// 3. None, and *this one is why the function returns an `Option`*. Under
///    release-plz's `git_only` mode, `cargo package --allow-dirty --workspace`
///    packages this crate and verify-builds the extracted copy — in which
///    `../../http-ui/dist` resolves to `target/package/http-ui/dist`, a
///    directory that does not exist and never will, because `cargo package`
///    includes only files under the crate root. A script that panicked here
///    would therefore not fail a developer's build; it would fail the release
///    PR, which is a failure this workspace has already paid for twice (see
///    release-plz.toml). So the third answer embeds a placeholder, and the
///    property that keeps a *shipped* binary from carrying it is that the flake
///    always answers (1).
fn dist_dir() -> Option<PathBuf> {
    if let Some(dist) = std::env::var_os(DIST_ENV) {
        let dist = PathBuf::from(dist);
        assert!(
            dist.is_dir(),
            "{DIST_ENV} is set to {}, which is not a directory",
            dist.display()
        );
        return Some(dist);
    }

    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let sibling = manifest.join("../../http-ui/dist");
    sibling.is_dir().then_some(sibling)
}

/// Every file under `dir`, as paths relative to `root` with `/` separators.
///
/// Emits a watch on each directory as it descends and on each file it finds.
/// The directories are not redundant: a watch on a file cannot notice a
/// *sibling* appearing, and a watch on a directory only reports that directory's
/// own mtime — so neither kind alone sees the whole of what `pnpm build` does.
fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    println!("cargo::rerun-if-changed={}", dir.display());

    for entry in std::fs::read_dir(dir).expect("reading the frontend dist directory") {
        let path = entry.expect("reading a dist directory entry").path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        println!("cargo::rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(root)
            .expect("a walked path is under the root it was walked from");
        out.push(
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        );
    }
}

/// Compress `file` into `$OUT_DIR/ui/<relative>` and return its table row.
fn write_asset(out: &Path, file: &Path, relative: &str) -> String {
    let bytes = std::fs::read(file).expect("reading a frontend asset");
    write_bytes(out, relative, &bytes, mime_of(relative))
}

/// Store `bytes` under `$OUT_DIR/ui/<relative>` and return its table row.
///
/// Compressed unless the format already is: gzipping a `.woff2` or a `.png`
/// spends build time to make the response larger, and the handler would then
/// have to inflate it for a client that could have taken the original.
fn write_bytes(out: &Path, relative: &str, bytes: &[u8], mime: &str) -> String {
    let gzipped = compressible(relative);
    let body = if gzipped { gzip(bytes) } else { bytes.to_vec() };

    let path = out.join("ui").join(relative);
    let path = if gzipped {
        path.with_file_name(format!(
            "{}.gz",
            path.file_name()
                .expect("an asset path ends in a file name")
                .to_string_lossy()
        ))
    } else {
        path
    };
    std::fs::create_dir_all(path.parent().expect("an asset path has a parent"))
        .expect("creating the asset output directory");
    std::fs::write(&path, body).expect("writing a compressed frontend asset");

    let include = path.display().to_string();
    format!(
        "    Asset {{ path: {relative:?}, mime: {mime:?}, gzipped: {gzipped}, \
         body: include_bytes!({include:?}) }},\n"
    )
}

/// gzip at `best` rather than the default: this runs once per build of one
/// crate and the difference is tens of milliseconds, against bytes that are paid
/// for in every binary and on every page load for the life of the release.
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).expect("compressing an asset");
    encoder.finish().expect("finishing the gzip stream")
}

/// Whether gzipping this file is worth the bytes.
fn compressible(relative: &str) -> bool {
    !matches!(
        extension(relative).as_str(),
        "woff" | "woff2" | "png" | "jpg" | "jpeg" | "webp" | "avif" | "gif" | "ico" | "gz" | "br"
    )
}

/// The `Content-Type` a browser needs for this file to be usable.
///
/// A short table rather than a `mime_guess` dependency: the whole input is one
/// vite build's output, and the cost of a type this does not know is a
/// download-prompt on one asset rather than a wrong answer somewhere subtle.
fn mime_of(relative: &str) -> &'static str {
    match extension(relative).as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

/// The lowercased extension of `relative`, or `""`.
fn extension(relative: &str) -> String {
    Path::new(relative)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// What `/ui` serves when the build was given no frontend.
///
/// Deliberately self-contained — no asset, no script, no off-host reference —
/// because the one situation it is shown in is the one where nothing else about
/// the frontend worked.
const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Sismatic</title>
  </head>
  <body>
    <h1>No frontend in this build</h1>
    <p>
      This binary was compiled with the <code>ui</code> feature but without a
      built frontend to embed. The API itself is unaffected: see
      <a href="/api">/api</a> for the reference and <code>/v1</code> for the
      routes.
    </p>
    <p>To build one, either</p>
    <pre>nix build .#server</pre>
    <p>which builds the frontend as an input, or</p>
    <pre>cd http-ui &amp;&amp; pnpm build
cargo build -p sismatic-server</pre>
  </body>
</html>
"#;
