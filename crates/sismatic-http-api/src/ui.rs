//! The embedded frontend: `http-ui`, compiled into the binary and served from
//! one subtree of the URL space.
//!
//! ```text
//! GET /              redirects to /ui/
//! GET /ui/           the application
//! GET /ui/{path}     an asset, or the application again — see `is_route`
//! ```
//!
//! # Why this crate and not the composition root
//!
//! The frontend's compile-time contract is *this crate's* OpenAPI document:
//! `http-ui/src/api/schema.d.ts` is generated from `/api-docs/openapi.json` by
//! `pnpm gen:api`, which is to say from [`ApiDoc`](crate::ApiDoc). Client and
//! contract therefore belong to one unit, and keeping them in one crate makes a
//! renamed field a build-order fact rather than a thing to stay vigilant about.
//! The composition root contributes nothing to that contract, and holding an
//! asset table, a content-type map and a cache policy would be the first
//! delivery-mechanism detail it ever carried.
//!
//! What the root does keep is the *composition* decision — whether a binary
//! ships a frontend at all — and it makes it by enabling the `ui` feature on
//! this crate. With the feature off, [`Mount`] is an empty struct whose
//! [`apply`](Mount::apply) registers nothing, so no route exists, no bytes are
//! linked, and this crate is still a plain API library.
//!
//! # Why a subtree rather than the root
//!
//! A single-page application wants every unrouted path to answer with its own
//! `index.html`, so the client-side router can read the URL. Served from `/`,
//! that would mean the server answers `200 text/html` to very nearly
//! everything — and the deliberate `404`s and `405`s this application is built
//! out of would quietly become near-misses that all resolve. `/health` would
//! stop being a 404, which is the complement that makes a `200` from
//! `/health_check` mean anything (`tests/health_check.rs`), and
//! `the_slash_is_folded_for_the_ui_only` would stop being true.
//!
//! Under `/ui` the fallback cannot reach any of them. It also removes the
//! registration-order hazard entirely: the catch-all lives inside a
//! [`web::scope`], so it is unable to shadow a route outside that scope no
//! matter where [`Mount::apply`] is called from — which is worth more than it
//! sounds, because the alternative is an invariant that holds only as long as
//! nobody reorders the `.service(..)` calls in [`crate::startup`].
//!
//! The cost is one line of vite configuration: `base: "/ui/"`, so the asset
//! URLs in `index.html` are absolute under the same subtree.
//!
//! # How the bytes get here
//!
//! `build.rs` gzips one file per asset into `OUT_DIR` and generates the table
//! [`ASSETS`] over them, which is `include!`d below. So an asset is a
//! `&'static [u8]` pointing into the binary's own image, and [`Assets::render`]
//! wraps each in a [`Bytes`] that borrows rather than owns — the same trick
//! [`Docs`](crate::openapi::Docs) uses for the Scalar bundle, for the same
//! reason: one copy, shared by every worker, and a response is a refcount bump.
//!
//! Storing the compressed form is what the client wanted anyway; [`serve`]
//! inflates only for a client that explicitly refuses a compressed response.

#[cfg(feature = "ui")]
pub use enabled::{Mount, UI_PATH};

#[cfg(not(feature = "ui"))]
pub use disabled::Mount;

#[cfg(feature = "ui")]
mod enabled {
    use std::collections::HashMap;

    use actix_web::web::{Bytes, ServiceConfig};
    use actix_web::{HttpRequest, HttpResponse, web};

    use crate::openapi::accepts_gzip;

    /// The subtree the application is served from.
    pub const UI_PATH: &str = "/ui";

    /// How long a content-hashed asset may be cached: vite puts the hash of the
    /// contents in the file name, so a changed asset is a changed URL and this
    /// one can never go stale.
    const IMMUTABLE: &str = "public, max-age=31536000, immutable";

    /// Everything else, `index.html` above all: its URL is fixed while its
    /// contents change with every deploy, so a cached copy would pin a browser
    /// to the asset names of an older build.
    const REVALIDATE: &str = "no-cache";

    /// One embedded file.
    pub(crate) struct Asset {
        /// Its path relative to the dist root, with `/` separators — which is
        /// also its path under [`UI_PATH`].
        path: &'static str,
        mime: &'static str,
        /// Whether `body` is gzipped. False for formats that are already
        /// compressed; see `build.rs`'s `compressible`.
        gzipped: bool,
        body: &'static [u8],
    }

    include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

    /// One asset, ready to serve.
    struct Entry {
        mime: &'static str,
        gzipped: bool,
        /// Borrowed from the binary's image, so cloning is a refcount bump.
        body: Bytes,
        /// Whether this asset's URL changes when its contents do.
        immutable: bool,
    }

    impl Entry {
        fn of(asset: &'static Asset) -> Self {
            Self {
                mime: asset.mime,
                gzipped: asset.gzipped,
                body: Bytes::from_static(asset.body),
                // Vite emits content-hashed files under `assets/` and copies
                // `public/` through unhashed, so the directory *is* the
                // cacheability. Deriving it from the path rather than taking a
                // build-time flag keeps one fewer thing in the generated table
                // that could disagree with what vite did.
                immutable: asset.path.starts_with("assets/"),
            }
        }
    }

