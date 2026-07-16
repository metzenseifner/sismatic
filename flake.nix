{
  description = ''
    Rust project built with crane

    What the book/gist pipeline consists of
    A general workflow with four jobs — tests (cargo test), formatting (cargo fmt
    --check), linting (cargo clippy -- -D warnings), and code coverage (cargo
    tarpaulin --verbose --workspace) — plus a separate security-audit workflow that
    runs on a daily cron and on any Cargo.toml/Cargo.lock change, executing cargo
    deny check advisories. The test and clippy jobs use Swatinem/rust-cache for
    dependency caching, and the toolchain is "stable" via dtolnay/rust-toolchain.
    Chapter 1 of the book additionally recommends cargo-watch for the inner dev
    loop, and the Docker chapter introduces cargo-chef for dependency layer
    caching.
  '';

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    # Pin the exact Rust toolchain (instead of whatever nixpkgs ships).
    # rust-overlay can read rust-toolchain.toml so cargo-outside-nix matches.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # Pinned RustSec advisory database for cargo-audit.
    # Needed because the Nix build sandbox has no network access.
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      advisory-db,
      ...
    }:
    let
      inherit (nixpkgs) lib;

      # All systems the flake supports. flakeExposed is broad (~10 systems);
      # narrow it if you only care about a few:
      #   systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      systems = lib.systems.flakeExposed;

      # F-map over the system set: instantiate pkgs (with the rust
      # overlay) per system and hand it to f, yielding Record(system).
      fmapSystems =
        f:
        lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
          }
        );

      perSystemOutputs =
        { system, pkgs }:
        let
          # Toolchain pinned by ./rust-toolchain.toml (single source of truth).
          # Alternative without a toolchain file:
          #   p: p.rust-bin.stable."1.87.0".default
          rustToolchain = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Filter the source so only cargo-relevant files affect the hash.
          # Editing README.md etc. won't trigger rebuilds. If your build needs
          # extra files (sql migrations, protobufs, ...), switch to
          # craneLib's fileset helpers and include them explicitly.
          #
          # python/ is kept in addition to the cargo sources: the stub freshness
          # test in src/stub.rs pulls python/sismatic/__init__.pyi in via
          # include_str!, so the file must be present whenever the crate's tests
          # are compiled (clippy, nextest), not only when the wheel is built.
          src = lib.cleanSourceWith {
            src = ./.;
            filter = path: type: (craneLib.filterCargoSources path type) || (lib.hasInfix "/python/" path);
          };

          # Version is shared by every workspace member (workspace.package),
          # so read it once from the root manifest.
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

          # Arguments shared by every crane invocation below.
          commonArgs = {
            inherit src version;
            # Virtual workspace: there is no root package to name the
            # derivations, so give crane a workspace-wide pname. Individual
            # crate builds below override it.
            pname = "sismatic-workspace";
            strictDeps = true;

            # Build-time tools (compilers, codegen, pkg-config) go here.
            # russh -> aws-lc-sys builds AWS-LC from C source. The CLI and web
            # front-ends always enable core's `ssh` feature, and cargo unifies
            # features across the workspace, so every build here (deps, clippy,
            # nextest, the binaries) compiles that C source and needs cmake +
            # perl -- not just the wheel.
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.perl
            ];
            # aws-lc-sys drives its own cmake invocation from build.rs; crane's
            # cmake setup hook would otherwise try (and fail) to configure at
            # the workspace root, which has no CMakeLists.txt.
            dontUseCmakeConfigure = true;

            # Libraries you link against go here.
            buildInputs = [
              # pkgs.openssl
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];
          };

          # Compile *only* the dependencies (keyed on Cargo.lock).
          # This is the expensive layer that gets cached and shared by
          # every check below and across CI runs.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # Arguments shared by the individual workspace-member builds, all on
          # top of the cached dependency layer.
          individualCrateArgs = commonArgs // {
            inherit cargoArtifacts;
            # Tests run in the dedicated nextest check below; don't run them a
            # second time per crate.
            doCheck = false;
          };

          # The CLI front-end binary (`sismatic`).
          cli = craneLib.buildPackage (
            individualCrateArgs
            // {
              pname = "sismatic-cli";
              cargoExtraArgs = "-p sismatic-cli";
              meta.mainProgram = "sismatic";
            }
          );

          # The HTTP server binary (`sismatic-web`).
          web = craneLib.buildPackage (
            individualCrateArgs
            // {
              pname = "sismatic-web";
              cargoExtraArgs = "-p sismatic-web";
              meta.mainProgram = "sismatic-web";
            }
          );

          # Source for the wheel: the cargo sources crane already filters,
          # plus the packaging files maturin reads (pyproject.toml and the
          # readme/license it points at, which cleanCargoSource drops) and the
          # hand-authored Python layer under python/ (py.typed + __init__.pyi
          # stub) that `python-source` in pyproject.toml pulls into the wheel.
          pythonSrc = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (lib.hasInfix "/python/" path)
              || (
                let
                  base = baseNameOf path;
                in
                base == "pyproject.toml" || base == "README.md" || base == "LICENSE"
              );
          };

          # The Python wheel, built by maturin against the same pinned
          # toolchain. Hermetic — it links against nixpkgs' Python and glibc —
          # so it is reproducible but NOT portable to arbitrary machines.
          # For a distributable wheel use the `build-wheel` app below, which
          # links against the host libc. This output is for `nix build`-based
          # dev and reproducibility.
          wheel = pkgs.stdenv.mkDerivation {
            pname = "sismatic-wheel";
            inherit version;
            src = pythonSrc;

            # Vendor the exact locked deps so maturin can build --offline.
            cargoDeps = pkgs.rustPlatform.importCargoLock {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook # unpacks cargoDeps, configures offline
              (rustToolchain pkgs)
              pkgs.maturin
              pkgs.python3
            ]
            # cmake + perl (for aws-lc-sys) come in via commonArgs.
            ++ commonArgs.nativeBuildInputs;

            buildInputs = commonArgs.buildInputs;

            # cmake is only here for aws-lc-sys's own invocation; there is no
            # CMakeLists.txt at the root, so skip nix's cmake configure phase.
            dontUseCmakeConfigure = true;

            buildPhase = ''
              runHook preBuild
              maturin build --offline --release --out dist \
                --features python \
                --interpreter ${pkgs.python3}/bin/python3
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp dist/*.whl $out/
              runHook postInstall
            '';
          };

          # `nix run .#build-wheel [-- <extra maturin args>]`
          #
          # The portable counterpart to the hermetic `wheel` package. It runs
          # maturin against the *host's* python, so the wheel it drops in
          # ./dist is distributable. This is an app rather than a package
          # precisely because it is impure: a sandboxed derivation could only
          # ever link nixpkgs' glibc.
          #
          # On Linux it links through `zig cc` (maturin's --zig) targeting an
          # old glibc, so the wheel is a PyPI-grade manylinux_2_28 build usable
          # far beyond the runner's own glibc — no manylinux container needed.
          # On macOS zig/compatibility don't apply, so it builds natively.
          #
          # Same command locally and in CI. The zig-provided C toolchain,
          # rust toolchain, maturin, cmake (for aws-lc-sys) and perl are all
          # pinned by the flake.
          build-wheel = pkgs.writeShellApplication {
            name = "sismatic-build-wheel";
            runtimeInputs = [
              (rustToolchain pkgs)
              pkgs.maturin
              pkgs.python3
              pkgs.cmake
              pkgs.perl
            ]
            ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.zig ];
            text =
              if pkgs.stdenv.isLinux then
                ''
                  exec maturin build --release --features python --out dist \
                    --zig --compatibility manylinux_2_28 "$@"
                ''
              else
                ''
                  exec maturin build --release --features python --out dist "$@"
                '';
          };

          # `nix run .#build-sdist` — the source distribution for the release.
          build-sdist = pkgs.writeShellApplication {
            name = "sismatic-build-sdist";
            runtimeInputs = [ pkgs.maturin ];
            text = ''
              exec maturin sdist --out dist "$@"
            '';
          };

          # The doc-site toolchain: MkDocs (Material theme) + mkdocstrings'
          # Python handler. Pinned by the flake like every other tool so the
          # site builds identically on a laptop and in CI.
          docsEnv = pkgs.python3.withPackages (ps: [
            ps.mkdocs
            ps.mkdocs-material
            ps.mkdocstrings
            ps.mkdocstrings-python
            # mkdocstrings uses it to pretty-print the rendered signatures.
            ps.black
          ]);

          # `nix run .#docs` builds the site into ./site; `nix run .#docs -- serve`
          # serves it with live reload. mkdocstrings/griffe reads the committed
          # `__init__.pyi` stub statically, so this needs no compiled extension
          # and stays pure and fast — the stub is the single source of truth,
          # regenerated from the Rust catalogs by `gen_stub` and freshness-checked
          # by `cargo test`.
          docs = pkgs.writeShellApplication {
            name = "sismatic-docs";
            runtimeInputs = [ docsEnv ];
            text = ''
              if [ "$#" -eq 0 ]; then
                exec mkdocs build
              else
                exec mkdocs "$@"
              fi
            '';
          };

          # Named binding (not just an output attr) so the devShell can
          # reference it locally instead of going through self.checks —
          # this keeps working even if the checks projection is disabled.
          checks = {
            # The member binaries building at all is itself a check.
            inherit cli web;

            # Clippy as a separate derivation: CI blocks on lints, but
            # downstream consumers can still build the package without
            # being subject to them.
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            # rustdoc must build cleanly (catches broken intra-doc links).
            # `testing` is enabled so the cfg-gated `fake` module (linked from
            # the always-compiled transport/connector module docs) resolves.
            doc = craneLib.cargoDoc (
              commonArgs
              // {
                inherit cargoArtifacts;
                # `--features` is not allowed at a virtual-workspace root, so
                # scope the docs to core, the crate whose module docs link the
                # `testing`-gated `fake` module.
                cargoExtraArgs = "-p sismatic-core --features testing";
                env.RUSTDOCFLAGS = "--deny warnings";
              }
            );

            # `cargo fmt --check`
            fmt = craneLib.cargoFmt {
              inherit src;
            };

            # Keep Cargo.toml & friends formatted, too.
            toml-fmt = craneLib.taploFmt {
              src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
            };

            # Security advisories against the pinned advisory-db input.
            # Update with: nix flake update advisory-db
            audit = craneLib.cargoAudit {
              inherit src advisory-db;
              # RUSTSEC-2023-0071 (rsa Marvin timing side-channel) has no fixed
              # release; russh >=0.60.3 requires rsa 0.10.0-rc, and the bump is
              # needed for RUSTSEC-2026-0154. Re-evaluate when rsa ships a fix.
              cargoAuditExtraArgs = "--ignore yanked --ignore RUSTSEC-2023-0071";
            };

            # License / ban / source policy via cargo-deny.
            # Requires a deny.toml in the repo root (cargo deny init).
            deny = craneLib.cargoDeny {
              inherit src;
            };

            # Test suite via cargo-nextest (better output & parallelism
            # than plain `cargo test`).
            nextest = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                partitions = 1;
                partitionType = "count";
                # Don't fail if a crate has no tests yet.
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
              }
            );
          }
          # Code coverage
          # Tarpaulin only works on Linux, hence the gate.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            coverage = craneLib.cargoTarpaulin (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );
          };
        in
        {
          inherit checks;

          packages = {
            default = cli;
            # `nix build .#cli` / `.#web` -> the front-end binaries.
            inherit cli web;
            # `nix build .#wheel` -> result/sismatic-*.whl
            inherit wheel;
          };

          # Plain app definition (flake-utils.mkApp without flake-utils).
          # lib.getExe resolves the binary; set meta.mainProgram on the
          # package if the binary name differs from the crate name.
          apps = {
            default = {
              type = "app";
              program = pkgs.lib.getExe cli;
            };
            # `nix run .#web` starts the HTTP server.
            web = {
              type = "app";
              program = pkgs.lib.getExe web;
            };
            # Pipeline steps, callable identically here and in CI:
            #   nix run .#build-wheel
            #   nix run .#build-sdist
            build-wheel = {
              type = "app";
              program = pkgs.lib.getExe build-wheel;
            };
            build-sdist = {
              type = "app";
              program = pkgs.lib.getExe build-sdist;
            };
            # `nix run .#docs [-- serve]` builds/serves the API doc site.
            docs = {
              type = "app";
              program = pkgs.lib.getExe docs;
            };
          };

          # `nix develop`: inherits every dependency the checks need,
          # plus the pinned toolchain (cargo, rustc, clippy, rustfmt).
          #
          # Fast linking:
          # - Linux x86_64: nothing to configure. rustc >= 1.90 links with
          #   its bundled rust-lld by default; the book's clang+lld
          #   .cargo/config.toml dance predates this. (aarch64-linux still
          #   uses GNU ld; add the same flags as darwin below if needed.)
          # - macOS: nix's cctools ld64 is the slow classic linker, so we
          #   provide LLVM's lld and tell cargo to link through it.
          devShells.default = craneLib.devShell (
            {
              inherit checks;

              packages = [
                #------------------------------------------------------------------------------#
                #                            Common-OS Derivations                             #
                #------------------------------------------------------------------------------#
                pkgs.cargo-nextest
                pkgs.cargo-deny
                # inner development loop
                pkgs.cargo-watch # or pkgs.bacon (maintained successor)
                # `cargo expand` needs a nightly rustc for --pretty=expanded;
                # the pinned stable toolchain stays the default, nightly is
                # only picked up by cargo-expand via the +nightly proxy.
                pkgs.cargo-expand
                (pkgs.rust-bin.selectLatestNightlyWith (t: t.minimal))
                # Python packaging: build wheels locally with `maturin build`.
                # cmake/perl are needed by aws-lc-sys (pulled in via the ssh
                # feature) whenever the `python` feature is compiled.
                pkgs.maturin
                pkgs.python3
                pkgs.cmake
                pkgs.perl
                # Doc site: `mkdocs serve` / `mkdocs build` (same toolchain the
                # `nix run .#docs` app uses).
                docsEnv
                # zero2prod chapter 3+: database tooling
                # pkgs.sqlx-cli
                # pkgs.postgresql
                # pkgs.rust-analyzer

                #------------------------------------------------------------------------------#
                #              Add any lifecycle derivations (scripts) here that               #
                #              control integrated entities or dependencies to pin              #
                #                           those in the flake.lock                            #
                #              e.g. pkgs.postgresql, pkgs.sqlx-cli, init-db, pg-stop           #
                #------------------------------------------------------------------------------#
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
                #------------------------------------------------------------------------------#
                #                          macOS-specific derivations                          #
                #------------------------------------------------------------------------------#
                pkgs.llvmPackages.bintools # provides ld64.lld
              ];
              shellHook = ''
                cat <<EOF
                  Day-0 Steps (Ensure the following)

                  #  Cargo.lock committed
                  cargo generate-lockfile; git add Cargo.lock
                  # deny.toml committed; else cargo deny init, then commit

                  cargo deny init; git add deny.toml
                  .gitignore contains result and result-* as Nix will output build artifacts there.


                  nix flake check --all-systems # check aforementioned  (or e.g. check for deny.toml by running nix build .#checks.<sys>.deny)
                EOF
              '';
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
              # Only set in the dev shell: the hermetic crane builds are
              # deliberately left on their default linker so derivation
              # hashes stay independent of dev-loop tuning.
              CARGO_BUILD_RUSTFLAGS = "-C link-arg=-fuse-ld=lld";
            }
          );
        };

      # Compute each system's outputs ONCE, then project each field out.
      # This is the System↔Output transpose: perSystem is keyed by system,
      # the flake schema wants each field keyed by system.
      perSystem = fmapSystems perSystemOutputs;
      project = field: builtins.mapAttrs (_: out: out.${field}) perSystem;
    in
    {
      # Projections over Record(system)
      packages = project "packages";
      apps = project "apps";
      devShells = project "devShells";
      checks = project "checks";
    };
}
