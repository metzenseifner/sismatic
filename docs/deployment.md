# Deployment

The server ships as a container image on three registries, published from the
same `flake.nix` that builds every other artifact. This page is about running
it on Kubernetes; for the image itself and for plain `docker run`, see
[Container image](https://github.com/metzenseifner/sismatic#container-image).

## The image

```
ghcr.io/metzenseifner/sismatic:<version>
quay.io/metzenseifner/sismatic:<version>
docker.io/metzenseifner/sismatic:<version>
```

Each tag is a multi-arch index over `linux/amd64` and `linux/arm64`. The three
are byte-identical: one archive is built per architecture and copied to all
three registries without conversion, so the digests agree.

| | |
| --- | --- |
| Entrypoint | `/bin/sismatic-server` |
| User | `65532:65532`, non-root |
| Port | `8080` |
| Config | `/etc/sismatic/configuration.yaml`, named by `SISMATIC_SERVER__CONFIG` |
| Fleet | `/etc/sismatic/devices.yaml`, empty as shipped |
| Health | `GET /health_check` |
| Writable paths | none required |

There is no base image. The content is the statically linked musl binary, two
config files and an `/etc/passwd` — no shell, no package manager, no CA bundle,
because nothing in the server opens an outbound TLS connection (devices are
reached over SSH, and their host keys come from the devices file). A
`kubectl exec` into the pod finds nothing to run.

## Generating the manifests

```console
$ nix build .#k8s && ls result/
configmap.yaml  deployment.yaml  env-configmap.yaml  service.yaml
```

They are generated rather than committed, so they name the version you built
and cannot drift from the image they describe. `env-configmap.yaml` in
particular is derived from the config type itself, so it lists every setting
the binary accepts and nothing it does not.

## Three things worth knowing before you apply them

### It is a singleton

`replicas: 1` and `strategy: Recreate`, and both are properties of the server
rather than placeholders.

Two replicas would each open their own SSH session to every device in the
fleet, doubling the connection count a recorder sees. Worse, each keeps its own
in-memory store, so the same query would return different history depending on
which pod the Service happened to route to. A rolling update briefly runs two,
which is why the strategy is `Recreate`.

### The memory limit must sit above the store's own cap

`SISMATIC_SERVER__STORE__MAX_MEMORY` defaults to 256 MiB. That is the store's
cap, enforced on every write — when it binds, the oldest reads are discarded.
The container's `resources.limits.memory` is the kernel's cap, and when *that*
binds the process is killed.

The first has to bind before the second, or the pod is OOM-killed instead of
expiring history. The generated Deployment leaves 384 Mi against the 256 MiB
default. Raise them together.

### The fleet carries credentials

`devices.yaml` holds the username and password for each device, necessarily —
the server cannot poll a device it cannot authenticate to. The generated
`configmap.yaml` puts it in a ConfigMap because the shipped fleet is empty.
Anything past a lab belongs in a Secret with the same two keys and a `secret:`
volume in place of the `configMap:` one.

## Configuring it

Three layers, most specific first, and every one of them optional:

1. the environment — `SISMATIC_SERVER__<SECTION>__<KEY>`,
2. the mounted `configuration.yaml`,
3. the binary's built-in defaults.

The environment wins at any key it names and is silent at every key it does
not, so a Deployment can override one setting without restating the document.

The variable name is derived from the key path: the prefix `SISMATIC_SERVER`,
then the path, joined by double underscores and upper-cased. `store.retain`
becomes `SISMATIC_SERVER__STORE__RETAIN`. Lists are comma-separated.

```yaml
env:
  - name: SISMATIC_SERVER__STORE__RETAIN
    value: "7d"
  - name: SISMATIC_SERVER__SYNC__INTERVAL_SECS
    value: "60"
  - name: SISMATIC_SERVER__SYNC__FIELDS
    value: "RUNNING_STATE,FIRMWARE"
```

A typo is not ignored. Every section is `deny_unknown_fields`, so
`SISMATIC_SERVER__STORE__RETIAN` fails startup naming the key rather than
quietly leaving the default in place.

### Every setting

`sismatic-server --env-schema` prints this list as JSON, and
`nix build .#env-reference` renders it as the table below. Both are derived
from the server's config type, so neither can fall behind the binary.

--8<-- "env-reference.md"

## Persisting a fleet edited through the API

`inventory.runtime_config_path` is unset by default, and that default is
load-bearing: with nothing persisted, the mounted devices file is
unambiguously authoritative and a restarted pod returns to exactly what it
says — which is what a declarative deployment already expects of every file it
mounts.

Setting it makes a fleet edited through `/v1/inventory` survive a restart, and
in a cluster that needs more than the key. The path has to land on a volume
that outlives the pod, or the state it exists to preserve is lost on the next
reschedule while still shadowing the devices file at startup. The server warns
on every startup that loads from it, naming both paths, and
`POST /v1/inventory/reset` makes the file authoritative again.

```yaml
env:
  - name: SISMATIC_SERVER__INVENTORY__RUNTIME_CONFIG_PATH
    value: /var/lib/sismatic/devices.state.yaml
volumeMounts:
  - name: state
    mountPath: /var/lib/sismatic
volumes:
  - name: state
    persistentVolumeClaim:
      claimName: sismatic-state
```

## Probes

`GET /health_check` answers 200 as soon as the process is routing requests, and
deliberately says nothing about whether devices are reachable. A liveness probe
that failed because a recorder was unplugged would restart a server that was
working perfectly — turning one fault into two. Which devices are reachable,
and how stale their last reads are, are real questions with real routes over
the store.