    /// Every asset, indexed by the path it is served under.
    struct Assets {
        by_path: HashMap<&'static str, Entry>,
        /// `index.html`, the answer to every unrouted path under [`UI_PATH`].
        index: Entry,
    }

    impl Assets {
        /// # Panics
        ///
        /// If nothing named `index.html` was embedded. That is a build-time
        /// fact wearing a runtime type — `build.rs` asserts a dist has one and
        /// supplies one itself otherwise — and failing here makes it a crash on
        /// the way up rather than a 404 the first time someone opens the page.
        fn render() -> Self {
            Self {
                by_path: ASSETS
                    .iter()
                    .map(|asset| (asset.path, Entry::of(asset)))
                    .collect(),
                index: ASSETS
                    .iter()
                    .find(|asset| asset.path == "index.html")
                    .map(Entry::of)
                    .expect("the embedded frontend has an index.html"),
            }
        }
    }

    /// The frontend's routes, rendered once and shared by every worker.
    ///
    /// Cheap to clone — one `Arc` — which is what lets [`crate::startup`] build
    /// it outside the `HttpServer::new` closure and hand each worker the same
    /// assets rather than a copy of the table per thread.
    #[derive(Clone)]
    pub struct Mount(web::Data<Assets>);

    impl Mount {
        /// Wrap the embedded assets, ready to be registered.
        pub fn render() -> Self {
            Self(web::Data::new(Assets::render()))
        }

        /// Register the frontend on `cfg`.
        ///
        /// Order-independent by construction: everything but the `/` redirect
        /// lives under [`UI_PATH`], and `/` is an exact path. So this may be
        /// called anywhere in the builder chain without shadowing a route or
        /// being shadowed by one.
        pub fn apply(&self, cfg: &mut ServiceConfig) {
            cfg.app_data(self.0.clone());
            // The bare host is what an operator types, and nothing else serves
            // `/`. A redirect rather than the page itself, so there is one URL
            // the application is at.
            cfg.service(web::redirect("/", format!("{UI_PATH}/")));
            cfg.service(
                web::scope(UI_PATH)
                    // The scope root, `/ui` with no trailing slash. A tail
                    // pattern needs the leading `/` to match, so without this
                    // the slashless form would 404 — and unlike `/api`, which
                    // redirects, this serves the page directly: every asset URL
                    // in it is absolute under `/ui/`, so nothing about the page
                    // depends on which of the two URLs loaded it.
                    .service(web::resource("").route(web::get().to(index)))
                    .service(web::resource("/{tail:.*}").route(web::get().to(asset))),
            );
        }
    }

    /// The application, at the root of its subtree.
    async fn index(request: HttpRequest, assets: web::Data<Assets>) -> HttpResponse {
        serve(&request, &assets.index)
    }

    /// An asset, or the application for a path the client-side router owns.
    async fn asset(
        request: HttpRequest,
        assets: web::Data<Assets>,
        tail: web::Path<String>,
    ) -> HttpResponse {
        let tail = tail.into_inner();
        match assets.by_path.get(tail.as_str()) {
            Some(entry) => serve(&request, entry),
            None if is_route(&tail) => serve(&request, &assets.index),
            None => HttpResponse::NotFound().finish(),
        }
    }

    /// Whether `tail` is a path the client-side router should be handed, rather
    /// than a missing file.
    ///
    /// The distinction matters in the direction that is easy to get wrong.
    /// Answering `index.html` to *everything* means a mistyped script URL
    /// arrives as `200 text/html`, and what the browser then reports is a MIME
    /// type error on a file that exists as far as it can tell — rather than the
    /// 404 that would have said the asset is simply not there.
    ///
    /// So two things are files rather than routes: anything under `assets/`,
    /// which is vite's own output directory and therefore exhaustively known at
    /// build time, and anything whose last segment carries an extension. A
    /// client-side route does not (`/ui/devices/atrium-101`), and `/ui/` itself
    /// has no last segment at all, which is why the empty tail is a route.
    fn is_route(tail: &str) -> bool {
        !tail.starts_with("assets/")
            && !tail
                .rsplit('/')
                .next()
                .is_some_and(|segment| segment.contains('.'))
    }

