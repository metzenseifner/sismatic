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
    # Update upstream rust toolchain availability with: nix flake update rust-overlay
    # The actual toolchain version is specified in the rust-toolchain.toml
    # nix build --no-link --print-out-paths 'github:oxalica/rust-overlay'
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

      #------------------------------------------------------------------#
      #                Where the container image is published             #
      #------------------------------------------------------------------#
      # System-independent, so these live outside perSystemOutputs and are
      # exported under legacyPackages.ci. Adding, removing
      # or renaming a registry is an edit here and nowhere else: the
      # workflow names no registry, and every app below is generated.

      # The repository name under every registry.
      containerName = "sismatic";

      # The architectures the multi-arch index is assembled from. One native
      # runner each, for the same reason `serverStatic` is built natively:
      # the vendored C in aws-lc-sys stays on its well-trodden path and only
      # ever sees a different libc, not a different machine.
      containerArches = [
        "amd64"
        "arm64"
      ];

      # Keyed by name: the name becomes the `publish-<name>` app and the
      # "publish → <name>" job. Each entry says where its credentials are
      # found, never what they are:
      #   usernameVar  a repository *variable*; usernames are not secret,
      #                and a secret would mask them in every log line
      #   tokenSecret  a repository secret
      # Run locally, `publish-all` reads environment variables with these
      # same names, so one spelling serves the workflow and a laptop.
      #
      # Do not name a registry `all`: its app would collide with
      # `publish-all`.
      containerRegistries = {
        ghcr = {
          host = "ghcr.io";
          namespace = "metzenseifner";
          usernameVar = "GHCR_USERNAME";
          # GitHub's own token, so a fork needs no secret of its own for
          # this one, only `packages: write` on the job.
          tokenSecret = "GITHUB_TOKEN";
        };
        quay = {
          host = "quay.io";
          namespace = "metzenseifner";
          usernameVar = "QUAY_USERNAME";
          tokenSecret = "QUAY_TOKEN";
        };
        dockerhub = {
          host = "docker.io";
          namespace = "metzenseifner";
          usernameVar = "DOCKERHUB_USERNAME";
          tokenSecret = "DOCKERHUB_TOKEN";
        };
      };

      # The registry the generated Kubernetes manifests pull from. Named
      # rather than "the first one", because an attribute set has no order
      # of its own.
      containerPrimaryRegistry = "ghcr";

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
          #
          # container_configuration.yaml and container_devices.yaml are the two
          # files the container image carries, kept on exactly that argument and
          # with more riding on it: they are the config of an artifact nobody
          # edits by hand, so the only thing standing between a renamed section
          # and a crash-looping pod is `shipped_config_check` reading them here.
          # They live in the crate rather than being generated in the image
          # derivation *so that* a test can reach them.
          #
          # The frontend under `crates/sismatic-http-api/http-ui/` is NOT kept,
          # and that is the point. `filterCargoSources` drops `.ts`/`.tsx`/
          # `.css`/`.json`/`.html`, so a CSS edit changes none of the inputs to
          # `cargoArtifacts` and cannot invalidate the shared dependency layer
          # every check here stands on — the same property the `uiEnv` comment
          # below protects from the other direction. The frontend reaches the
          # Rust build only as the `httpUi` derivation, through
          # `SISMATIC_HTTP_UI_DIST`. It sits inside the crate for release-plz's
          # benefit, not cargo's; see the comment on `httpUi`.
          src = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (lib.hasInfix "/python/" path)
              || (lib.hasInfix "/tests/fixtures/" path)
              || (lib.hasSuffix "/server_configuration.yaml" path)
              || (lib.hasSuffix "/container_configuration.yaml" path)
              || (lib.hasSuffix "/container_devices.yaml" path);
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
          # This used to carry a dependency layer of its own, scoped to
          # `-p sismatic-server`, to work around `utoipa-swagger-ui`: the shared
          # `cargoArtifacts` is resolved workspace-wide, that crate resolved to a
          # different feature variant per scope, and cargo would recompile it
          # while reusing the *cached* output of its build script — a script that
          # writes an embed.rs holding an absolute path to the bundle it unzipped
          # into a sandbox that no longer exists. Its replacement,
          # `scalar_api_reference`, ships its bundle in the .crate file, has no
          # build script, and resolves to the same featureless variant either
          # way, so there is nothing left to scope around and this shares the
          # cached layer like `cli` does.
          #
          # There *is* a build script in the graph again — `sismatic-http-api`'s,
          # which gzips that bundle — but it cannot reproduce the bug, and the
          # difference is worth naming rather than trusting. It belongs to a
          # workspace member, and `buildDepsOnly` caches third-party dependencies
          # only, so its output is never the stale half of a half-rebuilt crate.
          # What it writes is bytes rather than a path, so nothing it produces
          # can refer to a sandbox that has gone away.
          server = craneLib.buildPackage (
            individualCrateArgs
            // {
              pname = "sismatic-server";
              cargoExtraArgs = "-p sismatic-server";
              meta.mainProgram = "sismatic-server";
              # The frontend this binary serves from `/ui`.
              env = uiEnv;
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
                # The published artifact carries the same frontend the
                # `nix build .#server` one does.
                env = uiEnv;
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

          # What the pipeline enumerates. Data rather than derivations, so it
          # lives in `legacyPackages`, the one standard output whose contents
          # may be arbitrary attribute sets. Identical on every system; the
          # pipeline reads it on whichever system its runner has.
          legacyPackages.ci = {
            inherit containerArches;
            registries = lib.mapAttrsToList (name: r: {
              inherit name;
              inherit (r) usernameVar tokenSecret;
            }) containerRegistries;
          };

          # OCI's name for this machine's architecture, or null where there is
          # no image to build. `null` rather than a `throw`, so the attribute can
          # be *asked about* on a platform that has no answer — the outputs below
          # are gated on it, and a throw here would take down evaluation of the
          # whole flake on, say, riscv64.
          containerArch =
            {
              x86_64 = "amd64";
              aarch64 = "arm64";
            }
            .${pkgs.stdenv.hostPlatform.parsed.cpu.name} or null;

          # Whether this system can produce the image at all. Linux because the
          # artifact is a Linux image and `serverStatic` is only musl there; the
          # architecture test because the index above covers two.
          containerSupported = pkgs.stdenv.isLinux && containerArch != null;

          # The uid the server runs as.
          #
          # 65532 — distroless' `nonroot`, and the number most `securityContext`
          # templates already carry. Deliberately not 65534/`nobody`, which is
          # the kernel's overflow uid: an id-mapped or NFS-backed volume squashes
          # unknown owners onto it, so a file owned by "nobody" says nothing
          # about who wrote it and two unrelated processes collide on one name.
          containerUid = 65532;

          # The image reference the generated manifests name. The registry
          # is named by containerPrimaryRegistry — a concrete ref rather than a
          # placeholder, so the manifests can be applied as they are.
          containerRepo = registry: "${registry.host}/${registry.namespace}/${containerName}";
          containerRef = "${containerRepo containerRegistries.${containerPrimaryRegistry}}:${version}";

          containerConfigFile = ./crates/sismatic-server/container_configuration.yaml;
          containerDevicesFile = ./crates/sismatic-server/container_devices.yaml;

          # The two files the image ships under /etc/sismatic.
          #
          # Copied from the crate rather than written here, and that is the
          # point: as committed files they are reachable from `cargo test`, so
          # `shipped_config_check` puts both through the real loader on every
          # check run. A config generated inside this derivation could be tested
          # by nothing short of building the image and watching a pod, and
          # `deny_unknown_fields` makes "does this file still load" a question
          # with teeth — a section renamed in `RawServerConfig` turns the baked
          # config into a startup failure.
          containerEtc = pkgs.runCommand "sismatic-container-etc" { } ''
            install -Dm444 ${containerConfigFile} "$out/etc/sismatic/configuration.yaml"
            install -Dm444 ${containerDevicesFile} "$out/etc/sismatic/devices.yaml"
          '';

          containerImage = pkgs.dockerTools.buildLayeredImage {
            name = containerName;
            # Arch-suffixed, because this is one leg of a multi-arch image
            # rather than the image itself. `nix run .#publish-<name>`
            # publishes the unsuffixed tags over both legs.
            tag = "${version}-${toString containerArch}";
            architecture = containerArch;
            # An uncompressed archive, against the `gz` default.
            #
            # `docker save` produces a plain tar and that is what the
            # `docker-archive:` transport is specified against, so this is the
            # format skopeo and `docker load` are unambiguously correct on.
            # Compression here would buy nothing anyway: the only consumer is
            # a push, and the registry protocol compresses each layer on the
            # wire regardless of how the archive that held it was packed.
            compressor = "none";
            # `created` is left at its epoch default on purpose: a build
            # timestamp would make every rebuild a different image even when
            # nothing changed, which is the property `serverRelease` buys with
            # `--mtime='@1'`. The version is carried by a tag and a label.

            contents = [
              serverStatic
              containerEtc
              # /etc/passwd and /etc/group, so the uid below resolves to a name
              # and `getpwuid` has an answer; also /etc/nsswitch.conf, which
              # states `hosts: files dns` rather than leaving it to a resolver's
              # built-in guess.
              (pkgs.dockerTools.fakeNss.override {
                extraPasswdLines = [
                  "sismatic:x:${toString containerUid}:${toString containerUid}:sismatic:/var/lib/sismatic:/noshell"
                ];
                extraGroupLines = [ "sismatic:x:${toString containerUid}:" ];
              })
            ];

            extraCommands = ''
              # /tmp, because a read-only root filesystem still needs somewhere
              # to put a temporary file and the Deployment mounts an emptyDir
              # here. Sticky, as /tmp always is.
              mkdir -p tmp
              chmod 1777 tmp

              # A mount point, not a writable directory. `runtime_config_path`
              # is unset by default (see container_configuration.yaml), and a
              # deployment that turns it on has to mount a volume that outlives
              # the pod — so this is root-owned and unwritable on purpose.
              # Baking it writable would let that setting appear to work right
              # up until the first reschedule.
              mkdir -p var/lib/sismatic
            '';

            config = {
              Entrypoint = [ "/bin/sismatic-server" ];
              # The one variable the image sets for itself, and the one that
              # cannot be a config key — it names *which* document to load. See
              # `CONFIG_PATH_ENV`. Every other setting is left to the
              # environment, to the mounted file, or to the binary's own default.
              Env = [ "SISMATIC_SERVER__CONFIG=/etc/sismatic/configuration.yaml" ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              User = "${toString containerUid}:${toString containerUid}";
              WorkingDir = "/var/lib/sismatic";
              Labels = {
                "org.opencontainers.image.title" = containerName;
                "org.opencontainers.image.description" =
                  "Sismatic server: polls Extron SIS devices over SSH and serves what it read";
                "org.opencontainers.image.version" = version;
                "org.opencontainers.image.licenses" = "ECL-2.0";
                "org.opencontainers.image.source" = "https://github.com/metzenseifner/sismatic";
                "org.opencontainers.image.url" = "https://metzenseifner.github.io/sismatic/";
              };
            };
          };

          #------------------------------------------------------------------#
          #              The environment surface, and what reads it           #
          #------------------------------------------------------------------#
          # Everything below is generated from one JSON document, which the
          # binary generates from the config type. The chain exists so that
          # adding a key to `RawServerConfig` adds it to the reference table and
          # to the example `ConfigMap` with no edit here — see
          # `sismatic_server::schema` for the half of this that lives in Rust.

          # One key's description, rendered for a human who is not reading
          # rustdoc.
          #
          # The descriptions are the fields' own doc comments, which is what
          # makes them free and what makes them need this: a doc comment is
          # written for `cargo doc`, so it wraps across lines and links with
          # rustdoc's `[`Type::method`]` syntax, neither of which means anything
          # in a Markdown table or a YAML comment. Unwrapping and delinking here
          # — rather than flattening the prose in the source — keeps the Rust
          # docs properly linked and costs one jq expression, shared by both
          # generators below so the two renderings cannot diverge.
          envDescription = ''
            (.description // "")
            | gsub("\\s+"; " ")
            | gsub("\\[(?<t>[^\\]]*)\\]\\([^)]*\\)"; .t)
            | gsub("\\[(?<t>`[^\\]]*`)\\]"; .t)
          '';

          envSchema = pkgs.runCommand "sismatic-env-schema.json" { } ''
            ${lib.getExe server} --env-schema > "$out"
          '';

          # The reference table, for the docs site and the README.
          envReference =
            pkgs.runCommand "sismatic-env-reference.md"
              {
                nativeBuildInputs = [ pkgs.jq ];
              }
              ''
                {
                  echo "<!-- Generated by \`nix build .#env-reference\`; do not edit."
                  echo "     The source is \`RawServerConfig\` in crates/sismatic-server. -->"
                  echo
                  echo "| Variable | Type | Description |"
                  echo "| --- | --- | --- |"
                  jq -r '
                    .keys[]
                    | [
                        "`" + .var + "`",
                        (.types | join(" or ")) + (if .list then " (comma-separated)" else "" end),
                        (${envDescription} | gsub("\\|"; "\\|"))
                      ]
                    | "| " + join(" | ") + " |"
                  ' ${envSchema}
                } > "$out"
              '';

          # Example Kubernetes manifests. Built, not committed, so they cannot
          # go stale against the flake that produces the image they name.
          #
          # Self-validating: the derivation parses every file it wrote before it
          # succeeds, so `nix build .#k8s` failing *is* the check. A manifest
          # that is merely wrong is still possible, of course — what this rules
          # out is a generator emitting something no cluster would accept.
          #
          # Every heredoc below is quoted. The YAML is full of `$` and backticks
          # that mean nothing to a shell, and one unquoted delimiter would let
          # the shell eat them silently.
          k8sManifests =
            pkgs.runCommand "sismatic-k8s-manifests"
              {
                nativeBuildInputs = [
                  pkgs.jq
                  pkgs.yq-go
                ];
              }
              ''
                mkdir -p "$out"

                {
                  cat <<'EOF'
                # The two files the image already carries, restated so a cluster
                # owns them. Mounting this ConfigMap over /etc/sismatic replaces
                # both.
                #
                # devices.yaml carries credentials for any real fleet, so
                # anything past a lab belongs in a Secret with the same two keys
                # and a `secret:` volume in place of the `configMap:` one below.
                apiVersion: v1
                kind: ConfigMap
                metadata:
                  name: sismatic-config
                  labels:
                    app.kubernetes.io/name: sismatic
                data:
                  configuration.yaml: |
                EOF
                  sed 's/^/    /' ${containerConfigFile}
                  cat <<'EOF'
                  devices.yaml: |
                EOF
                  sed 's/^/    /' ${containerDevicesFile}
                } > "$out/configmap.yaml"

                # Every setting, as the environment spells it, commented out.
                # This is a catalogue: an uncommented key here would silently
                # outrank the file above, which is the surprise the whole
                # document exists to prevent.
                {
                  cat <<'EOF'
                # Every setting this server accepts, as an environment variable.
                #
                # Generated from the config type itself (`sismatic-server
                # --env-schema`), so the list cannot fall behind the binary.
                # Uncomment what you need: the environment wins over
                # configuration.yaml at any key it names, and is silent at every
                # key it does not.
                apiVersion: v1
                kind: ConfigMap
                metadata:
                  name: sismatic-env
                  labels:
                    app.kubernetes.io/name: sismatic
                data:
                EOF
                  jq -r '
                    .keys[]
                    | "  # " + (${envDescription})
                      + "\n  # " + .var + ": \"\""
                  ' ${envSchema}
                } > "$out/env-configmap.yaml"

                cat > "$out/deployment.yaml" <<'EOF'
                # A singleton, and that is a property of the server rather than
                # a placeholder to scale up later.
                #
                # Two replicas would each open their own SSH session to every
                # device in the fleet, doubling the connection count a recorder
                # sees, and each would answer reads out of its *own* in-memory
                # store — so one query would return different history depending
                # on which pod the Service picked. `Recreate` for the same
                # reason: a rolling update would briefly run two.
                #
                # The memory limit is deliberately above
                # SISMATIC_SERVER__STORE__MAX_MEMORY (256MiB by default). That
                # setting is the store's own cap, enforced on every write; the
                # limit here is the kernel's, enforced by killing the process.
                # The first has to bind before the second, or the pod is OOM
                # killed instead of expiring its oldest reads — raise them
                # together.
                apiVersion: apps/v1
                kind: Deployment
                metadata:
                  name: sismatic
                  labels:
                    app.kubernetes.io/name: sismatic
                spec:
                  replicas: 1
                  strategy:
                    type: Recreate
                  selector:
                    matchLabels:
                      app.kubernetes.io/name: sismatic
                  template:
                    metadata:
                      labels:
                        app.kubernetes.io/name: sismatic
                    spec:
                      securityContext:
                        runAsNonRoot: true
                        runAsUser: @UID@
                        runAsGroup: @UID@
                        fsGroup: @UID@
                        seccompProfile:
                          type: RuntimeDefault
                      containers:
                        - name: sismatic
                          image: "@IMAGE@"
                          imagePullPolicy: IfNotPresent
                          ports:
                            - name: http
                              containerPort: 8080
                          envFrom:
                            - configMapRef:
                                name: sismatic-env
                                optional: true
                          # GET /health_check answers 200 as soon as the process
                          # is routing requests. It deliberately says nothing
                          # about whether devices are reachable — a probe that
                          # failed because a recorder was unplugged would
                          # restart a server that was working perfectly.
                          startupProbe:
                            httpGet:
                              path: /health_check
                              port: http
                            failureThreshold: 30
                            periodSeconds: 2
                          readinessProbe:
                            httpGet:
                              path: /health_check
                              port: http
                            periodSeconds: 10
                          livenessProbe:
                            httpGet:
                              path: /health_check
                              port: http
                            periodSeconds: 20
                          resources:
                            requests:
                              cpu: 50m
                              memory: 64Mi
                            limits:
                              memory: 384Mi
                          securityContext:
                            allowPrivilegeEscalation: false
                            readOnlyRootFilesystem: true
                            capabilities:
                              drop: [ALL]
                          volumeMounts:
                            - name: config
                              mountPath: /etc/sismatic
                              readOnly: true
                            - name: tmp
                              mountPath: /tmp
                      volumes:
                        - name: config
                          configMap:
                            name: sismatic-config
                        # readOnlyRootFilesystem leaves the image's own /tmp
                        # unwritable, so it is replaced by a volume rather than
                        # by a writable layer.
                        - name: tmp
                          emptyDir: {}
                EOF

                cat > "$out/service.yaml" <<'EOF'
                apiVersion: v1
                kind: Service
                metadata:
                  name: sismatic
                  labels:
                    app.kubernetes.io/name: sismatic
                spec:
                  selector:
                    app.kubernetes.io/name: sismatic
                  ports:
                    - name: http
                      port: 80
                      targetPort: http
                EOF

                # The two values the quoted heredoc could not interpolate. Done
                # with placeholders rather than an unquoted delimiter so that
                # exactly these two substitutions happen and nothing else does.
                substituteInPlace "$out/deployment.yaml" \
                  --replace-fail '@IMAGE@' '${containerRef}' \
                  --replace-fail '@UID@' '${toString containerUid}'

                # The validation this derivation's comment promises. Syntax,
                # not schema: `kubeconform` would check these against the real
                # Kubernetes OpenAPI definitions, but it fetches them over the
                # network and this sandbox has none. So what is ruled out here
                # is a generator that emitted something unparseable — a jq
                # program that lost its indentation, a heredoc that swallowed a
                # line — and not a field Kubernetes would reject.
                for manifest in "$out"/*.yaml; do
                  yq --exit-status 'true' "$manifest" > /dev/null \
                    || { echo "error: generated $manifest is not valid YAML" >&2; exit 1; }
                done
              '';

          #------------------------------------------------------------------#
          #                       Publishing the image                        #
          #------------------------------------------------------------------#
          # One app per registry, generated from `containerRegistries`. Each
          # one does the whole job for its destination: check the archives,
          # log in, push every architecture's leg, then write the multi-arch
          # index over them. Writing the index in the same run that pushed the
          # legs guarantees the order per registry, and lets the pipeline give
          # every registry its own job, holding only that registry's token.
          #
          # None of these apps refers to `containerImage`. They take archives
          # off disk, so the job that holds credentials never compiles the
          # musl toolchain, and one x86_64 runner can publish both legs:
          # skopeo copies manifests and blobs and never executes them.
          #
          # Credentials: skopeo writes a Docker-format auth file at
          # `REGISTRY_AUTH_FILE`, and regctl reads the same file through
          # `DOCKER_CONFIG` (the directory that holds it), so one login covers
          # both tools.

          # What an architecture's archive is called. `export-container`
          # writes this name and every publish app reads it, so the
          # convention is stated once.
          containerArchiveName = arch: "sismatic-${arch}.tar";

          # skopeo's trust policy, passed explicitly because it will not copy
          # without one. nixpkgs installs upstream's default here but does not
          # patch the compiled-in lookup path (/etc/containers/policy.json),
          # which exists in no build sandbox and on no guaranteed runner. The
          # policy is upstream's permissive default: there is no signature to
          # verify on an image this flake built moments ago.
          skopeoPolicy = "${pkgs.skopeo}/etc/containers/default-policy.json";

          # Shell that refuses to go on unless `$archives` holds every leg.
          # Runs before any credential is read, so a missing leg can never
          # leave a registry half-populated with an index over an incomplete
          # set.
          checkArchives = lib.concatMapStringsSep "\n" (arch: ''
            if [ ! -f "$archives/${containerArchiveName arch}" ]; then
              echo "error: no ${arch} archive at $archives/${containerArchiveName arch}" >&2
              exit 1
            fi
          '') containerArches;

          # `nix run .#export-container -- <dir>` puts this machine's leg into
          # <dir> under the name the publish apps expect, so the workflow
          # moves a directory and never spells a file name.
          exportContainer = pkgs.writeShellApplication {
            name = "sismatic-export-container";
            text = ''
              dir=''${1:?usage: sismatic-export-container <dir>}
              mkdir -p "$dir"
              install -m644 ${containerImage} "$dir/${containerArchiveName containerArch}"
              echo "$dir/${containerArchiveName containerArch}"
            '';
          };

          # `nix run .#publish-<name> -- <dir> [tag...]`
          #
          # Reads REGISTRY_USERNAME and REGISTRY_TOKEN rather than the
          # per-registry names, so the pipeline sets the same two variables
          # in every job. With no tags it publishes the version and `latest`.
          publishTo =
            name: registry:
            pkgs.writeShellApplication {
              name = "sismatic-publish-${name}";
              runtimeInputs = [
                pkgs.skopeo
                pkgs.regclient
              ];
              text = ''
                archives=''${1:?usage: sismatic-publish-${name} <dir> [tag...]}
                shift

                ${checkArchives}

                : "''${REGISTRY_USERNAME:?${name} needs REGISTRY_USERNAME (the ${registry.usernameVar} variable in CI)}"
                : "''${REGISTRY_TOKEN:?${name} needs REGISTRY_TOKEN (the ${registry.tokenSecret} secret in CI)}"

                # The auth file holds a live token, so it lives in a private
                # temporary directory that is removed however this exits.
                auth=$(mktemp -d)
                trap 'rm -rf "$auth"' EXIT
                export REGISTRY_AUTH_FILE="$auth/config.json" DOCKER_CONFIG="$auth"
                printf '%s' "$REGISTRY_TOKEN" \
                  | skopeo login "${registry.host}" --username "$REGISTRY_USERNAME" --password-stdin

                repo=${containerRepo registry}

                ${lib.concatMapStringsSep "\n" (arch: ''
                  echo "==> $repo:${version}-${arch}"
                  # `--format oci` so the leg and the index that wraps it are
                  # the same kind of thing. `dockerTools` writes a Docker v2s2
                  # manifest and `regctl index create` writes an OCI index; the
                  # conversion is manifest-level and deterministic, so every
                  # registry receives the same digest and a re-run is a no-op.
                  skopeo --policy ${skopeoPolicy} copy --retry-times 3 --format oci \
                    "docker-archive:$archives/${containerArchiveName arch}" \
                    "docker://$repo:${version}-${arch}"
                '') containerArches}

                tags=("$@")
                [ ''${#tags[@]} -gt 0 ] || tags=("${version}" "latest")
                for tag in "''${tags[@]}"; do
                  echo "==> $repo:$tag over ${lib.concatStringsSep ", " containerArches}"
                  regctl index create "$repo:$tag" \
                    ${lib.concatMapStringsSep " " (arch: ''--ref "$repo:${version}-${arch}"'') containerArches}
                done
              '';
            };

          # `nix run .#publish-all -- <dir> [tag...]`
          #
          # Every registry from one machine: the recovery path when the
          # pipeline is unavailable, and the way to publish from a laptop.
          # Reads each registry's credentials from environment variables
          # named after its `usernameVar` and `tokenSecret`, and hands them to
          # that registry's own app, so this adds no publishing logic of its
          # own.
          #
          # Strict before it starts: every archive and every credential must
          # be present, so a missing token is named before anything is
          # uploaded rather than halfway through. Once publishing has begun it
          # continues past a failing registry, the way the pipeline's
          # `fail-fast: false` does, and names every failure at the end.
          publishAll = pkgs.writeShellApplication {
            name = "sismatic-publish-all";
            text = ''
              archives=''${1:?usage: sismatic-publish-all <dir> [tag...]}
              shift

              ${checkArchives}

              missing=()
              ${lib.concatStrings (
                lib.mapAttrsToList (_: r: ''
                  [ -n "''${${r.usernameVar}:-}" ] || missing+=(${r.usernameVar})
                  [ -n "''${${r.tokenSecret}:-}" ] || missing+=(${r.tokenSecret})
                '') containerRegistries
              )}
              if [ ''${#missing[@]} -gt 0 ]; then
                echo "error: set these before publishing anywhere: ''${missing[*]}" >&2
                exit 1
              fi

              failed=()
              ${lib.concatStrings (
                lib.mapAttrsToList (name: r: ''
                  echo "==> ${name}"
                  REGISTRY_USERNAME="''$${r.usernameVar}" REGISTRY_TOKEN="''$${r.tokenSecret}" \
                    ${lib.getExe (publishTo name r)} "$archives" "$@" \
                    || failed+=(${name})
                '') containerRegistries
              )}

              if [ ''${#failed[@]} -gt 0 ]; then
                echo "error: publishing failed for: ''${failed[*]}" >&2
                echo "       re-run each with: nix run .#publish-<name> -- $archives" >&2
                exit 1
              fi
            '';
          };

          # What the image promises a cluster, asserted against the image itself
          # rather than against the expression that built it.
          #
          # Worth a check of its own because each of these is a fault nothing
          # else catches: a root `User` passes every unit test and is refused by
          # `runAsNonRoot` at admission; a missing `SISMATIC_SERVER__CONFIG`
          # leaves the binary looking for ./configuration.yaml and exiting 2; a
          # port that drifted from the one the Service targets produces a pod
          # that is perfectly healthy and unreachable.
          containerConfigCheck =
            pkgs.runCommand "sismatic-container-config-check"
              {
                nativeBuildInputs = [
                  pkgs.skopeo
                  pkgs.jq
                ];
              }
              ''
                skopeo --policy ${skopeoPolicy} inspect --config \
                  "docker-archive:${containerImage}" > config.json
                jq . config.json

                expect() {
                  local filter=$1 want=$2 got
                  got=$(jq -r "$filter" config.json)
                  if [ "$got" != "$want" ]; then
                    echo "error: $filter is '$got', expected '$want'" >&2
                    exit 1
                  fi
                }

                expect '.config.Entrypoint | join(" ")' '/bin/sismatic-server'
                expect '.config.User' '${toString containerUid}:${toString containerUid}'
                expect '.config.ExposedPorts | keys | join(",")' '8080/tcp'
                expect '.architecture' '${toString containerArch}'
                expect '.os' 'linux'
                expect '.config.Env | map(select(startswith("SISMATIC_SERVER__CONFIG="))) | join("")' \
                  'SISMATIC_SERVER__CONFIG=/etc/sismatic/configuration.yaml'
                expect '.config.Labels["org.opencontainers.image.version"]' '${version}'

                touch "$out"
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
              # The settings table is derived from the server's config type, not
              # written by hand, so it is dropped here rather than committed —
              # `docs/deployment.md` pulls it in with a `--8<--` snippet. Written
              # into docs/ relative to the working directory, which mkdocs
              # already requires to be the repo root because that is where
              # mkdocs.yml is. Gitignored: it is a build output that happens to
              # land in a source tree.
              install -m644 ${envReference} docs/env-reference.md

              if [ "$#" -eq 0 ]; then
                exec mkdocs build
              else
                exec mkdocs "$@"
              fi
            '';
          };

          # Node for the http-ui/ frontend (vite + react-ts). nixpkgs attribute
          # names carry only the MAJOR version — there is no `nodejs_26_4` —
          # because the minor/patch is a property of the nixpkgs *revision*,
          # not of the attribute. That revision is pinned in flake.lock, so
          # `nodejs_26` resolves to exactly one version (today 26.4.0) on
          # every machine and in every CI run until someone runs
          # `nix flake update`. The pin is the lock, not the attribute name.
          #
          # The guard below makes that implicit pin explicit: a minor bump
          # riding along with an unrelated `nix flake update` becomes an
          # eval-time error naming both versions, instead of a silent change
          # in everyone's dev shell. To accept a bump, edit nodeVersion.
          #
          # Switching majors: nodejs_22 (maintenance LTS until 2027-04),
          # nodejs_24 (LTS, but see below), nodejs_26 (current, LTS from
          # 2026-10). Vite needs >= 20.19 / >= 22.12. The nodejs derivation
          # bundles npm + npx.
          #
          # NOT nodejs_24: 24.16.0 aborts `pnpm add` (SIGABRT, exit 134).
          # Node tracks the raw fds a worker thread opens via fs.open/fs.close
          # and closes them on worker exit (the `trackUnmanagedFds` Worker
          # option, on by default). 24.16.0 miscounts — it warns "File
          # descriptor N closed but not opened in unmanaged mode" — and on
          # worker exit closes an fd it does not own, which is libuv's own
          # async pipe. The next uv_async_send() write gets EBADF, and libuv
          # answers that with abort():
          #   Worker::Exit -> Environment::ExitEnv -> uv_async_send -> abort
          # The install itself completes first, so the lockfile is written and
          # only the exit status is lost — which makes it easy to mistake for
          # a pnpm bug. It is not: pnpm 11.9.0 is clean on both neighbouring
          # majors, and pnpm exposes no knob for trackUnmanagedFds. Measured
          # in http-ui/, 3 runs each: 22.23.1 and 26.4.0 both exit 0 with zero
          # fd warnings; 24.16.0 exits 134 with 9, every run. Revisit 24 only
          # if a later patch fixes it.
          nodeVersion = "26.4.0";
          nodejs =
            # without this guard, a minor bump arrives silently as a side effect
            # of nix flake update. this forces manual decision to accept the
            # incremented minor version.
            if pkgs.nodejs_26.version == nodeVersion then
              pkgs.nodejs_26
            else
              throw ''
                flake.nix: Node is pinned to ${nodeVersion}, but the locked
                nixpkgs now ships ${pkgs.nodejs_26.version}. To accept the bump set
                  nodeVersion = "${pkgs.nodejs_26.version}";
                and re-run `pnpm install` in crates/sismatic-http-api/http-ui/ so the
                lockfile is rebuilt.
              '';

          # pnpm, pinned the same way as Node. There IS coupling to Node, in
          # three directions that behave quite differently:
          #
          # 1. pnpm declares `engines.node`: 11.9.0 wants >= 22.13, 10.x wants
          #    >= 18.12. pnpmRequiresNode records the pinned pnpm's figure and
          #    the assert checks the pinned Node against it, so an
          #    incompatible pair is an eval error, not a runtime surprise.
          #    Re-read it after a bump with:
          #      nix eval --raw --impure --expr '(builtins.fromJSON (builtins.readFile "${(builtins.getFlake (toString ./.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem}.pnpm_11}/libexec/pnpm/package.json")).engines.node'
          #
          # 2. The `pnpm` CLI does not run on the `nodejs` above. nixpkgs
          #    builds it with its own nodejs-slim and `patchShebangs` bakes
          #    that absolute store path into bin/pnpm.mjs, so pnpm's
          #    interpreter is fixed at build time and the Node on PATH only
          #    reaches the scripts pnpm *spawns* (`pnpm run dev`). Left alone
          #    that interpreter is nixpkgs' choice: nodejs-slim_24 (24.16.0)
          #    in this revision, which is exactly the build that aborts
          #    `pnpm add` — see the nodeVersion comment. Repinning nodeVersion
          #    does NOT fix that, because it never touches this shebang. So
          #    the override below pins the interpreter too, to the slim build
          #    of the same major as `nodeVersion`. Keep the two in step.
          #    That is also why (1) is a recorded constant rather than a check
          #    against pnpm's real interpreter: the two remain independent
          #    knobs, now both set deliberately.
          #
          # 3. pnpm 11 replaces itself, and Node, at runtime. A
          #    `packageManager` or `devEngines.packageManager` field naming a
          #    different pnpm makes it download and exec that version
          #    (`pmOnFail` defaults to "download"); `devEngines.runtime` does
          #    the same for Node via `runtimeOnFail`. Either silently defeats
          #    this pin. The only places that switch it off are
          #    `pnpm-workspace.yaml` (`pmOnFail: ignore`) and the CLI flag
          #    (`--pm-on-fail=ignore`) — measured, not assumed: `.npmrc` is
          #    not consulted for it, nor is any npm_config_* env var, and the
          #    pnpm 10 spelling (`manage-package-manager-versions`) is gone.
          #    The shellHook warns when http-ui/ is missing the setting.
          #
          # Note: pnpm_9 is marked insecure in this nixpkgs revision, so 10 or
          # 11 are the real choices.
          pnpmVersion = "11.9.0";
          pnpmRequiresNode = "22.13";
          pnpm =
            assert lib.assertMsg (lib.versionAtLeast nodeVersion pnpmRequiresNode) ''
              flake.nix: pnpm ${pnpmVersion} requires Node >= ${pnpmRequiresNode}, but Node
              is pinned to ${nodeVersion}. Raise nodeVersion (and the nodejs_NN
              attribute it reads) or pin an older pnpm.
            '';
            assert lib.assertMsg (pkgs.nodejs-slim_26.version == nodeVersion) ''
              flake.nix: pnpm's interpreter is pinned to nodejs-slim_26
              (${pkgs.nodejs-slim_26.version}) but nodeVersion is ${nodeVersion}.
              These must be the same build: the whole point of the override is
              that pnpm runs on the Node we chose, not on whichever one nixpkgs
              happened to build it against. Move both attributes together.
            '';
            if pkgs.pnpm_11.version == pnpmVersion then
              # nodejs-slim, not nodejs: generic.nix warns on the `nodejs` arg
              # and tells you to override this one. It is the interpreter
              # patchShebangs stamps into bin/pnpm.mjs.
              pkgs.pnpm_11.override { nodejs-slim = pkgs.nodejs-slim_26; }
            else
              throw ''
                flake.nix: pnpm is pinned to ${pnpmVersion}, but the locked
                nixpkgs now ships ${pkgs.pnpm_11.version}. To accept the bump set
                  pnpmVersion = "${pkgs.pnpm_11.version}";
                and re-check pnpmRequiresNode against the new pnpm's engines.node.
              '';

          #------------------------------------------------------------------#
          #                    The http-ui frontend derivation               #
          #------------------------------------------------------------------#
          # The bytes of this derivation get embedded in `sismatic-http-api`
          # when the "ui" feature is imported. This derivation serves as input
          # to get advantages of nix build without relying on a committed dist/ directory.

          # Here the frontend's sources and `pnpm-lock.yaml` are inputs to this
          # derivation.
          #
          # `src` is the flake source, which is the git tree: `node_modules/`
          # and `dist/` are gitignored and therefore already absent from the
          # sandbox, so there is nothing to filter out.
          #
          # The frontend lives *inside* `sismatic-http-api` rather than at the
          # repo root, and that location is load-bearing for releases rather
          # than cosmetic. release-plz decides which commits belong to a package
          # by asking whether they touched that package's directory — its only
          # other watched path is a `readme` pointing outside it, and there is no
          # "extra paths" knob. A frontend at the repo root is therefore
          # invisible to version determination: every commit under it would ship
          # with no version bump at all. See release-plz.toml.
          httpUi = pkgs.stdenv.mkDerivation (finalAttrs: {
            pname = "sismatic-http-ui";
            inherit version;
            src = ./crates/sismatic-http-api/http-ui;

            # The offline pnpm store, as a fixed-output derivation — the one
            # place in this flake that is allowed to reach the network, and it
            # is pinned by `hash` to exactly what `pnpm-lock.yaml` resolves to.
            # Re-run after any `pnpm install`: set `hash = lib.fakeHash;`, build,
            # and take the hash the mismatch reports.
            # `fetchPnpmDeps` resolves the lockfile by *running* pnpm, so it is
            # overridden onto the pnpm pinned above rather than taken as it
            # comes — otherwise the store would be built by whichever pnpm
            # nixpkgs happens to default to, which is the pin's whole purpose.
            # (`pnpm.fetchDeps` is the deprecated alias of the same thing.)
            pnpmDeps = (pkgs.fetchPnpmDeps.override { inherit pnpm; }) {
              inherit (finalAttrs) pname version src;
              fetcherVersion = 3;
              hash = "sha256-vQgFcvqyrm8wtgxPn5I02TKDYVwr/y1rC/ntczFd/EA=";
            };

            # `pnpmConfigHook` needs no override: it resolves `pnpm` off `PATH`,
            # which is what the pinned one beside it puts there. The hook links
            # `pnpmDeps` into an offline store and installs from it, so the build
            # below never reaches the network.
            nativeBuildInputs = [
              nodejs
              pnpm
              pkgs.pnpmConfigHook
            ];

            # `pnpm build` is `tsc -b && vite build`, so a frontend that does
            # not type-check fails this derivation — which is why it is also
            # registered as a check below rather than only as a package.
            buildPhase = ''
              runHook preBuild
              pnpm build
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              cp -r dist $out
              runHook postInstall
            '';
          });

          # Handed to every derivation that compiles `sismatic-http-api` with
          # its `ui` feature on: the server binaries, and the workspace-wide
          # lint/test/coverage runs that compile the server as a member.
          #
          # Deliberately NOT part of `commonArgs`. `cargoArtifacts` is built
          # from those, so a CSS edit would invalidate the shared third-party
          # dependency layer that every check in this file stands on — tens of
          # minutes of rebuild for a change no dependency can see. Nothing is
          # lost by leaving it out: crane builds that layer over dummy sources,
          # so no workspace member's build script runs there at all.
          #
          # A derivation that compiles the crate *without* this (the `doc`
          # check, scoped to core; a bare `cargo build`) is not a failure — see
          # `dist_dir` in that crate's `build.rs`.
          uiEnv = {
            SISMATIC_HTTP_UI_DIST = "${httpUi}";
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

            # The frontend building at all, on the same principle — and since
            # `pnpm build` is `tsc -b && vite build`, this is where a type error
            # in `http-ui/` fails the check run. The pipeline enumerates checks
            # dynamically, so it needs no workflow change to pick this up.
            http-ui = httpUi;

            # Clippy as a separate derivation: CI blocks on lints, but
            # downstream consumers can still build the package without
            # being subject to them.
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
                # Lints the frontend's embed path too: without this the `ui`
                # feature would be linted against the placeholder.
                env = uiEnv;
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

                # Both halves are vacuous while every member is released, and both
                # must stay: half one is the check that a newly added crate — the
                # only kind that is ever `release = false` now — is scoped during
                # the one release it spends unreleased.
                #
                # Half one: an unreleased member with an unscoped template is the
                # intent-relay bug, armed and waiting for the next new crate.
                unscopedUnreleased = map (p: p.name) (
                  builtins.filter (p: !(isReleased p) && !(isScoped p)) entries
                );
                # Half two: a released member with a scoped template loses its
                # changelog baseline — and then votes on the version with its
                # entire history, which is a breaking bump every release.
                scopedReleased = map (p: p.name) (builtins.filter (p: isReleased p && isScoped p) entries);

                # Promoting a crate to released is the one case this check cannot
                # settle statically: it would have to prove the crate exists at the
                # last tag, which needs history the sandbox does not have. Pin the
                # set so that decision is made by a human editing this list, who can
                # then check the tag by hand.
                #
                # Every member is released today — that is the point of the config,
                # so that a change in any crate produces a version. The pin's job is
                # therefore no longer to hold a minority open: it is the thing that
                # catches a *newly added* crate, which cannot be released in the same
                # PR that introduces it (it does not exist at the last tag, so the
                # workspace-shaped template sends release-plz looking for it in a
                # worktree that has never heard of it). A new member must ship once
                # as `release = false` + scoped, then join this list.
                expectedReleased = [
                  "sismatic-api-types"
                  "sismatic-cli"
                  "sismatic-core"
                  "sismatic-http-api"
                  "sismatic-intent-relay"
                  "sismatic-python-sdk"
                  "sismatic-server"
                  "sismatic-store"
                  "sismatic-store-memory"
                  "sismatic-sync"
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

                # `ok` above only asks whether the requirement holds *right now*.
                # That is not enough, and the gap has now cost a release: at
                # workspace 0.2.25 the exact pin `sismatic-intent-relay = "0.2.25"`
                # in sismatic-server satisfied ^0.2.25 and sailed through this
                # check, then broke the first release PR that crossed a minor
                # boundary — release-plz bumped the member to 0.3.0, the pin still
                # demanded ^0.2.25, and `cargo update` failed with
                #
                #   failed to select a version for the requirement
                #   `sismatic-intent-relay = "^0.2.25"`
                #   candidate versions found which didn't match: 0.3.0
                #
                # which is exactly the rot the note in [workspace.dependencies]
                # warns against, and exactly the blind spot the older version of
                # this comment predicted ("invisible here until the 0.3.0
                # boundary"). Predicting a hole is not the same as closing it.
                #
                # So require the requirement to be bump-proof, not merely
                # currently-true: it must spell out no more than the leading zero,
                # i.e. `0` (or `^0`, or `*`). `^0` matches every 0.x, so every bump
                # below 1.0 keeps resolving and nothing here needs maintaining. At
                # a 1.0 bump release-plz rewrites these to `1` itself.
                bumpProof =
                  d:
                  d.req == "*"
                  || builtins.elem d.req [
                    "0"
                    "^0"
                  ];
                narrow = lib.filter (d: ok d && !(bumpProof d)) deps;
              in
              assert lib.assertMsg (narrow == [ ]) ''
                Internal path dependencies pinned more narrowly than `0`:
                ${lib.concatStringsSep "\n" (map describe narrow)}
                These satisfy the current workspace version ${version}, so they look
                fine today, but they will fail the next release that crosses the
                bound — release-plz bumps the depended-on member and `cargo update`
                can no longer select it:
                  failed to select a version for the requirement `<crate> = "^X.Y.Z"`
                  candidate versions found which didn't match: <new version>
                Use `version = "0"` and declare the dep in [workspace.dependencies],
                consumed as `<crate>.workspace = true` — see the note there.
              '';
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
                # `tests/ui.rs` and `ui::tests` assert against the *real*
                # frontend, so the test run is given one.
                env = uiEnv;
              }
            );
          }
          # Code coverage
          # Tarpaulin only works on Linux, hence the gate.
          # cargo-llvm-cov is an alternative to tarpaulin
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            coverage = craneLib.cargoTarpaulin (
              commonArgs
              // {
                inherit cargoArtifacts;
                # The same frontend `nextest` is given: this runs the same
                # tests, and a `ui::tests` compiled against the placeholder
                # would cover a different branch than the one that ships.
                env = uiEnv;
                # `--engine llvm` rather than tarpaulin's default, which is
                # ptrace: that one single-steps the test binary through an INT3
                # written over every address DWARF calls a statement, and crane
                # builds every check `--release`, where a statement no longer
                # maps onto one address. Some breakpoints land mid-instruction,
                # and stepping over them resumes the process in the middle of
                # one — it miscomputes rather than crashes, which is the bad kind
                # of wrong. The run that found this had
                # `openapi::tests::the_page_carries_the_bundle_path_and_the_configuration`
                # observe `scalar_config()` as `false` instead of the object it
                # is, while `nextest` ran the same test out of the same
                # `--release` profile on the same commit and passed.
                #
                # The llvm engine instruments at compile time and reads the
                # `.profraw` afterwards, so nothing patches the running process
                # and optimization stops being a correctness question. It is also
                # what cargo-llvm-cov does, the alternative named above.
                #
                # The three flags after it are crane's own default tail, which
                # this attribute replaces wholesale rather than appends to.
                cargoTarpaulinExtraArgs = "--engine llvm --skip-clean --out xml --output-dir $out";
              }
            );
          }
          // {
            # The generated Kubernetes manifests, which validate themselves —
            # building this derivation parses every file it wrote. Cheap enough
            # to be a check anywhere: it stands on the native `server` (already
            # a check above) and not on `serverStatic`, so unlike the image it
            # drags no musl toolchain in.
            #
            # What it actually guards is the generator. The manifests are built
            # rather than committed precisely so they cannot go stale, and the
            # cost of that is that nobody reads them before they are published;
            # this is what notices when a schema change turns the env catalogue
            # into something no cluster would accept.
            k8s = k8sManifests;
          };
        in
        {
          inherit checks legacyPackages;

          packages = {
            default = cli;
            # `nix build .#cli` / `.#server` -> the binaries.
            inherit cli server;
            # `nix build .#wheel` -> result/sismatic-*.whl
            inherit wheel;

            # `nix build .#http-ui` -> the frontend's dist on its own, which is
            # also what `.#server` embeds. Worth an output of its own so the
            # thing the Rust build takes as an input can be built and inspected
            # without building the binary around it.
            http-ui = httpUi;

            # The dependency layer on its own. Nothing consumes this directly —
            # it exists so one machine (locally, or one CI job) can compile the
            # expensive Cargo.lock-keyed layer once and populate a store the
            # checks then hit instead of rebuilding. One layer now covers every
            # build; `server-deps` was a second one scoped to sismatic-server,
            # and the comment on `server` says why it is gone.
            deps = cargoArtifacts;

            # `nix build .#server-portable` -> a sismatic-server that runs on
            # a machine with no Nix; `.#server-release` -> that binary packed
            # as sismatic-server-<version>-<target>.tar.gz plus its SHA-256.
            server-portable = serverPortable;
            server-release = serverRelease;

            # The environment surface, read off `RawServerConfig` by the binary
            # itself. Available everywhere, unlike the image below: the question
            # "what can I set" has the same answer on a laptop as in a pod, and
            # the docs build needs it on whatever system runs it.
            env-schema = envSchema;
            env-reference = envReference;

            # `nix build .#k8s` -> example manifests naming this exact version.
            # Outside the gate below because it needs only the native `server`
            # to read the schema off: the manifests describe the image without
            # building it.
            k8s = k8sManifests;
          }
          # The container image and everything derived from it. Linux only, and
          # only on the two architectures the index covers: `serverStatic` is a
          # musl build, which is a thing only Linux has, and the arch-suffixed
          # tag has to be a name OCI recognises. On every other system these
          # outputs are simply absent rather than present-and-failing, so
          # `nix flake check` on a laptop checks what that laptop can build.
          // lib.optionalAttrs containerSupported {
            # `nix build .#container` -> a docker-archive tarball, loadable with
            # `docker load < result` and pushable with `nix run
            # .#export-container` published with `nix run .#publish-all`.
            container = containerImage;

            # The image's own invariants, asserted against the built artifact.
            #
            # A package rather than a check, for exactly the reason
            # `serverRelease` is not a check: it stands on `serverStatic`, and
            # putting it in `checks` would drag the musl toolchain into every
            # `nix flake check` — tens of minutes added to a run that is
            # supposed to be the fast gate. The container job in the pipeline
            # builds it alongside the image it inspects, where that toolchain
            # is already paid for.
            container-config-check = containerConfigCheck;
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
          }
          # Publishing the image. Not gated on `containerSupported`: none of
          # these apps builds the image, so any machine can publish archives
          # that native runners built. That is what lets one x86_64 runner
          # push both legs, and what makes a release recoverable from a
          # laptop.
          // lib.mapAttrs' (
            name: r:
            lib.nameValuePair "publish-${name}" {
              type = "app";
              program = lib.getExe (publishTo name r);
            }
          ) containerRegistries
          // {
            publish-all = {
              type = "app";
              program = lib.getExe publishAll;
            };
          }
          # Exporting, by contrast, needs the image, so it exists only where
          # the image can be built.
          // lib.optionalAttrs containerSupported {
            export-container = {
              type = "app";
              program = lib.getExe exportContainer;
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
                # Frontend toolchain for http-ui/ (vite + react-ts). Pinned by the
                # flake like every other tool, so `npm` here is the same npm on
                # a laptop and in CI rather than whatever the host happens to
                # have on PATH. Scaffold with:
                #   pnpm install   (in crates/sismatic-http-api/http-ui/)
                nodejs
                pnpm
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

                # pnpm can swap itself (and Node) out at runtime on the
                # strength of a package.json field, which would quietly undo
                # the pins above. Only the frontend's `pnpm-workspace.yaml` can
                # disable that, and the flake cannot write a file into a project
                # that does not exist yet -- so warn rather than imply the pin
                # is airtight. See the pnpm binding for the mechanism.
                SIS_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
                if [ -f "$SIS_ROOT/crates/sismatic-http-api/http-ui/package.json" ] \
                  && ! grep -qs pmOnFail "$SIS_ROOT/crates/sismatic-http-api/http-ui/pnpm-workspace.yaml"; then
                  echo "warning: crates/sismatic-http-api/http-ui/pnpm-workspace.yaml does not set 'pmOnFail: ignore', so a" >&2
                  echo "         packageManager field can override this flake's pnpm ${pnpmVersion}." >&2
                fi
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
      # "Project" here as a verb, not a noun
      project = field: builtins.mapAttrs (_: out: out.${field}) perSystem;
    in
    {
      # Projections over Record(system)
      packages = project "packages";
      apps = project "apps";
      devShells = project "devShells";
      checks = project "checks";
      legacyPackages = project "legacyPackages";
    };
}
