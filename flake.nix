{
  description = "Sismatic: Extron SIS device tooling, built with crane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    # Toolchain version lives in rust-toolchain.toml; refresh what is
    # available with `nix flake update rust-overlay`.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # RustSec database for cargo-audit; the sandbox has no network.
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

      systems = lib.systems.flakeExposed;

      # Instantiate pkgs (with rust-overlay) per system and apply f.
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
      # System-independent; exported as legacyPackages.ci. Registries are
      # defined only here: the workflow names none and every app is generated.
      # The repository name under every registry.
      containerName = "sismatic";

      # Used as the OCI description label (GHCR's About line), Docker Hub's
      # `description`, and the overview lead. Docker Hub refuses more than 100
      # characters, so the limit is enforced at evaluation.
      containerDescription =
        let
          desc = "Sismatic server: polls Extron SIS devices over SSH and serves what it read";
          len = builtins.stringLength desc;
          tooLong = "containerDescription is ${toString len} characters; Docker Hub refuses more than 100";
        in
        lib.throwIf (len > 100) tooLong desc;

      # One native runner per arch (see `serverStatic`).
      containerArches = [
        "amd64"
        "arm64"
      ];

      # Keyed by name: `publish-<name>` app and CI job. Entries name where
      # credentials live, never their values:
      #   usernameVar  repository variable (not secret, so logs stay unmasked)
      #   tokenSecret  repository secret
      #   docs         how the registry page is written, or null. `kind` selects
      #                the API in `publishDocsTo`; `tokenSecret` is a separate
      #                credential; `maxChars` is the size limit, or null.
      # Locally, `publish-all` reads env vars with the same names.
      # Don't name a registry `all` (collides with `publish-all`).
      containerRegistries = {
        ghcr = {
          host = "ghcr.io";
          namespace = "metzenseifner";
          usernameVar = "GHCR_USERNAME";
          # GitHub's own token: forks need only `packages: write`.
          tokenSecret = "GITHUB_TOKEN";
          # GHCR has no writable description: `org.opencontainers.image.source`
          # links the package to the repo, whose README it renders, with the
          # description label as the About line.
          docs = null;
        };
        quay = {
          host = "quay.io";
          namespace = "metzenseifner";
          usernameVar = "QUAY_USERNAME";
          tokenSecret = "QUAY_TOKEN";
          # Disabled: the API credential is unresolved. To re-enable, add the
          # `QUAY_API_TOKEN` secret and set:
          #   docs = { kind = "quay"; tokenSecret = "QUAY_API_TOKEN"; maxChars = null; };
          # Known (from GET https://quay.io/api/v1/discovery):
          #   - `PUT /api/v1/repository/<ns>/<name>`, body `{"description": <md>}`,
          #     scope `repo:write`; the request `publishDocsTo` builds matches it.
          #   - Only `oauth2_implicit` auth exists, so the robot `QUAY_TOKEN` is
          #     likely insufficient (inferred, not measured).
          #   - A non-OAuth credential gets `403 CSRF token was invalid or missing.`
          #   - OAuth apps are organization-scoped; a user namespace needs a holding
          #     org whose app token carries the granting user's access.
          #   - No documented size limit.
          docs = null;
        };
        dockerhub = {
          host = "docker.io";
          namespace = "metzenseifner";
          usernameVar = "DOCKERHUB_USERNAME";
          tokenSecret = "DOCKERHUB_TOKEN";
          # One PATCH writes `description` and `full_description`. It needs a
          # Read, Write, Delete PAT: a Read & Write one (enough to push) logs in, then
          # gets 403. A separate secret keeps the push token narrower.
          docs = {
            kind = "dockerhub";
            tokenSecret = "DOCKERHUB_DOCS_TOKEN";
            # Docker Hub silently truncates longer bodies; fail the build instead.
            maxChars = 25000;
          };
        };
      };

      # Registry the generated k8s manifests pull from.
      containerPrimaryRegistry = "ghcr";

      # Registries with a page to write; drives every docs app, credential and CI step.
      registriesWithDocs = lib.filterAttrs (_: r: r.docs != null) containerRegistries;

      perSystemOutputs =
        { system, pkgs }:
        let
          # Toolchain pinned by ./rust-toolchain.toml.
          rustToolchain = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Cargo sources plus files tests read at build time:
          #   python/                    stub freshness test (include_str! in src/stub.rs)
          #   tests/fixtures/            integration test configs
          #   server_configuration.yaml  shipped config; a test checks it still loads
          #   container_*.yaml           image configs; `shipped_config_check` loads them
          # The http-ui frontend is excluded on purpose so UI edits cannot invalidate
          # `cargoArtifacts`; it reaches Rust only via `httpUi`.
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

          # Shared by every workspace member.
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
          # Arguments shared by every crane call below.
          commonArgs = {

            inherit src version;
            # Virtual workspace: no root package. Per-crate builds override this.
            pname = "sismatic-workspace";
            strictDeps = true;

            # aws-lc-sys (russh, via core's `ssh` feature, unified workspace-wide)
            # needs cmake + perl in every build.
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.perl
            ];
            # aws-lc-sys runs cmake itself; the root has no CMakeLists.txt.
            dontUseCmakeConfigure = true;

            buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];
          };

          # Dependencies only, keyed on Cargo.lock: the cached layer every check shares.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # Per-crate builds on top of the cached dependencies.
          individualCrateArgs = commonArgs // {
            inherit cargoArtifacts;
            # Tests run once, in `nextest`.
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

          # The deployable composition root (`sismatic-server`). Shares
          # `cargoArtifacts`; the separate layer it once needed for utoipa-swagger-ui's
          # path-embedding build script went away with that crate.
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
          # `server` links /nix/store paths (glibc interpreter on Linux, libiconv
          # on macOS), so it runs only where that store exists. These fix that. Built
          # natively per arch, so aws-lc-sys' C build never cross-compiles.

          # Rust target triple; also keys the release tarball name.
          releaseTarget =
            let
              arch = pkgs.stdenv.hostPlatform.parsed.cpu.name; # x86_64 | aarch64
            in
            if pkgs.stdenv.isDarwin then "${arch}-apple-darwin" else "${arch}-unknown-linux-musl";

          # Linux: static musl (`crt-static`), no ELF interpreter or runtime libc.
          serverStatic =
            let
              # rust-toolchain.toml plus the musl std.
              muslToolchain = p: (rustToolchain p).override { targets = [ releaseTarget ]; };
              craneLibMusl = (crane.mkLib pkgs).overrideToolchain muslToolchain;

              # Rust bundles musl libc.a; this C toolchain is only for aws-lc-sys.
              cc = pkgs.pkgsStatic.stdenv.cc;
              ccBin = "${cc}/bin/${cc.targetPrefix}cc";

              # cargo wants the triple upper-cased with underscores; `cc` takes it verbatim.
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
                # Build scripts and proc macros keep the native compiler.
                HOST_CC = "${pkgs.stdenv.cc.nativePrefix}cc";

                depsBuildBuild = [ cc ];
              };
            in
            craneLibMusl.buildPackage (
              muslArgs
              // {
                # Different target, so its own dependency layer.
                cargoArtifacts = craneLibMusl.buildDepsOnly muslArgs;
                doCheck = false;
                meta.mainProgram = "sismatic-server";
                # Same embedded frontend as `.#server`.
                env = uiEnv;
              }
            );

          # macOS: repoint nixpkgs' libiconv to the system copy, then re-sign ad hoc
          # (arm64 refuses to exec a modified, unsigned Mach-O).
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

                # `otool -L` line 1 is the binary's own path; skip it.
                loadCommands() {
                  otool -L $out/bin/sismatic-server | tail -n +2
                }

                for dep in $(loadCommands | awk '/\/nix\/store\// { print $1 }'); do
                  install_name_tool -change "$dep" "/usr/lib/$(basename "$dep")" \
                    $out/bin/sismatic-server
                done
                codesign --force --sign - $out/bin/sismatic-server

                # Fail rather than ship a store-dependent binary.
                if loadCommands | grep -q /nix/store/; then
                  echo "error: binary still links against /nix/store:" >&2
                  loadCommands >&2
                  exit 1
                fi
              '';

          serverPortable = if pkgs.stdenv.isDarwin then serverPortableDarwin else serverStatic;

          # Release tarball + checksum, byte-identical locally and in CI.
          serverRelease = pkgs.runCommand "sismatic-server-release-${version}-${releaseTarget}" { } ''
            name="sismatic-server-${version}-${releaseTarget}"
            mkdir -p "$name" "$out"
            cp ${serverPortable}/bin/sismatic-server "$name/"
            cp ${./LICENSE} "$name/LICENSE"
            cp ${./README.md} "$name/README.md"
            chmod -R u+w "$name"

            # Reproducible: sorted, fixed mtime, no owners, no gzip timestamp.
            tar --sort=name --mtime='@1' --owner=0 --group=0 --numeric-owner \
              -cf - "$name" | gzip -9n > "$out/$name.tar.gz"
            ( cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )
          '';

          # Data the pipeline enumerates; `legacyPackages` allows plain attrsets.
          legacyPackages.ci = {
            inherit containerArches;
            registries = lib.mapAttrsToList (name: r: {
              inherit name;
              inherit (r) usernameVar tokenSecret;
              # "" rather than null: GitHub expressions have no null literal, so the
              # workflow tests `!= ''`.
              docsTokenSecret = if r.docs == null then "" else r.docs.tokenSecret;
            }) containerRegistries;
          };

          # OCI arch name, or null (not throw) so other systems still evaluate.
          containerArch =
            {
              x86_64 = "amd64";
              aarch64 = "arm64";
            }
            .${pkgs.stdenv.hostPlatform.parsed.cpu.name} or null;

          # Linux only (musl image), indexed arches only.
          containerSupported = pkgs.stdenv.isLinux && containerArch != null;

          # distroless' `nonroot`. Not 65534/nobody, the overflow uid that squashed
          # volume owners map onto.
          containerUid = 65532;

          # Concrete ref on the primary registry, so the manifests apply as-is.
          containerRepo = registry: "${registry.host}/${registry.namespace}/${containerName}";
          containerRef = "${containerRepo containerRegistries.${containerPrimaryRegistry}}:${version}";

          containerConfigFile = ./crates/sismatic-server/container_configuration.yaml;
          containerDevicesFile = ./crates/sismatic-server/container_devices.yaml;

          # Shipped under /etc/sismatic. Committed in the crate so
          # `shipped_config_check` loads both in `cargo test`.
          containerEtc = pkgs.runCommand "sismatic-container-etc" { } ''
            install -Dm444 ${containerConfigFile} "$out/etc/sismatic/configuration.yaml"
            install -Dm444 ${containerDevicesFile} "$out/etc/sismatic/devices.yaml"
          '';

          containerImage = pkgs.dockerTools.buildLayeredImage {
            name = containerName;
            # Arch-suffixed: one leg of the index `publish-<name>` writes.
            tag = "${version}-${toString containerArch}";
            architecture = containerArch;
            # Uncompressed: the format docker-archive is specified against; the
            # registry compresses layers on push anyway.
            compressor = "none";
            # `created` stays at epoch for reproducibility; the version is in tag and label.

            contents = [
              serverStatic
              containerEtc
              # /etc/passwd, /etc/group, and nsswitch.conf (`hosts: files dns`).
              (pkgs.dockerTools.fakeNss.override {
                extraPasswdLines = [
                  "sismatic:x:${toString containerUid}:${toString containerUid}:sismatic:/var/lib/sismatic:/noshell"
                ];
                extraGroupLines = [ "sismatic:x:${toString containerUid}:" ];
              })
            ];

            extraCommands = ''
              # Writable /tmp for a read-only root (the Deployment mounts an emptyDir).
              mkdir -p tmp
              chmod 1777 tmp

              # Root-owned mount point: `runtime_config_path` needs a persistent volume,
              # and a writable baked dir would silently lose data at reschedule.
              mkdir -p var/lib/sismatic
            '';

            config = {
              Entrypoint = [ "/bin/sismatic-server" ];
              # Selects which config file to load, so it cannot be a config key
              # (see `CONFIG_PATH_ENV`).
              Env = [ "SISMATIC_SERVER__CONFIG=/etc/sismatic/configuration.yaml" ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              User = "${toString containerUid}:${toString containerUid}";
              WorkingDir = "/var/lib/sismatic";
              Labels = {
                "org.opencontainers.image.title" = containerName;
                "org.opencontainers.image.description" = containerDescription;
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
          # Generated from `sismatic-server --env-schema` (see `sismatic_server::schema`),
          # so a new `RawServerConfig` key needs no edit here.

          # jq: rustdoc field description -> one plain line (unwrapped, delinked).
          # Shared by both generators so they agree.
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

          # Example k8s manifests, built so they cannot go stale. Heredocs are
          # quoted so the shell leaves `$` and backticks alone.
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

                # All commented out: a set variable would override configuration.yaml.
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

                # Placeholders, so exactly these two substitutions happen.
                substituteInPlace "$out/deployment.yaml" \
                  --replace-fail '@IMAGE@' '${containerRef}' \
                  --replace-fail '@UID@' '${toString containerUid}'

                # Syntax check only; kubeconform needs network the sandbox lacks.
                for manifest in "$out"/*.yaml; do
                  yq --exit-status 'true' "$manifest" > /dev/null \
                    || { echo "error: generated $manifest is not valid YAML" >&2; exit 1; }
                done
              '';

          #------------------------------------------------------------------#
          #                       Publishing the image                        #
          #------------------------------------------------------------------#
          # One app per registry: check archives, log in, push each arch leg, then
          # write the index, all in one job holding one registry's token. They read
          # archives off disk instead of building `containerImage`, so one x86_64
          # runner can push every leg. skopeo writes the auth file at
          # REGISTRY_AUTH_FILE; regctl reads it via DOCKER_CONFIG.

          # Archive name shared by `export-container` and the publish apps.
          containerArchiveName = arch: "sismatic-${arch}.tar";

          # skopeo needs a policy, and its compiled-in /etc/containers path exists in
          # no sandbox. Permissive: there is nothing to verify.
          skopeoPolicy = "${pkgs.skopeo}/etc/containers/default-policy.json";

          # Abort before reading credentials unless every leg is present.
          checkArchives = lib.concatMapStringsSep "\n" (arch: ''
            if [ ! -f "$archives/${containerArchiveName arch}" ]; then
              echo "error: no ${arch} archive at $archives/${containerArchiveName arch}" >&2
              exit 1
            fi
          '') containerArches;

          # `nix run .#export-container -- <dir>`: copy this machine's leg into <dir>.
          exportContainer = pkgs.writeShellApplication {
            name = "sismatic-export-container";
            text = ''
              dir=''${1:?usage: sismatic-export-container <dir>}
              mkdir -p "$dir"
              install -m644 ${containerImage} "$dir/${containerArchiveName containerArch}"
              echo "$dir/${containerArchiveName containerArch}"
            '';
          };

          # `nix run .#publish-<name> -- <dir> [tag...]` (default: version, latest).
          # Reads generic REGISTRY_USERNAME/REGISTRY_TOKEN so every CI job is alike.
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

                # Private auth dir, removed on exit.
                auth=$(mktemp -d)
                trap 'rm -rf "$auth"' EXIT
                export REGISTRY_AUTH_FILE="$auth/config.json" DOCKER_CONFIG="$auth"
                printf '%s' "$REGISTRY_TOKEN" \
                  | skopeo login "${registry.host}" --username "$REGISTRY_USERNAME" --password-stdin

                repo=${containerRepo registry}

                ${lib.concatMapStringsSep "\n" (arch: ''
                  echo "==> $repo:${version}-${arch}"
                  # OCI, to match the OCI index; the conversion is deterministic, so
                  # re-runs are no-ops.
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

          # `nix run .#publish-all -- <dir> [tag...]`: every registry from one
          # machine, via each registry's app. Checks all archives and credentials up
          # front; then continues past failures and lists them at the end.
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

          #------------------------------------------------------------------#
          #              Publishing the registry's own page                   #
          #------------------------------------------------------------------#
          # Overview pages, generated for `registriesWithDocs` only (GHCR has none).

          # One registry's page: docs/container-docs.md rendered for this version.
          # Nothing reviews it before it is live, so fail on unsubstituted
          # placeholders, relative links, and pages over the destination's limit.
          containerOverview =
            name: registry:
            pkgs.runCommand "sismatic-container-docs-${name}.md" { } ''
              # --replace-fail: a placeholder missing from the template is an error.
              substitute ${./docs/container-docs.md} spliced.md \
                --replace-fail '@@REPO@@' '${containerRepo registry}' \
                --replace-fail '@@VERSION@@' '${version}' \
                --replace-fail '@@DESCRIPTION@@' '${containerDescription}'

              # Inline the env table at its placeholder.
              awk -v table=${envReference} '
                $0 == "@@ENV_REFERENCE@@" {
                  while ((getline line < table) > 0) print line
                  next
                }
                { print }
              ' spliced.md > commented.md

              # Strip HTML comments (registry editors show them as text). Whole-line
              # only: `<!--` through the next line ending `-->`.
              awk '
                /^[[:space:]]*<!--/ { stripping = 1 }
                stripping { if ($0 ~ /-->[[:space:]]*$/) stripping = 0; next }
                # Collapse blank runs; drop leading and trailing ones.
                NF == 0 { if (started) blank = 1; next }
                { if (blank) print ""; blank = 0; started = 1; print }
              ' commented.md > "$out"

              if grep -n '@@' "$out"; then
                echo "error: unsubstituted placeholder in the ${name} overview (above)" >&2
                exit 1
              fi

              # Links must be absolute (fragments are fine): the page is not served from the repo.
              relative=$(grep -oE '\]\([^)]*\)' "$out" | grep -vE '^\]\((https?://|#)' || true)
              if [ -n "$relative" ]; then
                echo "error: relative links in the ${name} overview: $relative" >&2
                exit 1
              fi
              ${lib.optionalString (registry.docs.maxChars != null) ''
                # Bytes overcount em dashes against a char limit, erring strict.
                bytes=$(wc -c < "$out")
                if [ "$bytes" -gt ${toString registry.docs.maxChars} ]; then
                  echo "error: the ${name} overview is $bytes bytes; ${registry.host} takes ${toString registry.docs.maxChars}" >&2
                  exit 1
                fi
              ''}
            '';

          # `nix build .#container-docs`: one page per registry, readable before release.
          containerOverviews = pkgs.runCommand "sismatic-container-docss" { } ''
            mkdir -p "$out"
            ${lib.concatStrings (
              lib.mapAttrsToList (name: r: ''
                install -m444 ${containerOverview name r} "$out/${name}.md"
              '') registriesWithDocs
            )}
          '';

          # Env a page's app needs and its CI source; shared by the app's guard and
          # `publish-docs-all`.
          docsCredentials =
            registry:
            [
              {
                var = "REGISTRY_DOCS_TOKEN";
                from = registry.docs.tokenSecret;
                kind = "secret";
              }
            ]
            # Docker Hub trades username + token for a JWT; Quay's token stands alone.
            ++ lib.optional (registry.docs.kind == "dockerhub") {
              var = "REGISTRY_USERNAME";
              from = registry.usernameVar;
              kind = "variable";
            };

          # `nix run .#publish-docs-<name>` (no args: the page is built in).
          # Reads generic REGISTRY_DOCS_TOKEN (+ REGISTRY_USERNAME for Docker Hub).
          # Must follow `publish-<name>` (the repository must exist), so CI keeps both
          # in one job. Idempotent. Tokens never reach argv: bodies go on stdin and
          # auth headers through a curl config in a private temp dir.
          publishDocsTo =
            name: registry:
            let
              overview = containerOverview name registry;

              call =
                {
                  dockerhub = ''
                    auth=$(mktemp -d)
                    trap 'rm -rf "$auth"' EXIT

                    # Login yields a JWT; see `containerRegistries.dockerhub` for the token scope.
                    jwt=$(jq -n --arg u "$REGISTRY_USERNAME" --arg p "$REGISTRY_DOCS_TOKEN" \
                            '{username: $u, password: $p}' \
                          | curl -fsS --retry 3 -X POST --data @- \
                              -H 'Content-Type: application/json' \
                              https://hub.docker.com/v2/users/login/ \
                          | jq -er .token)
                    printf 'header = "Authorization: JWT %s"\n' "$jwt" > "$auth/curlrc"

                    # PATCH touches only the two description fields.
                    jq -n --arg d ${lib.escapeShellArg containerDescription} --rawfile f ${overview} \
                        '{description: $d, full_description: $f}' \
                      | curl -fsS --retry 3 -K "$auth/curlrc" -X PATCH --data @- \
                          -H 'Content-Type: application/json' \
                          -o /dev/null -w 'HTTP %{http_code}\n' \
                          'https://hub.docker.com/v2/repositories/${registry.namespace}/${containerName}/'
                  '';
                  quay = ''
                    # Unused while `containerRegistries.quay.docs` is null; kept for re-enabling.
                    auth=$(mktemp -d)
                    trap 'rm -rf "$auth"' EXIT
                    printf 'header = "Authorization: Bearer %s"\n' "$REGISTRY_DOCS_TOKEN" \
                      > "$auth/curlrc"

                    # One field; the overview's first line carries the short description.
                    jq -n --rawfile f ${overview} '{description: $f}' \
                      | curl -fsS --retry 3 -K "$auth/curlrc" -X PUT --data @- \
                          -H 'Content-Type: application/json' \
                          -o /dev/null -w 'HTTP %{http_code}\n' \
                          'https://quay.io/api/v1/repository/${registry.namespace}/${containerName}'
                  '';
                }
                .${registry.docs.kind};
            in
            pkgs.writeShellApplication {
              name = "sismatic-publish-docs-${name}";
              runtimeInputs = [
                pkgs.curl
                pkgs.jq
              ];
              text = ''
                if [ "$#" -gt 0 ]; then
                  echo "usage: sismatic-publish-docs-${name}   (no arguments: the page is built in)" >&2
                  echo "       read the page it would publish with: nix build .#container-docs" >&2
                  exit 1
                fi

                ${lib.concatMapStrings (c: ''
                  : "''${${c.var}:?${name} needs ${c.var} (the ${c.from} ${c.kind} in CI)}"
                '') (docsCredentials registry)}

                echo "==> ${containerRepo registry}: overview for ${version}"
                ${call}
              '';
            };

          # `nix run .#publish-docs-all`: counterpart to `publish-all`, also for
          # fixing a page without a release. Same up-front check, same keep-going.
          publishDocsAll = pkgs.writeShellApplication {
            name = "sismatic-publish-docs-all";
            text = ''
              missing=()
              ${lib.concatStrings (
                lib.mapAttrsToList (
                  _: r:
                  lib.concatMapStrings (c: ''
                    [ -n "''${${c.from}:-}" ] || missing+=(${c.from})
                  '') (docsCredentials r)
                ) registriesWithDocs
              )}
              if [ ''${#missing[@]} -gt 0 ]; then
                echo "error: set these before publishing any overview: ''${missing[*]}" >&2
                exit 1
              fi

              failed=()
              ${lib.concatStrings (
                lib.mapAttrsToList (name: r: ''
                  echo "==> ${name}"
                  ${lib.concatMapStringsSep " " (c: ''${c.var}="''$${c.from}"'') (docsCredentials r)} \
                    ${lib.getExe (publishDocsTo name r)} \
                    || failed+=(${name})
                '') registriesWithDocs
              )}

              if [ ''${#failed[@]} -gt 0 ]; then
                echo "error: publishing the overview failed for: ''${failed[*]}" >&2
                echo "       re-run each with: nix run .#publish-docs-<name>" >&2
                exit 1
              fi
            '';
          };

          # Asserts the built image's config: non-root User (`runAsNonRoot`), the
          # SISMATIC_SERVER__CONFIG path, the port the Service targets, arch, version.
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

          # Wheel source: cargo sources, python/, pyproject.toml, README.md, LICENSE.
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

          # Hermetic wheel (nixpkgs Python and glibc): reproducible, not portable.
          # Use the `build-wheel` app for a distributable one.
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

            # cmake is only for aws-lc-sys; the root has no CMakeLists.txt.
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

          # `nix run .#build-wheel [-- <maturin args>]`: distributable wheel in
          # ./dist, built against the host Python (impure, hence an app). Linux links
          # via `zig cc` for manylinux_2_28; macOS builds natively.
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

          # MkDocs (Material) + mkdocstrings' Python handler.
          docsEnv = pkgs.python3.withPackages (ps: [
            ps.mkdocs
            ps.mkdocs-material
            ps.mkdocstrings
            ps.mkdocstrings-python
            # mkdocstrings uses it to pretty-print the rendered signatures.
            ps.black
          ]);

          # `nix run .#docs [-- serve]`: build into ./site or serve. Reads the
          # committed `__init__.pyi` stub statically; no compiled extension needed.
          docs = pkgs.writeShellApplication {
            name = "sismatic-docs";
            runtimeInputs = [ docsEnv ];
            text = ''
              # Generated env table, included by docs/deployment.md (`--8<--`).
              # Gitignored; mkdocs already requires cwd to be the repo root.
              install -m644 ${envReference} docs/env-reference.md

              if [ "$#" -eq 0 ]; then
                exec mkdocs build
              else
                exec mkdocs "$@"
              fi
            '';
          };

          # Node for http-ui. nixpkgs attrs carry only the major; flake.lock fixes
          # the exact version, and this guard turns a silent minor bump into an eval
          # error. Vite needs >= 20.19 / >= 22.12.
          #
          # Not nodejs_24: 24.16.0 miscounts `trackUnmanagedFds` and closes libuv's
          # async pipe on worker exit, so `pnpm add` aborts (exit 134) after writing
          # the lockfile. 22.23.1 and 26.4.0 are clean. Revisit on a later 24 patch.
          nodeVersion = "26.4.0";
          nodejs =
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

          # pnpm, pinned like Node. Couplings:
          # 1. engines.node: recorded in pnpmRequiresNode, asserted below. Re-read with:
          #      nix eval --raw --impure --expr '(builtins.fromJSON (builtins.readFile "${(builtins.getFlake (toString ./.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem}.pnpm_11}/libexec/pnpm/package.json")).engines.node'
          # 2. nixpkgs bakes pnpm's interpreter into its shebang (nodejs-slim_24 here,
          #    the aborting build), so the override pins it to nodejs-slim of
          #    nodeVersion's major. Move the two together.
          # 3. pnpm 11 may download another pnpm/Node per `packageManager` or
          #    `devEngines`. Only `pmOnFail: ignore` in pnpm-workspace.yaml (or
          #    `--pm-on-fail=ignore`) stops it; the shellHook warns when it is missing.
          # pnpm_9 is marked insecure.
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
              # nodejs-slim is the arg to override: it is the shebang interpreter.
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
          # Frontend dist, embedded by sismatic-http-api's `ui` feature; built here
          # instead of committing dist/ (node_modules/ and dist/ are gitignored, so
          # absent from src). It lives inside the crate because release-plz assigns
          # commits to packages by directory.
          httpUi = pkgs.stdenv.mkDerivation (finalAttrs: {
            pname = "sismatic-http-ui";
            inherit version;
            src = ./crates/sismatic-http-api/http-ui;

            # Offline pnpm store (fixed-output: the only network access). After
            # `pnpm install`, set `hash = lib.fakeHash;`, build, and take the reported
            # hash. Uses the pinned pnpm, since resolving runs pnpm.
            pnpmDeps = (pkgs.fetchPnpmDeps.override { inherit pnpm; }) {
              inherit (finalAttrs) pname version src;
              fetcherVersion = 3;
              hash = "sha256-vQgFcvqyrm8wtgxPn5I02TKDYVwr/y1rC/ntczFd/EA=";
            };

            # `pnpmConfigHook` finds the pinned pnpm on PATH and installs offline.
            nativeBuildInputs = [
              nodejs
              pnpm
              pkgs.pnpmConfigHook
            ];

            # `pnpm build` is `tsc -b && vite build`, so this also type-checks.
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

          # For every build that compiles sismatic-http-api's `ui` feature. Kept out
          # of `commonArgs` so UI edits cannot invalidate `cargoArtifacts`. Builds
          # without it still work (see `dist_dir` in that crate's build.rs).
          uiEnv = {
            SISMATIC_HTTP_UI_DIST = "${httpUi}";
          };

          # Bound locally so the devShell can use it without self.checks.
          checks = {
            # Building is the check. Not `serverRelease`: it needs the musl
            # toolchain, and CI's release matrix builds it.
            inherit cli server;

            # Fails on frontend type errors.
            http-ui = httpUi;

            # Fails when http-ui's `src/api/schema.d.ts` (regenerated by hand with
            # `pnpm gen:api`) drifts from the server's OpenAPI; 5f73245 shipped such
            # drift and crashed the fleet page. Uses `--openapi` (no socket) and the
            # lockfile's openapi-typescript.
            http-ui-schema = httpUi.overrideAttrs {
              name = "sismatic-http-ui-schema-${version}";
              buildPhase = ''
                runHook preBuild
                ${lib.getExe server} --openapi > openapi.json
                pnpm exec openapi-typescript openapi.json -o generated.d.ts
                if ! diff -u src/api/schema.d.ts generated.d.ts; then
                  echo >&2
                  echo "error: crates/sismatic-http-api/http-ui/src/api/schema.d.ts does not match the" >&2
                  echo "OpenAPI document this server serves. Regenerate it:" >&2
                  echo "  cd crates/sismatic-http-api/http-ui && pnpm gen:api" >&2
                  exit 1
                fi
                runHook postBuild
              '';
              installPhase = ''
                runHook preInstall
                touch $out
                runHook postInstall
              '';
            };

            # Separate derivation, so consumers can build without the lint gate.
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
                # Lint the real `ui` embed path, not the placeholder.
                env = uiEnv;
              }
            );

            # rustdoc must build cleanly; `testing` makes the linked `fake` module resolve.
            doc = craneLib.cargoDoc (
              commonArgs
              // {
                inherit cargoArtifacts;
                # `--features` is not allowed at a virtual-workspace root.
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

            # Registry pages with `containerOverview`'s assertions. Cheap, unlike
            # `container-config-check`: needs only the native `server`.
            container-docs = containerOverviews;

            # Guards release-plz's `git_only` mode (Actions run 32251398251). The
            # workspace tag template `v{{ version }}` matches every member, so
            # release-plz packages each one at the last tag and fails with `Failed to
            # find package` for a crate added since; only the release PR moves the tag,
            # so main stays wedged. `release = false` does not exempt packaging. Scoping
            # a released crate instead regenerates its changelog from all history. So:
            #     release = false  <=>  git_tag_name contains {{ package }}
            # Both sides are static; the sandbox has no .git.
            release-plz-config =
              let
                cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
                releasePlz = builtins.fromTOML (builtins.readFile ./release-plz.toml);

                # Dir basename may differ from the package name.
                memberNames = map (
                  m: (builtins.fromTOML (builtins.readFile (./. + "/${m}/Cargo.toml"))).package.name
                ) cargoToml.workspace.members;

                entries = releasePlz.package or [ ];
                entryNames = map (p: p.name) entries;
                missing = lib.subtractLists entryNames memberNames; # members with no entry
                stale = lib.subtractLists memberNames entryNames; # entries with no member

                # Member override, else the workspace default.
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

                # Both halves are vacuous while all members are released; keep them for new crates.
                #
                # Half one: unreleased and unscoped (the intent-relay bug).
                unscopedUnreleased = map (p: p.name) (
                  builtins.filter (p: !(isReleased p) && !(isScoped p)) entries
                );
                # Half two: released and scoped (loses its changelog baseline).
                scopedReleased = map (p: p.name) (builtins.filter (p: isReleased p && isScoped p) entries);

                # Pinned set: a promoted crate must exist at the last tag, which only a
                # human can check. A new member ships once as `release = false` + scoped,
                # then joins this list.
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

            # `cargo package --workspace` (release-plz git_only) rejects a path
            # dependency with no version (regressed in 0d16892). Assert every internal
            # path dependency has a requirement the workspace version satisfies.
            internal-dep-versions =
              let
                cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
                inherit (cargoToml.workspace.package) version;

                # [ major minor patch ]; a prerelease suffix is dropped (never used).
                triple = v: map lib.toInt (lib.splitString "." (lib.head (lib.splitString "-" v)));
                workspace = triple version;

                # "^0.2" -> [ 0 2 ]. Caret form only; anything else is null (unsupported).
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

                # All dependency tables, including target-scoped ones.
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

                # Internal iff declared with `path`; `.workspace = true` is checked at the root.
                depsIn =
                  file: table:
                  lib.mapAttrsToList (name: dep: {
                    inherit file name;
                    req = dep.version or null;
                  }) (lib.filterAttrs (_: dep: builtins.isAttrs dep && dep ? path) table);

                memberManifest = m: builtins.fromTOML (builtins.readFile (./. + "/${m}/Cargo.toml"));

                # A member's dev-dependency on itself, under any table or rename.
                isSelf = m: name: dep: (if builtins.isAttrs dep then dep.package or name else name) == (memberManifest m).package.name;

                deps =
                  depsIn "Cargo.toml" (cargoToml.workspace.dependencies or { })
                  ++ lib.concatMap (
                    m:
                    lib.concatMap (t: depsIn "${m}/Cargo.toml" (lib.filterAttrs (n: d: !(isSelf m n d)) t)) (
                      depTables (memberManifest m)
                    )
                  ) cargoToml.workspace.members;

                # Self dev-dependencies are the inverse case: they must be path-only.
                # `cargo package` strips a versionless path dev-dependency, but keeps a
                # versioned one (explicit, or inherited via `workspace = true`) and
                # resolves it against the registry before the crate being packaged
                # exists there — "no matching package named <self> found". That broke
                # release-plz on the v0.9.0 baseline.
                badSelf = lib.concatMap (
                  m:
                  lib.concatMap (
                    t:
                    lib.mapAttrsToList (name: _: "  ${m}/Cargo.toml: ${name}") (
                      lib.filterAttrs (
                        n: d: isSelf m n d && !(builtins.isAttrs d && d ? path && !(d ? version) && !(d.workspace or false))
                      ) t
                    )
                  ) (depTables (memberManifest m))
                ) cargoToml.workspace.members;

                # `*` is unbounded, so always satisfied.
                ok = d: d.req == "*" || (d.req != null && reqParts d.req != null && satisfies (reqParts d.req));

                offenders = lib.filter (d: !(ok d)) deps;
                describe = d: "  ${d.file}: ${d.name} -> ${if d.req == null then "(no version field)" else d.req}";

                # Satisfied now is not enough: the exact pin `0.2.25` broke the first
                # release crossing into 0.3.0 (`failed to select a version for the
                # requirement`). Require `0`, `^0` or `*`, which survive every bump below 1.0.
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
              assert lib.assertMsg (badSelf == [ ]) ''
                Self dev-dependencies that carry a version:
                ${lib.concatStringsSep "\n" badSelf}
                A crate's dependency on itself must be path-only, e.g.
                  <crate> = { path = ".", features = [ ... ] }
                not `workspace = true` and with no `version`. Otherwise `cargo package`
                keeps it, looks for the crate on crates.io before it exists there, and
                release-plz's release-pr job fails on every push for as long as the
                offending manifest is the latest `v*` tag.
              '';
              pkgs.runCommand "internal-dep-versions-ok" { } "touch $out";

            # Security advisories against the pinned advisory-db input.
            # Update with: nix flake update advisory-db
            audit = craneLib.cargoAudit {
              inherit src advisory-db;
              # RUSTSEC-2023-0071 (rsa Marvin timing side-channel) has no fixed
              # release; russh >=0.60.3 requires rsa 0.10.0-rc, and the bump is
              # needed for RUSTSEC-2026-0154. Re-evaluate when rsa ships a fix.
              cargoAuditExtraArgs = "--ignore yanked --ignore RUSTSEC-2023-0071";
            };

            # License, ban and source policy (deny.toml).
            deny = craneLib.cargoDeny {
              inherit src;
            };

            # Tests via cargo-nextest.
            nextest = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                partitions = 1;
                partitionType = "count";
                # Don't fail if a crate has no tests yet.
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
                # `tests/ui.rs` and `ui::tests` need the real frontend.
                env = uiEnv;
              }
            );
          }
          # Coverage; tarpaulin is Linux-only.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            coverage = craneLib.cargoTarpaulin (
              commonArgs
              // {
                inherit cargoArtifacts;
                # Same frontend as `nextest`.
                env = uiEnv;
                # `--engine llvm`: the default ptrace engine miscomputes on crane's
                # `--release` builds (breakpoints land mid-instruction; seen in
                # `openapi::tests::the_page_carries_the_bundle_path_and_the_configuration`).
                # The rest is crane's default tail, which this attribute replaces.
                cargoTarpaulinExtraArgs = "--engine llvm --skip-clean --out xml --output-dir $out";
              }
            );
          }
          // {
            # Self-validating manifests; cheap (native `server`, no musl).
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

            # The frontend dist `.#server` embeds.
            http-ui = httpUi;

            # The dependency layer alone, to pre-populate a store or cache.
            deps = cargoArtifacts;

            # `nix build .#server-portable` -> a sismatic-server that runs on
            # a machine with no Nix; `.#server-release` -> that binary packed
            # as sismatic-server-<version>-<target>.tar.gz plus its SHA-256.
            server-portable = serverPortable;
            server-release = serverRelease;

            # Env surface read off `RawServerConfig`; available on every system.
            env-schema = envSchema;
            env-reference = envReference;

            # `nix build .#container-docs` -> result/<registry>.md. Ungated: needs no image.
            container-docs = containerOverviews;

            # `nix build .#k8s` -> manifests for this version. Ungated: needs no image.
            k8s = k8sManifests;
          }
          # Image outputs: Linux on the indexed arches only; absent elsewhere so
          # `nix flake check` covers what the host can build.
          // lib.optionalAttrs containerSupported {
            # `nix build .#container` -> docker-archive (`docker load < result`).
            # Publish with `export-container` + `publish-all`.
            container = containerImage;

            # Image invariants. A package, not a check: it needs the musl toolchain,
            # which CI's container job already pays for.
            container-config-check = containerConfigCheck;
          };

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
            # Pipeline steps, identical locally and in CI.
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
          # Ungated: these push prebuilt archives, so any machine can publish.
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
          # Only for `registriesWithDocs`.
          // lib.mapAttrs' (
            name: r:
            lib.nameValuePair "publish-docs-${name}" {
              type = "app";
              program = lib.getExe (publishDocsTo name r);
            }
          ) registriesWithDocs
          // {
            publish-docs-all = {
              type = "app";
              program = lib.getExe publishDocsAll;
            };
          }
          # Needs the image, so gated.
          // lib.optionalAttrs containerSupported {
            export-container = {
              type = "app";
              program = lib.getExe exportContainer;
            };
          };

          # `nix develop`: check inputs plus the pinned toolchain. Linking: x86_64
          # Linux uses rustc's bundled rust-lld; macOS uses LLVM lld (below).
          devShells.default = craneLib.devShell (
            {
              inherit checks;

              packages = [
                pkgs.cargo-nextest
                pkgs.cargo-deny
                pkgs.cargo-watch # or pkgs.bacon (maintained successor)
                # `cargo expand` needs nightly (via +nightly); stable stays the default.
                pkgs.cargo-expand
                (pkgs.rust-bin.selectLatestNightlyWith (t: t.minimal))
                # Python packaging; cmake/perl for aws-lc-sys.
                pkgs.maturin
                pkgs.python3
                pkgs.cmake
                pkgs.perl
                # Doc site (same env as `nix run .#docs`).
                docsEnv
                # Frontend toolchain for http-ui.
                nodejs
                pnpm
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
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

                # Venv managed here so direnv's `use flake` captures its PATH too;
                # anchored to the repo root.
                SIS_VENV="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")/sis-venv"
                [ -d "$SIS_VENV" ] || python3 -m venv "$SIS_VENV"
                export VIRTUAL_ENV="$SIS_VENV"
                export PATH="$SIS_VENV/bin:$PATH"

                # Warn when pnpm could swap itself out (see the pnpm binding).
                SIS_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
                if [ -f "$SIS_ROOT/crates/sismatic-http-api/http-ui/package.json" ] \
                  && ! grep -qs pmOnFail "$SIS_ROOT/crates/sismatic-http-api/http-ui/pnpm-workspace.yaml"; then
                  echo "warning: crates/sismatic-http-api/http-ui/pnpm-workspace.yaml does not set 'pmOnFail: ignore', so a" >&2
                  echo "         packageManager field can override this flake's pnpm ${pnpmVersion}." >&2
                fi
              '';
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
              # Dev shell only, so build hashes stay independent of it.
              CARGO_BUILD_RUSTFLAGS = "-C link-arg=-fuse-ld=lld";
            }
          );
        };

      # Compute each system's outputs once, then transpose to field -> system.
      perSystem = fmapSystems perSystemOutputs;
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
