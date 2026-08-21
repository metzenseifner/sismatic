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
          #
          # tests/fixtures/ likewise: an integration test that reads a config
          # file off disk (sismatic-server) needs the file itself in the build
          # sandbox, and it is a .yaml/.toml that filterCargoSources drops.
          #
          # server_configuration.yaml is the shipped config rather than a
          # fixture, and it is kept for the same reason: `deny_unknown_fields`
          # makes "does the config we ship still load?" a real question — a
          # section present in the YAML and absent from `RawServerConfig` is a
          # startup failure — and the unit test that answers it has to be able
          # to read the file.
          src = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (lib.hasInfix "/python/" path)
              || (lib.hasInfix "/tests/fixtures/" path)
              || (lib.hasSuffix "/server_configuration.yaml" path);
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
            # russh -> aws-lc-sys builds AWS-LC from C source. The CLI
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

          # The composition-root binary (`sismatic-server`) — the deployable
          # artifact, wired up from core, the HTTP API, the store and sync.
          #
          # Unlike cli this one gets a dependency layer of its own,
          # scoped exactly the way the build is scoped. The shared
          # `cargoArtifacts` is resolved workspace-wide, and for
          # `utoipa-swagger-ui` that lands on a different feature variant than
          # `-p sismatic-server` does: the shared layer carries only an .rmeta
          # for the variant this build wants, so cargo recompiles the crate
          # while reusing the *cached* output of its build script. That script
          # writes an embed.rs holding an absolute path to the Swagger bundle
          # it unpacked, which no longer exists in this sandbox, so rust-embed
          # silently emits a `SwaggerUiDist` with no `Embed` impl and the build
          # dies on "no function or associated item named `get`". Scoping the
          # layer means the .rlib is already present and nothing recompiles.
          #
          # This only ever bites on macOS: Linux builds are sandboxed into a
          # build dir that is always /build, so the stale absolute path happens
          # to resolve and the bug stays invisible.
          serverDeps = craneLib.buildDepsOnly (
            commonArgs
            // {
              pname = "sismatic-server-deps";
              cargoExtraArgs = "-p sismatic-server";
            }
          );

          server = craneLib.buildPackage (
            commonArgs
            // {
              pname = "sismatic-server";
              cargoArtifacts = serverDeps;
              cargoExtraArgs = "-p sismatic-server";
              # Tests run in the dedicated nextest check.
              doCheck = false;
              meta.mainProgram = "sismatic-server";
            }
          );

          #------------------------------------------------------------------#
          #                 Publishable sismatic-server binaries              #
          #------------------------------------------------------------------#
          # `server` above is hermetic but NOT portable, and in two different
          # ways: on Linux its ELF interpreter is an absolute /nix/store glibc
          # path, on macOS it carries a /nix/store load command for libiconv.
          # Either way the binary only runs on a machine that has this exact
          # store, which is useless for something downloaded off a GitHub
          # release page. The outputs below fix that per platform.
          #
          # Both are built natively — CI runs one runner per architecture — so
          # nothing here is a cross-compile. That matters for aws-lc-sys
          # (pulled in by russh via core's `ssh` feature), which drives its own
          # cmake build of vendored C: it stays on the well-trodden native path
          # and only sees a different libc, not a different machine.

          # Rust target triple of the published artifact, and the name the
          # release tarball is keyed by.
          releaseTarget =
            let
              arch = pkgs.stdenv.hostPlatform.parsed.cpu.name; # x86_64 | aarch64
            in
            if pkgs.stdenv.isDarwin then "${arch}-apple-darwin" else "${arch}-unknown-linux-musl";

          # Linux: statically linked against musl. `crt-static` leaves no ELF
          # interpreter and no runtime libc at all, so the artifact runs on any
          # kernel of the same architecture — distro, glibc version and
          # /nix/store all stop mattering.
          serverStatic =
            let
              # Same channel and components as rust-toolchain.toml (still the
              # single source of truth), plus the musl std for this arch.
              muslToolchain = p: (rustToolchain p).override { targets = [ releaseTarget ]; };
              craneLibMusl = (crane.mkLib pkgs).overrideToolchain muslToolchain;

              # Rust ships its own musl libc.a for the pure-Rust half of the
              # link; this C toolchain is only for aws-lc-sys' vendored C.
              cc = pkgs.pkgsStatic.stdenv.cc;
              ccBin = "${cc}/bin/${cc.targetPrefix}cc";

              # cargo spells per-target variables with the triple upper-cased
              # and dashes turned into underscores; the `cc` crate (and through
              # it aws-lc-sys' cmake invocation) spells them verbatim.
              screamingTarget = lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] releaseTarget);

              muslArgs = commonArgs // {
                pname = "sismatic-server-static";
                # Scoped on the deps layer too, for the same reason `server`
                # above needs its own — see the comment there.
                cargoExtraArgs = "-p sismatic-server";

                CARGO_BUILD_TARGET = releaseTarget;
                CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
                "CARGO_TARGET_${screamingTarget}_LINKER" = ccBin;
                "CC_${releaseTarget}" = ccBin;
                "CXX_${releaseTarget}" = "${cc}/bin/${cc.targetPrefix}c++";
                "AR_${releaseTarget}" = "${cc.bintools}/bin/${cc.targetPrefix}ar";
                # Build scripts and proc macros run on the build machine, so
                # they keep the native compiler.
                HOST_CC = "${pkgs.stdenv.cc.nativePrefix}cc";

                depsBuildBuild = [ cc ];
              };
            in
            craneLibMusl.buildPackage (
              muslArgs
              // {
                # A different target means a different dependency layer: the
                # glibc `cargoArtifacts` above cannot be reused here.
                cargoArtifacts = craneLibMusl.buildDepsOnly muslArgs;
                doCheck = false;
                meta.mainProgram = "sismatic-server";
              }
            );

          # macOS: the only non-system library the binary picks up is nixpkgs'
          # libiconv, and macOS ships its own copy at the stock path, so
          # repoint the load command there. Rewriting a Mach-O invalidates its
          # signature — fatal on arm64, where the kernel refuses to exec an
          # unsigned binary — so re-sign ad-hoc afterwards.
          serverPortableDarwin =
            pkgs.runCommand "sismatic-server-portable-${version}"
              {
                nativeBuildInputs = [
                  pkgs.cctools
                  pkgs.darwin.sigtool
                ];
                meta.mainProgram = "sismatic-server";
              }
              ''
                mkdir -p $out/bin
                cp ${server}/bin/sismatic-server $out/bin/
                chmod u+w $out/bin/sismatic-server

                # `otool -L` leads with the binary's own path, which is a
                # /nix/store path by construction; the load commands are the
                # indented lines after it, so every read below skips line 1.
                loadCommands() {
                  otool -L $out/bin/sismatic-server | tail -n +2
                }

                for dep in $(loadCommands | awk '/\/nix\/store\// { print $1 }'); do
                  install_name_tool -change "$dep" "/usr/lib/$(basename "$dep")" \
                    $out/bin/sismatic-server
                done
                codesign --force --sign - $out/bin/sismatic-server

                # Fail the build rather than ship something that only runs on
                # a machine with this store.
                if loadCommands | grep -q /nix/store/; then
                  echo "error: binary still links against /nix/store:" >&2
                  loadCommands >&2
                  exit 1
                fi
              '';

          serverPortable = if pkgs.stdenv.isDarwin then serverPortableDarwin else serverStatic;

          # The published artifact. Naming, contents and checksum live here so
          # the workflow stays a dispatcher — `nix build .#server-release`
          # produces byte-identical output on a laptop and on a runner.
          serverRelease = pkgs.runCommand "sismatic-server-release-${version}-${releaseTarget}" { } ''
            name="sismatic-server-${version}-${releaseTarget}"
            mkdir -p "$name" "$out"
            cp ${serverPortable}/bin/sismatic-server "$name/"
            cp ${./LICENSE} "$name/LICENSE"
            cp ${./README.md} "$name/README.md"
            chmod -R u+w "$name"

            # Reproducible archive: sorted entries, fixed mtime, no owner
            # names, and gzip -n so the header carries no timestamp either.
            tar --sort=name --mtime='@1' --owner=0 --group=0 --numeric-owner \
              -cf - "$name" | gzip -9n > "$out/$name.tar.gz"
            ( cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )
          '';

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
            # `serverRelease` is deliberately not here: it would drag the musl
            # toolchain into every `nix flake check`, and the release matrix in
            # CI builds it on each architecture anyway.
            inherit cli server;

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

            # Guardrail for the release-plz footgun that wedged main when
            # sismatic-intent-relay was added (Actions run 32251398251).
            #
            # Mechanism, measured rather than inferred. Under `git_only`,
            # release-plz resolves each member's last release by matching tags
            # against that member's `git_tag_name` template, checks the match out
            # in a temp worktree, runs `cargo package --workspace` there, and
            # looks the member up in the result (release_plz_core/src/next_ver.rs,
            # `get_cargo_package`). Our workspace template `v{{ version }}` carries
            # no `{{ package }}`, so *every* member matches `v0.2.25` — including
            # crates that did not exist at that tag. The lookup then fails hard:
            #
            #     Failed to find package "sismatic-intent-relay"
            #
            # and no release PR opens again, ever, because the tag can only move
            # via the release PR that is now failing.
            #
            # `release = false` does not help, and the older version of this check
            # was built on the belief that it did. It exempts a member from version
            # determination — only sismatic-core and sismatic-python-sdk ever reach
            # `determining next version` — but NOT from the packaging pass that
            # crashes. sismatic-intent-relay had an explicit entry with
            # `release = false` and wedged main anyway.
            #
            # The fix is to make the tag match impossible for members that have
            # never been released: scope their template with `{{ package }}`. Since
            # `git_tag_enable = false` for all of them, no such tag is ever created,
            # so release-plz takes its "treated as initial release" branch and never
            # opens a worktree. Measured: identical version and changelog output to
            # the unscoped config, in ~2 min rather than ~18.
            #
            # The converse matters just as much. Scoping a member that *is* released
            # strips the baseline its changelog is diffed against, and the next
            # release PR regenerates that changelog from the beginning of history.
            # Measured on sismatic-python-sdk: its proposed 0.2.26 entry came back
            # containing "release v0.2.25", "release v0.2.24", and "rename
            # sismatic-python to sismatic-python-sdk".
            #
            # So the invariant is a biconditional, not an implication:
            #
            #     release = false  <=>  git_tag_name contains {{ package }}
            #
            # Both halves are pure functions of two files in this tree, which is
            # why this can live in the flake sandbox at all. The historical form of
            # the invariant — "every member exists at the last tag" — cannot: the
            # sandbox has no .git, no tags and no network.
            release-plz-config =
              let
                cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
                releasePlz = builtins.fromTOML (builtins.readFile ./release-plz.toml);

                # Dir basename != package name necessarily, so read each member's
                # own manifest for the real name.
                memberNames = map (
                  m: (builtins.fromTOML (builtins.readFile (./. + "/${m}/Cargo.toml"))).package.name
                ) cargoToml.workspace.members;

                entries = releasePlz.package or [ ];
                entryNames = map (p: p.name) entries;
                missing = lib.subtractLists entryNames memberNames; # members with no entry
                stale = lib.subtractLists memberNames entryNames; # entries with no member

                # A member's effective template is its own override, else the
                # workspace default.
                wsTagName = releasePlz.workspace.git_tag_name or null;
                tagTemplate = p: p.git_tag_name or wsTagName;
                # Accept any spelling: {{package}}, {{ package }}, {{  package  }}.
                isScoped =
                  p:
                  let
                    t = tagTemplate p;
                  in
                  t != null && lib.hasInfix "{{package}}" (builtins.replaceStrings [ " " ] [ "" ] t);
                isReleased = p: (p.release or true) == true;

                # Half one: an unreleased member with an unscoped template is the
                # intent-relay bug, armed and waiting for the next new crate.
                unscopedUnreleased = map (p: p.name) (
                  builtins.filter (p: !(isReleased p) && !(isScoped p)) entries
                );
                # Half two: a released member with a scoped template loses its
                # changelog baseline.
                scopedReleased = map (p: p.name) (builtins.filter (p: isReleased p && isScoped p) entries);

                # Promoting a crate to released is the one case this check cannot
                # settle statically: it would have to prove the crate exists at the
                # last tag, which needs history the sandbox does not have. Pin the
                # set so that decision is made by a human editing this list, who can
                # then check the tag by hand.
                expectedReleased = [
                  "sismatic-core"
                  "sismatic-python-sdk"
                  "sismatic-http-api"
                ];
                actualReleased = map (p: p.name) (builtins.filter isReleased entries);
                newlyReleased = lib.subtractLists expectedReleased actualReleased;
                noLongerReleased = lib.subtractLists actualReleased expectedReleased;
              in
              assert lib.assertMsg (missing == [ ] && stale == [ ]) ''
                release-plz.toml is out of sync with the workspace members.
                  members missing a [[package]] entry: ${lib.concatStringsSep ", " missing}
                  stale [[package]] entries (no such member): ${lib.concatStringsSep ", " stale}
                Every workspace member needs an explicit entry.
              '';
              assert lib.assertMsg (unscopedUnreleased == [ ]) ''
                release-plz.toml: unreleased member(s) with a workspace-shaped git_tag_name:
                  ${lib.concatStringsSep ", " unscopedUnreleased}
                These carry `release = false`, so release-plz never releases them —
                but the workspace template `${toString wsTagName}` still matches the
                last `v*` tag for them. The moment such a crate is added after a tag,
                release-plz checks out that tag, cannot find the crate there, and the
                release-pr job dies with `Failed to find package "<crate>"` — with no
                way to recover, since the tag only moves via the release PR.
                Give each one a scoped template so no tag can ever match it:
                  git_tag_name = "{{ package }}-v{{ version }}"
              '';
              assert lib.assertMsg (scopedReleased == [ ]) ''
                release-plz.toml: released member(s) with a scoped git_tag_name:
                  ${lib.concatStringsSep ", " scopedReleased}
                A scoped template matches no tag, so release-plz treats these as an
                initial release and regenerates their CHANGELOG.md from the whole
                git history instead of from the last release. Released members must
                keep the workspace-shaped template so they retain a diff baseline.
                Drop the git_tag_name override from these entries.
              '';
              assert lib.assertMsg (newlyReleased == [ ] && noLongerReleased == [ ]) ''
                release-plz.toml: the set of released members changed.
                  newly released: ${lib.concatStringsSep ", " newlyReleased}
                  no longer released: ${lib.concatStringsSep ", " noLongerReleased}
                A released member must already exist at the latest `v*` tag, which
                this check cannot verify (the flake sandbox has no tag history).
                Confirm by hand:
                  git ls-tree --name-only "$(git describe --tags --abbrev=0)" crates/
                If the crate is NOT there, ship a release containing it first (as
                release = false + scoped), then promote it in a follow-up PR.
                Then update expectedReleased in flake.nix to match.
              '';
              pkgs.runCommand "release-plz-config-ok" { } "touch $out";

            # Guardrail for the release-blocking regression in 0d16892: internal
            # path dependencies were reduced to `path`-only, and every release-plz
            # run since died in `cargo package`, which refuses a path dependency
            # with no version requirement. release-plz packages the *whole*
            # workspace under git_only (`cargo package --allow-dirty --workspace`),
            # so `release = false` exempts nothing and one bare `path` is enough to
            # stop the release job — on whichever member happens to be packaged
            # first, far from the line that caused it. The requirement is static,
            # so assert it statically.
            #
            # The assertion is "the workspace version satisfies the requirement",
            # not the older "the pin equals the workspace version exactly". Exact
            # pins are what [workspace.dependencies] deliberately stopped using:
            # release-plz only rewrites the pins of packages it releases, so the
            # rest rotted (api-types sat at 0.2.19 against a 0.2.20 workspace) and
            # every release needed a hand-fix. A requirement like `0` covers every
            # 0.x and never needs maintaining. What this catches is therefore the
            # regression that actually breaks releases — a missing or unsatisfiable
            # requirement — not pin freshness, which the `0` form makes moot. (One
            # consequence worth naming: an exact `0.2.24` against a 0.2.25
            # workspace still satisfies ^0.2.24, so re-introducing exact pins buys
            # back the old rot, invisible here until the 0.3.0 boundary.)
            internal-dep-versions =
              let
                cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
                inherit (cargoToml.workspace.package) version;

                # [ major minor patch ]. A prerelease suffix is dropped rather
                # than ordered — this workspace has never cut one, and getting it
                # wrong should not be a silent pass.
                triple = v: map lib.toInt (lib.splitString "." (lib.head (lib.splitString "-" v)));
                workspace = triple version;

                # The components a requirement spells out: "^0.2" -> [ 0 2 ].
                # Only cargo's default (caret) form is understood; a range like
                # ">=0.2, <0.4" needs a real semver parser, so it is reported as
                # unsupported instead of being waved through.
                reqParts =
                  req:
                  if builtins.match "\\^?[0-9]+(\\.[0-9]+)?(\\.[0-9]+)?" req == null then
                    null
                  else
                    triple (lib.removePrefix "^" req);

                # Caret semantics: the range runs up to the next increment of the
                # leftmost component the requirement spells out that is nonzero.
                # ^0 := <1.0.0, ^0.2 := <0.3.0, ^0.0.3 := <0.0.4, ^1.2 := <2.0.0.
                upper =
                  parts:
                  let
                    c = i: lib.elemAt parts i;
                    n = lib.length parts;
                  in
                  if c 0 != 0 then
                    [
                      (c 0 + 1)
                      0
                      0
                    ]
                  else if n == 1 then
                    [
                      1
                      0
                      0
                    ]
                  else if c 1 != 0 then
                    [
                      0
                      (c 1 + 1)
                      0
                    ]
                  else if n == 2 then
                    [
                      0
                      1
                      0
                    ]
                  else
                    [
                      0
                      0
                      (c 2 + 1)
                    ];

                # a >= b, on [ major minor patch ].
                atLeast =
                  a: b:
                  if lib.elemAt a 0 != lib.elemAt b 0 then
                    lib.elemAt a 0 > lib.elemAt b 0
                  else if lib.elemAt a 1 != lib.elemAt b 1 then
                    lib.elemAt a 1 > lib.elemAt b 1
                  else
                    lib.elemAt a 2 >= lib.elemAt b 2;

                satisfies =
                  parts:
                  let
                    lower = parts ++ lib.genList (_: 0) (3 - lib.length parts);
                  in
                  atLeast workspace lower && !(atLeast workspace (upper parts));

                # Every table that can name an internal crate: a member's three
                # dependency tables plus any target-scoped variants of them.
                depTables =
                  manifest:
                  let
                    direct = m: [
                      (m.dependencies or { })
                      (m.dev-dependencies or { })
                      (m.build-dependencies or { })
                    ];
                  in
                  direct manifest ++ lib.concatMap direct (lib.attrValues (manifest.target or { }));

                # A dependency is internal iff it is declared with a `path`.
                # `foo.workspace = true` has none: it re-uses the root's entry,
                # which this same check covers at the root.
                depsIn =
                  file: table:
                  lib.mapAttrsToList (name: dep: {
                    inherit file name;
                    req = dep.version or null;
                  }) (lib.filterAttrs (_: dep: builtins.isAttrs dep && dep ? path) table);

                deps =
                  depsIn "Cargo.toml" (cargoToml.workspace.dependencies or { })
                  ++ lib.concatMap (
                    m:
                    lib.concatMap (depsIn "${m}/Cargo.toml") (
                      depTables (builtins.fromTOML (builtins.readFile (./. + "/${m}/Cargo.toml")))
                    )
                  ) cargoToml.workspace.members;

                # `*` imposes no bound at all, so the workspace version satisfies
                # it by construction; cargo packages it happily.
                ok = d: d.req == "*" || (d.req != null && reqParts d.req != null && satisfies (reqParts d.req));

                offenders = lib.filter (d: !(ok d)) deps;
                describe = d: "  ${d.file}: ${d.name} -> ${if d.req == null then "(no version field)" else d.req}";
              in
              assert lib.assertMsg (offenders == [ ]) ''
                Internal path dependencies without a usable version requirement
                (workspace version ${version}):
                ${lib.concatStringsSep "\n" (map describe offenders)}
                Every path dependency on a workspace member needs a `version` that
                the workspace version satisfies, or release-plz's `cargo package
                --workspace` fails and no release can be cut. Use `version = "0"`,
                which covers every 0.x and never needs updating — see the note in
                [workspace.dependencies]. Requirements other than the caret form
                (`0`, `0.2`, `^0.2.3`) or `*` are not understood by this check.
              '';
              pkgs.runCommand "internal-dep-versions-ok" { } "touch $out";

            # Guardrail for the publish/dependency incoherence that blocked every
            # release from 0.2.21 on and produced five consecutive "fix:" commits.
            # release-plz's git_only mode runs `cargo package` over every member to
            # diff it against the last tag. When a packaged crate has a path
            # dependency carrying a `version` — which internal-version-pins above
            # *requires* — cargo rewrites it into a registry dependency and resolves
            # it, substituting the locally packaged sibling only if that sibling is
            # itself publishable. A `publish = false` sibling is never substituted,
            # so cargo goes to crates.io, finds no sismatic crate there (none has
            # ever been published), and the release-pr job errors with "no matching
            # package named ... found". Flipping `publish` crate by crate only moves
            # which member trips first — locally it was sismatic-store ->
            # sismatic-api-types, in CI sismatic-server -> sismatic-http-api — which
            # is exactly why the fix attempts kept recurring. So assert the
            # invariant itself, statically: a member that another member depends on
            # must stay publishable. This is *not* a decision to publish; the single
            # gate on that is release-plz.toml's workspace-wide `publish = false`.
            # internal-deps-publishable =
            #   let
            #     cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
            #
            #     # package name -> { dir; manifest }. Dir basename != package name
            #     members = lib.listToAttrs (
            #       map (
            #         dir:
            #         let
            #           manifest = builtins.fromTOML (builtins.readFile (./. + "/${dir}/Cargo.toml"));
            #         in
            #         lib.nameValuePair manifest.package.name { inherit dir manifest; }
            #       ) cargoToml.workspace.members
            #     );
            #
            #     depTables =
            #       manifest:
            #       let
            #         direct = m: [
            #           (m.dependencies or { })
            #           (m.dev-dependencies or { })
            #           (m.build-dependencies or { })
            #         ];
            #       in
            #       direct manifest ++ lib.concatMap direct (lib.attrValues (manifest.target or { }));
            #
            #     # Internal iff declared with a `path`, matching internal-version-pins.
            #     pathDeps = table: lib.attrNames (lib.filterAttrs (_: d: builtins.isAttrs d && d ? path) table);
            #
            #     # Every member named as a path dependency, by the workspace root
            #     # (which `foo.workspace = true` re-uses) or by another member.
            #     depended = lib.unique (
            #       pathDeps (cargoToml.workspace.dependencies or { })
            #       ++ lib.concatMap (e: lib.concatMap pathDeps (depTables e.manifest)) (lib.attrValues members)
            #     );
            #
            #     # `publish` is absent (publishable), a bool, or a registry list.
            #     offenders = lib.filter (name: (members.${name}.manifest.package.publish or true) == false) (
            #       lib.filter (name: members ? ${name}) depended
            #     );
            #   in
            #   assert lib.assertMsg (offenders == [ ]) ''
            #     These workspace members are depended on by another member but are
            #     marked `publish = false`:
            #     ${lib.concatStringsSep "\n" (map (n: "  ${members.${n}.dir}/Cargo.toml: ${n}") offenders)}
            #     Under release-plz's git_only mode `cargo package` cannot resolve a
            #     versioned path dependency on an unpublishable crate: it falls back to
            #     crates.io, where no sismatic crate exists, and the release fails with
            #     "no matching package named <crate> found".
            #     Drop `publish = false` from the crates listed above. Publishing stays
            #     disabled workspace-wide in release-plz.toml — that file, not this
            #     field, is what keeps these crates off crates.io. Keep `publish =
            #     false` only on leaf members nothing else depends on (the binaries and
            #     the pyo3 cdylib).
            #   '';
            #   pkgs.runCommand "internal-deps-publishable-ok" { } "touch $out";

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
            # `nix build .#cli` / `.#server` -> the binaries.
            inherit cli server;
            # `nix build .#wheel` -> result/sismatic-*.whl
            inherit wheel;

            # The dependency layers on their own. Nothing consumes these
            # directly — they exist so one machine (locally, or one CI job) can
            # compile the expensive Cargo.lock-keyed layers once and populate a
            # store the checks then hit instead of rebuilding. `server-deps` is
            # separate because sismatic-server needs its own scoped layer; see
            # the comment on `serverDeps`.
            deps = cargoArtifacts;
            server-deps = serverDeps;

            # `nix build .#server-portable` -> a sismatic-server that runs on
            # a machine with no Nix; `.#server-release` -> that binary packed
            # as sismatic-server-<version>-<target>.tar.gz plus its SHA-256.
            server-portable = serverPortable;
            server-release = serverRelease;
          };

          # Plain app definition (flake-utils.mkApp without flake-utils).
          # lib.getExe resolves the binary; set meta.mainProgram on the
          # package if the binary name differs from the crate name.
          apps = {
            default = {
              type = "app";
              program = pkgs.lib.getExe cli;
            };
            # `nix run .#server` starts the composition root.
            server = {
              type = "app";
              program = pkgs.lib.getExe server;
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

                # Single source of truth for the Python env: nix develop AND
                # direnv's `use flake` run this hook (print-dev-env ends with
                # `eval "$shellHook"`), so managing the venv here -- rather than
                # sourcing one by hand after the shell is up -- keeps its bin dir
                # inside direnv's captured diff and stops the next prompt from
                # stripping the flake's /nix/store paths. Anchored to the repo
                # root so it lands in the same place regardless of cwd.
                SIS_VENV="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")/sis-venv"
                [ -d "$SIS_VENV" ] || python3 -m venv "$SIS_VENV"
                export VIRTUAL_ENV="$SIS_VENV"
                export PATH="$SIS_VENV/bin:$PATH"
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
