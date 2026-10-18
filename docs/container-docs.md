<!--
The registry-facing page: what Docker Hub shows as a repository's
`full_description` and Quay as its `description`.

This file is a template, not a page. `nix build .#container-docs` renders
one copy per registry, substituting @@REPO@@, @@VERSION@@ and @@DESCRIPTION@@
and splicing the generated environment table in where @@ENV_REFERENCE@@ stands
alone on a line. mkdocs is told it is not a page by `not_in_nav` in mkdocs.yml.

Two rules the renderer enforces, because a registry page cannot be reviewed
before it is live:

  - Every link is absolute. A registry page is not served out of the
    repository, so `docs/deployment.md` and `./LICENSE` resolve to nothing
    there — and an image reference like `docs/fleet_overview.png` resolves to
    a broken image.
  - The whole document stays under the destination's size limit (Docker Hub
    truncates at 25,000 characters).

HTML comments that own their lines — this one, and the one the environment
table carries — are stripped from the rendered copy: they are addressed to
whoever edits the source, not to the page.
-->

# sismatic-server

@@DESCRIPTION@@

```console
$ docker run --rm -p 8080:8080 @@REPO@@:@@VERSION@@
```

That is the whole of it. The image carries an empty fleet, so the server comes
up healthy with nothing mounted and nothing configured:

```console
$ curl localhost:8080/health_check          # 200
$ curl localhost:8080/v1/inventory/devices  # []
```

`http://localhost:8080/` redirects to `/ui`, the bundled web frontend — the
fleet view, served by the same binary out of the same image.

## Tags

| Tag | What it is |
| --- | --- |
| `@@VERSION@@` | this release, immutable |
| `latest` | whatever the most recent release is |

Each tag is a multi-arch index over `linux/amd64` and `linux/arm64`. The same
image is published to three registries, and the digests agree — one archive is
built per architecture and copied to all three without conversion:

```
ghcr.io/metzenseifner/sismatic:@@VERSION@@
quay.io/metzenseifner/sismatic:@@VERSION@@
docker.io/metzenseifner/sismatic:@@VERSION@@
```

## What is in it

There is no base image and no Dockerfile. The content is a statically linked
musl binary, two config files and an `/etc/passwd` — no shell, no package
manager, no CA bundle, because nothing in the server opens an outbound TLS
connection (devices are reached over SSH, and their host keys come from the
devices file). A `docker exec` into the container finds nothing to run.

| | |
| --- | --- |
| Entrypoint | `/bin/sismatic-server` |
| User | `65532:65532`, non-root |
| Port | `8080` |
| Config | `/etc/sismatic/configuration.yaml`, named by `SISMATIC_SERVER__CONFIG` |
| Fleet | `/etc/sismatic/devices.yaml`, empty as shipped |
| Health | `GET /health_check` |
| Writable paths | none required |

It needs no writable filesystem, so `--read-only` is fine, as are
`runAsNonRoot: true` and a dropped capability set.

## Giving it a fleet

Two ways, and they compose:

```console
$ docker run --rm -p 8080:8080 \
    -v ./devices.yaml:/etc/sismatic/devices.yaml:ro \
    @@REPO@@:@@VERSION@@
```

```console
$ curl -X POST localhost:8080/v1/inventory/devices -H 'content-type: application/json' -d @device.json
```

A device's username and password live in the devices document, necessarily —
the server cannot poll a device it cannot authenticate to. In a cluster that
makes it a Secret rather than a ConfigMap.

Fleet edits made through the API are in memory only unless
`SISMATIC_SERVER__INVENTORY__RUNTIME_CONFIG_PATH` names a path on a volume
that outlives the container. Left unset — the default — the mounted devices
file is unambiguously authoritative and a restart returns to exactly what it
says.

## Configuring it

Three layers, most specific first, and every one of them optional:

1. the environment — `SISMATIC_SERVER__<SECTION>__<KEY>`,
2. the mounted `configuration.yaml`,
3. the binary's built-in defaults.

The environment wins at any key it names and is silent at every key it does
not, so one setting can be overridden without restating the document. That
makes the environment the whole of how this image is configured:

```console
$ docker run --rm -p 8080:8080 \
    -e SISMATIC_SERVER__STORE__RETAIN=7d \
    -e SISMATIC_SERVER__SYNC__INTERVAL_SECS=60 \
    -e SISMATIC_SERVER__SYNC__FIELDS=RUNNING_STATE,FIRMWARE \
    @@REPO@@:@@VERSION@@
```

The variable name is derived from the key path: the prefix `SISMATIC_SERVER`,
then the path, joined by double underscores and upper-cased. `store.retain`
becomes `SISMATIC_SERVER__STORE__RETAIN`. Lists are comma-separated.

A typo is not ignored. Every section refuses unknown keys, so
`SISMATIC_SERVER__STORE__RETIAN` fails startup naming the key rather than
quietly leaving the default in place.

### Every setting

Generated from the server's own config type, so this cannot fall behind the
binary in the image:

@@ENV_REFERENCE@@

## On Kubernetes

Example manifests are generated for the exact version you are running, rather
than committed and left to drift:

```console
$ nix build github:metzenseifner/sismatic/v@@VERSION@@#k8s && ls result/
configmap.yaml  deployment.yaml  env-configmap.yaml  service.yaml
```

Two things worth knowing before applying them. The server is a **singleton**:
two replicas would each open their own SSH session to every device and keep
their own in-memory store, so the same query would answer differently
depending on which pod answered it — hence `replicas: 1` and
`strategy: Recreate`. And the container's memory limit must sit **above** the
store's own cap (`SISMATIC_SERVER__STORE__MAX_MEMORY`, 256 MiB by default), or
the pod is OOM-killed instead of expiring history; the generated Deployment
leaves 384 Mi against that default, and the two are raised together.

The [deployment guide](https://metzenseifner.github.io/sismatic/deployment/)
has the rest: probes, persistence, and what each generated manifest is for.

## Elsewhere

- [Source and issues](https://github.com/metzenseifner/sismatic)
- [Documentation](https://metzenseifner.github.io/sismatic/)
- [Python SDK](https://pypi.org/project/sismatic/) — the same core, as a
  blocking Python facade
- Licensed [ECL-2.0](https://github.com/metzenseifner/sismatic/blob/main/LICENSE)