    /// One asset as a response — gzipped if the client will take it that way,
    /// which every browser will.
    ///
    /// The stored form *is* the compressed form, so the common path copies
    /// nothing and compresses nothing. Inflating is the exceptional path and is
    /// done per request rather than once at startup for the same reason the
    /// Scalar bundle's is: a client that refuses `gzip` is rare enough that
    /// keeping a second uncompressed copy resident for its benefit would be
    /// paying forever for something that may never be asked for.
    fn serve(request: &HttpRequest, entry: &Entry) -> HttpResponse {
        let mut response = HttpResponse::Ok();
        response.content_type(entry.mime).insert_header((
            "cache-control",
            if entry.immutable {
                IMMUTABLE
            } else {
                REVALIDATE
            },
        ));

        if !entry.gzipped {
            return response.body(entry.body.clone());
        }

        // Two clients that disagree about `Accept-Encoding` get different bytes
        // from this one URL, so anything caching in between has to key on that
        // header rather than on the URL alone.
        response.insert_header(("vary", "accept-encoding"));

        if accepts_gzip(request) {
            return response
                .insert_header(("content-encoding", "gzip"))
                .body(entry.body.clone());
        }

        let mut body = Vec::new();
        match std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(entry.body.as_ref()),
            &mut body,
        ) {
            Ok(_) => response.body(body),
            // Unreachable short of memory corruption — the input is a constant
            // the build script produced with the very library reading it back.
            // Answered rather than panicked because one asset is not worth
            // taking a worker thread down over.
            Err(_) => HttpResponse::InternalServerError().finish(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The classification above, over the paths a browser actually asks
        /// for.
        #[test]
        fn a_path_without_a_file_name_is_the_router_s() {
            for route in [
                // `/ui/` itself.
                "",
                "devices",
                "devices/atrium-101",
                "groups/atrium/fields",
                // A trailing slash leaves an empty last segment, which carries
                // no extension and so is still a route.
                "devices/",
            ] {
                assert!(is_route(route), "{route} should have been a route");
            }

            for file in [
                // Vite's own output, known exhaustively at build time.
                "assets/index-Dh_XKfyo.js",
                "assets/nope.js",
                // Anything else that names a file.
                "favicon.svg",
                "icons.svg",
                "robots.txt",
                "nested/thing.css",
            ] {
                assert!(!is_route(file), "{file} should have been a file");
            }
        }

        /// Every embedded asset is reachable and classified, and the index is
        /// among them.
        #[test]
        fn the_table_is_indexed_by_the_path_it_is_served_under() {
            let assets = Assets::render();

            assert_eq!(assets.by_path.len(), ASSETS.len());
            for asset in ASSETS {
                let entry = assets
                    .by_path
                    .get(asset.path)
                    .unwrap_or_else(|| panic!("{} is not reachable", asset.path));
                assert_eq!(entry.immutable, asset.path.starts_with("assets/"));
            }
            assert_eq!(assets.index.mime, "text/html; charset=utf-8");
            assert!(!assets.index.immutable);
        }

        /// A build handed a frontend embeds *that* frontend, rather than
        /// falling through to the placeholder.
        ///
        /// Read off the table rather than off a flag the build script could set
        /// wrongly: vite's hashed output lives under `assets/`, and the
        /// placeholder is one `index.html` and nothing else, so the presence of
        /// an `assets/` entry *is* the distinction.
        ///
        /// `option_env!` reads at compile time, which is the right time — the
        /// question is not what the environment holds now but whether the build
        /// that produced these bytes was given a dist. Under the flake every
        /// derivation that compiles this crate with the `ui` feature is given
        /// one, so this asserts in CI and is a no-op on a laptop that has never
        /// run `pnpm build`.
        #[test]
        fn a_build_given_a_frontend_embeds_it() {
            if option_env!("SISMATIC_HTTP_UI_DIST").is_some() {
                assert!(
                    ASSETS.iter().any(|asset| asset.path.starts_with("assets/")),
                    "the build was handed a frontend but embedded {:?} — the placeholder?",
                    ASSETS.iter().map(|asset| asset.path).collect::<Vec<_>>()
                );
            }
        }

        /// The page reaches for nothing off this host.
        ///
        /// Embedding the frontend only settles where the page comes from, not
        /// where it goes once it is running — the same distinction
        /// `openapi::scalar_config` draws for the API reference, and the same
        /// reason it matters: these installations are normally on a network with
        /// no route off the LAN, so a stylesheet or a webfont from a CDN is not
        /// a slow page but a broken one.
        #[test]
        fn the_page_names_no_off_host_url() {
            let index = Assets::render().index;
            let mut html = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(index.body.as_ref()),
                &mut html,
            )
            .expect("inflating the embedded index.html");
            let html = String::from_utf8(html).expect("the page is UTF-8");

            for scheme in ["http://", "https://", "//fonts.", "//cdn."] {
                assert!(
                    !html.contains(scheme),
                    "the page reaches off-host via {scheme}: {html}"
                );
            }
        }
    }
}

/// What the frontend is when the `ui` feature is off: nothing at all.
///
/// Deliberately the same shape as the real [`Mount`](enabled::Mount) rather than
/// a `#[cfg]` at the call site, so [`crate::startup`] reads the same either way
/// and the feature cannot change the structure of the builder chain.
#[cfg(not(feature = "ui"))]
mod disabled {
    use actix_web::web::ServiceConfig;

    /// A frontend that is not there.
    #[derive(Clone, Copy)]
    pub struct Mount;

    impl Mount {
        pub fn render() -> Self {
            Self
        }

        /// Registers nothing, so every path keeps the answer it had before the
        /// frontend existed.
        pub fn apply(&self, _cfg: &mut ServiceConfig) {}
    }
}
