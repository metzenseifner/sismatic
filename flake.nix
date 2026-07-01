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

    # Divergences in this flake
    - Test runner. The gist uses plain cargo test; you have nextest.
      Functionally equivalent for the book's purposes and generally considered an
      upgrade, but if you want a literal match, swap cargoNextest for
      craneLib.cargoTest. I'd keep nextest.
    - The big one: the book's integration tests won't run in the Nix sandbox
      as-is. This doesn't bite in chapter 1, but from chapter 3 onward,
      zero2prod's tests spin up the app and talk to a live Postgres (launched via
      scripts/init_db.sh in Docker). The Nix build sandbox has no network and no
      running services, so the moment you write those tests, your nextest check
      will start failing — not because the code is wrong, but because the
      database isn't there. You have three realistic options: run integration
      tests outside Nix in the devShell (book-style, simplest — keep the Nix
      check limited to unit tests via nextest filter expressions); start an
      ephemeral Postgres inside the check derivation (pkgs.postgresql, initdb +
      pg_ctl against a Unix socket in a preCheck hook — fully hermetic, very
      Nix-idiomatic, some setup work); or a NixOS VM test for the full
      integration suite (the heavyweight, most rigorous option). Worth deciding
      before you hit chapter 3 rather than when CI suddenly goes red.
    - Note the coverage check, like tarpaulin itself, is Linux-only — on
      Darwin systems it simply doesn't appear in checks, which is the correct
      behavior (the book's coverage job likewise runs only on a Linux runner). If
      you'd rather have coverage on macOS too, craneLib.cargoLlvmCov is the
      cross-platform alternative.

    ## Extra Notes from AI
    - The dependency-caching match is almost poetic. cargo-chef — the tool the
      book's Docker chapter uses to build dependencies as a separate cached layer
      — was written by Palmieri for the book. Crane's buildDepsOnly is exactly
      that idea, natively in Nix. You're not approximating his recommendation;
      you're using the Nix-native implementation of it. Relatedly, your
      derivation-level caching subsumes what rust-cache does in his Actions jobs.
    - The audit freshness model differs in a way worth acting on. His audit
      runs on a daily cron precisely so newly published advisories flag existing
      code. Your advisory-db is a pinned input — it only knows about advisories
      as of your last nix flake update advisory-db. To match the book's intent,
      add a small scheduled CI job that updates that one input and runs the audit
      check (and ideally opens a PR with the lockfile bump). Without it, your
      audit check is technically present but silently goes stale.
    - Coverage is the one genuinely missing check. Crane has a helper for
      exactly this. Let me add it, plus the chapter-1 dev-loop tooling:
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
          src = craneLib.cleanCargoSource ./.;

          # Arguments shared by every crane invocation below.
          commonArgs = {
            inherit src;
            strictDeps = true;

            # Build-time tools (compilers, codegen, pkg-config) go here.
            nativeBuildInputs = [
              # pkgs.pkg-config
            ];

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

          # The crate itself, built on top of the cached dependency layer.
          my-crate = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              # Tests run in the dedicated nextest check below;
              # don't run them a second time here.
              doCheck = false;
            }
          );

          # Source for the wheel: the cargo sources crane already filters,
          # plus the packaging files maturin reads (pyproject.toml and the
          # readme/license it points at, which cleanCargoSource drops).
          pythonSrc = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
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
            pname = "opensis-wheel";
            inherit (my-crate) version;
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
              # russh -> aws-lc-sys builds AWS-LC from C source.
              pkgs.cmake
              pkgs.perl
            ]
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
            name = "opensis-build-wheel";
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
            name = "opensis-build-sdist";
            runtimeInputs = [ pkgs.maturin ];
            text = ''
              exec maturin sdist --out dist "$@"
            '';
          };

          # Named binding (not just an output attr) so the devShell can
          # reference it locally instead of going through self.checks —
          # this keeps working even if the checks projection is disabled.
          checks = {
            # The package building at all is itself a check.
            inherit my-crate;

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
                cargoExtraArgs = "--features testing";
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
          # Code coverage (zero2prod's `cargo tarpaulin` job).
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
            default = my-crate;
            # `nix build .#wheel` -> result/opensis-*.whl
            inherit wheel;
          };

          # Plain app definition (flake-utils.mkApp without flake-utils).
          # lib.getExe resolves the binary; set meta.mainProgram on the
          # package if the binary name differs from the crate name.
          apps = {
            default = {
              type = "app";
              program = pkgs.lib.getExe my-crate;
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
          };

          # `nix develop`: inherits every dependency the checks need,
          # plus the pinned toolchain (cargo, rustc, clippy, rustfmt).
          #
          # Fast linking (zero2prod ch1):
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
                # zero2prod chapter 1: inner development loop
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
