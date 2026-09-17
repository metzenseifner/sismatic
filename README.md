<div align="center">

# sismatic

**A polling server, HTTP API, CLI, and Python SDK for [Extron](https://www.extron.com/) devices speaking the Simple Instruction Set (SIS) over SSH.**

[![Build & Release](https://github.com/metzenseifner/sismatic/actions/workflows/pipeline.yml/badge.svg)](https://github.com/metzenseifner/sismatic/actions/workflows/pipeline.yml)
[![License: ECL-2.0](https://img.shields.io/badge/license-ECL--2.0-blue.svg)](./LICENSE)
[![Rust 1.96](https://img.shields.io/badge/rust-1.96.0-orange.svg)](./rust-toolchain.toml)
[![PyPI](https://img.shields.io/pypi/v/sismatic.svg)](https://pypi.org/project/sismatic/)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/metzenseifner/sismatic)

</div>

`sismatic` is **SIS + automatic**: it handles the SIS machinery — connection pooling,
the SSH handshake, byte-level framing, reconnection — behind the scenes, so you
address a fleet of recorders by id and send plain instructions.

The primary way to run it is **[`sismatic-server`](#sismatic-server)**: a single static
binary that keeps warm SSH connections to every device, polls the fields you care
about on the schedule you set, stores what it reads, and serves the result over a
JSON HTTP API. Your dashboard talks to the server; the server talks to the devices.

```console
$ sismatic-server -c configuration.yaml
$ curl localhost:8080/v1/reads/devices/atrium-101/fields/RUNNING_STATE
{"device":"atrium-101","field":"RUNNING_STATE","value":{"type":"state","value":"started"},"at":"2026-09-17T09:14:03Z"}
```

---

## Table of contents

- [Why sismatic?](#why-sismatic)
- [sismatic-server](#sismatic-server)
  - [Quickstart](#quickstart)
  - [Installation](#installation)
- [Configuration](#configuration)
  - [Two documents, one process](#two-documents-one-process)
  - [Server configuration](#server-configuration)
  - [Devices configuration](#devices-configuration)
  - [Environment variables](#environment-variables)
  - [Precedence](#precedence)
  - [Changing configuration while it runs](#changing-configuration-while-it-runs)
- [HTTP API](#http-api)
- [Concepts](#concepts)
  - [Warm versus cold connections](#warm-versus-cold-connections)
  - [Device groups](#device-groups)
  - [Field vetoes](#field-vetoes)
  - [Retention and the memory budget](#retention-and-the-memory-budget)
- [Other ways to drive it](#other-ways-to-drive-it)
- [Architecture](#architecture)
- [Development](#development)
- [Documentation](#documentation)
- [License](#license)

## Why sismatic?

1. **A stable target.** SIS is a fixed, documented, unchanging protocol — the
   integration does not rot underneath you.
2. **Connections are somebody else's problem.** SSH handshakes to an SMP are
   expensive and its idle timer will drop you. `sismatic` keeps connections warm,
   re-dials what has gone cold, and shields a dead device from costing every
   caller a full timeout.
3. **No byte-level work.** Instructions and replies are typed values, not escape
   sequences.
4. **Devices are protected from your dashboard.** Reads are served from a store the
   poll loops populate, so an HTTP flood hits memory rather than a recorder.
5. **Reconfigurable in place.** Most settings — poll intervals, retention, the
   fleet itself — change over the API without a restart, which is what makes it
   deployable behind a ConfigMap.

## sismatic-server

The server starts three things on one Tokio runtime: a **sync** task set that
queries devices on a schedule, an **intent relay** that drains queued writes to
devices, and an **HTTP API** that serves what has been read.

```
                  ┌──────────────┐  poll    ┌─────────────┐
   HTTP clients ──▶│ sismatic-    │◀────────▶│  Extron SMP │
   (dashboards,    │   server     │  writes  │  devices    │
    scripts, k8s)  └──────┬───────┘          └─────────────┘
                          │ reads
                   ┌──────▼───────┐
                   │ store        │  latest + bounded history
                   └──────────────┘
```

### Quickstart

**1. Describe your devices** — `devices.yaml`:

```yaml
defaults:
  username: admin
  password: extron
  eager: true            # open connections up front and hold them warm

devices:
  - id: atrium-101
    host: 10.0.0.7
  - id: annex-far
    host: 10.0.0.8
    connect_secs: 10     # this one is far away; override just the timeout

groups:
  - id: room-5
    devices: ["atrium-101", "annex-far"]
```

**2. Describe the process** — `configuration.yaml`:

```yaml
inventory:
  config_path: devices.yaml   # relative to *this* file

sync:
  interval_secs: 30
  fields: ["*"]               # every field core can query

store:
  retain: 24h
  max_memory: 256MiB

http:
  host: 127.0.0.1
  port: 8080
```

**3. Run it:**

```console
$ sismatic-server -c configuration.yaml
```

**4. Use it:**

```console
$ curl localhost:8080/health_check

# read one field of one device, as of the last poll
$ curl localhost:8080/v1/reads/devices/atrium-101/fields/RUNNING_STATE

# that field over time
$ curl 'localhost:8080/v1/reads/devices/atrium-101/fields/RUNNING_STATE/history?limit=50'

# start every recorder in room 5, together
$ curl -X POST localhost:8080/v1/writes/groups/room-5/recording/start
```

Then open <http://localhost:8080/api> for the full interactive API reference.

### Installation

#### Prebuilt binaries

Every tagged release attaches a prebuilt `sismatic-server` for four targets, each
with a `.sha256` beside it:

| Target                       | Runs on                              |
| ---------------------------- | ------------------------------------ |
| `x86_64-unknown-linux-musl`  | any x86_64 Linux — statically linked  |
| `aarch64-unknown-linux-musl` | any arm64 Linux — statically linked   |
| `x86_64-apple-darwin`        | Intel macOS                          |
| `aarch64-apple-darwin`       | Apple-silicon macOS                  |

The Linux builds are static, so there is no glibc floor and no runtime dependency
to install. The macOS builds link only against system libraries.

```console
$ tar -xzf sismatic-server-<version>-<target>.tar.gz
$ ./sismatic-server-<version>-<target>/sismatic-server --help
```

#### Nix

The same artifact is one command away locally — CI runs exactly this, and nothing
about the build lives in the workflow:

```console
$ nix run   .#server -- --help    # run it straight out of the flake
$ nix build .#server-release      # -> result/sismatic-server-<version>-<target>.tar.gz
```

Each architecture is built on its own native runner, so `nix build .#server-release`
produces the artifact for the machine you run it on.

#### From source

```console
$ cargo build --release -p sismatic-server
```

#### Python

```console
$ pip install sismatic
```

An `abi3` wheel covering CPython 3.9+ — see [Other ways to drive it](#other-ways-to-drive-it).

## Configuration

### Two documents, one process

| Document                      | Names it                    | Default               | Describes                                                     |
| ----------------------------- | --------------------------- | --------------------- | ------------------------------------------------------------- |
| **Server configuration**      | `-c` / `SISMATIC_SERVER__CONFIG` | `configuration.yaml` | The process: what to poll, what to keep, where to listen      |
| **Devices configuration**     | `inventory.config_path`     | `devices.toml`        | The fleet: hosts, credentials, timeouts, groups               |

They are deliberately separate. The devices document is passed to `sismatic-core`
as-is and could be shared with another process (the CLI and the Python SDK read
exactly this file); the server document is about one running server.

**Both documents can be written in YAML or TOML**, and the format is inferred from
the file extension — so `configuration.yaml` and `configuration.toml` are equally
valid, as are `devices.yaml` and `devices.toml`. Mixing is fine: a YAML server
config may name a TOML devices file. The server configuration additionally accepts
JSON; the devices document, as the server reads it, is YAML or TOML only (a
`devices.json` is refused with `unsupported config file extension`, though the CLI
and the Python SDK do accept it).

A relative `inventory.config_path` is resolved against the **server config file's
own directory**, not the process's working directory. (A path given on the command
line is relative to the working directory, as a command-line path should be.)

Every key in both documents is checked on load: an unknown or misspelled key is a
startup error naming the key, never a setting that silently did nothing.

### Server configuration

<table>
<tr><th>YAML — <code>configuration.yaml</code></th><th>TOML — <code>configuration.toml</code></th></tr>
<tr valign="top"><td>

```yaml
inventory:
  config_path: devices.yaml
  # runtime_config_path: devices.state.yaml

intent_relay:
  poll_ms: 250
  max_attempts: 3

sync:
  interval_secs: 300
  fields:
    - "*"
    - name: RUNNING_STATE
      interval_secs: 5
    - name: MAC_ADDRESS
      interval_secs: 0

store:
  retain: 24h
  cleanup_interval: 5min
  max_memory: 256MiB

http:
  host: 0.0.0.0
  port: 9000
```

</td><td>

```toml
[inventory]
config_path = "devices.toml"
# runtime_config_path = "devices.state.toml"

[intent_relay]
poll_ms = 250
max_attempts = 3

[sync]
interval_secs = 300
fields = [
  "*",
  { name = "RUNNING_STATE", interval_secs = 5 },
  { name = "MAC_ADDRESS", interval_secs = 0 },
]

[store]
retain = "24h"
cleanup_interval = "5min"
max_memory = "256MiB"

[http]
host = "0.0.0.0"
port = 9000
```

</td></tr>
</table>

Those are the same settings in each format, each naming the devices file in its own.
One YAML-only wrinkle is worth knowing: a bare `*` opens an alias in YAML, so the
wildcard **must be quoted** there. In TOML it is an ordinary string either way.

#### `inventory`

| Key                   | Default        | Meaning                                                                 |
| --------------------- | -------------- | ----------------------------------------------------------------------- |
| `config_path`         | `devices.toml` | The devices document, relative to this file's directory.                |
| `runtime_config_path` | *unset*        | Where fleet edits made over the API are persisted. Unset means nothing is persisted, so the devices file stays unambiguously authoritative and a restart returns to exactly what it says. |

Set `runtime_config_path` only if you want a fleet edited through the API to
survive a restart, and know what that buys: the state file then **wins at startup**,
so an edit to `devices.yaml` has no effect until a reset adopts it (the server warns
on every startup that loads from it, naming both paths), and it necessarily carries
device credentials, since it exists to be loadable.

#### `sync`

The polling runtime. `interval_secs` is the default cadence; `fields` says what to
poll and lets any field override it.

| Key             | Default | Meaning                                                     |
| --------------- | ------- | ----------------------------------------------------------- |
| `interval_secs` | `30`    | Default poll interval for every listed field.                |
| `fields`        | `["*"]` | Which fields to poll. Entries are a bare name or a `{ name, interval_secs }` pair. |

`"*"` is a catch-all standing for every field `sismatic-core` can query, so a
deployment never has to spell the catalog out. A named field outranks the wildcard,
which is what makes the idiom above work: everything at 300s, `RUNNING_STATE` at 5s,
and `MAC_ADDRESS` listed but never polled (`interval_secs: 0`) because it does not
change.

#### `store`

What the store keeps, and how much of the machine it may use. See
[Retention and the memory budget](#retention-and-the-memory-budget) for the reasoning.

| Key                 | Default   | Accepts                                                        |
| ------------------- | --------- | -------------------------------------------------------------- |
| `retain`            | `24h`     | A rolling window (`30d`, `2 weeks`, `1h 30min`), a fixed floor (`2026-01-01T00:00:00Z`, or a bare `2026-01-01` read as midnight UTC), or `forever`. |
| `cleanup_interval`  | `5min`    | A duration, or `never` / `0` to start no sweeper at all.       |
| `max_memory`        | `256MiB`  | `512MiB`, `2GB`, `1.5GiB`, a plain byte count, or `unlimited`.  |
| `cleanup_on_remove` | `false`   | Whether removing a device also drops its recorded reads.        |

Durations use [systemd's time-span spelling](https://www.freedesktop.org/software/systemd/man/systemd.time.html)
— terms of `<number><unit>`, summed, with `s`/`min`/`h`/`d`/`w`/`M`/`y` and their
long forms. Sizes follow `systemd`'s `MemoryMax=`: `KiB`/`MiB`/`GiB`/`TiB` and the
bare `K`/`M`/`G`/`T` are powers of 1024, while `KB`/`MB`/`GB`/`TB` are powers of 1000.
Every key here also accepts a plain integer — seconds for the durations, bytes for
the size — which is what makes the section reachable from the environment.

A bare `0` is rejected for `retain` rather than guessed at, because "off" and "keep
nothing" are opposite readings of it and only one of them is safe. Write `forever`.

#### `http`

| Key    | Default     | Meaning                     |
| ------ | ----------- | --------------------------- |
| `host` | `127.0.0.1` | Interface to bind.          |
| `port` | `8080`      | Port to bind.               |

#### `intent_relay`

How the write side drains its outbox.

| Key            | Default | Meaning                                                                        |
| -------------- | ------- | ------------------------------------------------------------------------------ |
| `poll_ms`      | `250`   | Floor on how long an accepted write waits before a device hears about it.       |
| `max_attempts` | `3`     | Total tries, not retries — `1` means a write that fails once is not re-attempted. |

#### `defaults`

A flat fallback section for anything the sections above leave unset —
`interval_secs`, `fields`, `host`, and `port`:

```yaml
defaults:
  interval_secs: 60
  port: 9000
```

### Devices configuration

The fleet: an optional `defaults` table, the devices, and any groups over them.
Each device inherits every default it does not set itself, so a nearby device can
be three lines while a far one overrides only the timeouts it needs.

Both spellings of the collection keys are accepted in either format — `devices` /
`device` and `groups` / `group` — so TOML's array-of-tables idiom (`[[device]]`) and
YAML's list idiom (`devices:`) each read naturally.

<table>
<tr><th>YAML — <code>devices.yaml</code></th><th>TOML — <code>devices.toml</code></th></tr>
<tr valign="top"><td>

```yaml
defaults:
  port: 22023
  username: admin
  password: extron
  connect_secs: 5
  exchange_secs: 3
  eager: true
  sis_keepalive_secs: 120
  eager_retry_secs: 30
  cold_backoff_secs: 30
  auto_disable_after: 2
  self_heal_secs: 0

devices:
  - id: atrium-101
    host: 10.0.0.7

  - id: annex-far
    host: 10.9.40.12
    connect_secs: 20
    exchange_secs: 10
    # unlicensed on this unit; never ask
    disabled_fields:
      - STREAM_2_NAME
      - STREAM_3_NAME

groups:
  - id: room-5
    devices: ["atrium-101", "annex-far"]
    barrier_timeout_secs: 15
    barrier: fail_batch
```

</td><td>

```toml
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
sis_keepalive_secs = 120
eager_retry_secs = 30
cold_backoff_secs = 30
auto_disable_after = 2
self_heal_secs = 0

[[device]]
id = "atrium-101"
host = "10.0.0.7"

[[device]]
id = "annex-far"
host = "10.9.40.12"
connect_secs = 20
exchange_secs = 10
# unlicensed on this unit; never ask
disabled_fields = ["STREAM_2_NAME", "STREAM_3_NAME"]

[[group]]
id = "room-5"
devices = ["atrium-101", "annex-far"]
barrier_timeout_secs = 15
barrier = "fail_batch"
```

</td></tr>
</table>

#### Device keys

`id` and `host` are the only keys a device must state itself. `username` and
`password` must be resolvable from the device or from `defaults`; everything else
falls back to a built-in default.

| Key                  | Default   | Meaning                                                                       |
| -------------------- | --------- | ----------------------------------------------------------------------------- |
| `id`                 | *required*| How every facade addresses this device. Shares a namespace with group ids.      |
| `host`               | *required*| Hostname or address.                                                          |
| `port`               | `22023`   | SSH port.                                                                     |
| `username`           | —         | SSH user. Required, from here or `defaults`.                                   |
| `password`           | —         | SSH password. Required, from here or `defaults`.                               |
| `connect_secs`       | `5`       | Connect (handshake) timeout.                                                  |
| `exchange_secs`      | `3`       | Per-instruction round-trip timeout.                                           |
| `eager`              | `false`   | Hold a warm connection rather than connecting on first instruction.            |
| `sis_keepalive_secs` | `120`     | While warm, how often to send a benign probe so the device's idle timer never fires. `0` disables. |
| `eager_retry_secs`   | `30`      | While eager but cold, how often to re-attempt the handshake. `0` gives up after the first failure. |
| `cold_backoff_secs`  | `30`      | After a failed dial, how long before another is attempted. `0` disables the gate. |
| `disabled_fields`    | *unset*   | Fields never to ask this device for. Inherited whole or not at all.            |
| `auto_disable_after` | `2`       | Consecutive refusals that take a field out of the schedule by themselves. `0` disables the inference. |
| `self_heal_secs`     | `0`       | How often to retry an auto-disabled field. `0` never retries.                  |

`disabled_fields` is the one key that does not merge: a device writing its own list
**replaces** the inherited one rather than adding to it. That is deliberate — it is
what lets one licensed unit opt out of a fleet-wide veto with `disabled_fields: []`.

#### Group keys

| Key                    | Default                                    | Meaning                                                    |
| ---------------------- | ------------------------------------------ | ---------------------------------------------------------- |
| `id`                   | *required*                                 | How the group is addressed.                                |
| `devices`              | *required*                                 | Member ids, each naming a device defined in the same file.  |
| `barrier_timeout_secs` | derived from the slowest member's connect + exchange | How long to hold a group command waiting for every member. |
| `barrier`              | `fail_batch`                               | `fail_batch` or `dispatch_ready` — what to do when the wait expires with members missing. |

Nothing under a group inherits from `defaults`: a group is a name over existing
devices plus a policy of its own, and a fleet-wide default barrier would be a claim
about groups this file cannot make.

### Environment variables

Every key of the server configuration is reachable from the environment. The
variable name is the prefix `SISMATIC_SERVER`, then the key path, joined by a double
underscore and upper-cased:

```sh
SISMATIC_SERVER__CONFIG=/etc/sismatic/configuration.yaml   # which file to read
SISMATIC_SERVER__INVENTORY__CONFIG_PATH=/etc/sismatic/devices.yaml
SISMATIC_SERVER__SYNC__INTERVAL_SECS=60
SISMATIC_SERVER__SYNC__FIELDS=RUNNING_STATE,FIRMWARE        # lists are comma-separated
SISMATIC_SERVER__STORE__RETAIN=7d
SISMATIC_SERVER__STORE__CLEANUP_INTERVAL=10min
SISMATIC_SERVER__STORE__MAX_MEMORY=2GiB
SISMATIC_SERVER__HTTP__HOST=0.0.0.0
SISMATIC_SERVER__HTTP__PORT=8080
```

`SISMATIC_SERVER__CONFIG` is the one variable that is not a config key: it names
*which* document to load, so it must be answered before there is a document to look in.

### Precedence

Most specific wins, and every layer is optional. For the host and the port:

1. `--host` / `--port` on the command line
2. `SISMATIC_SERVER__HTTP__HOST` / `SISMATIC_SERVER__HTTP__PORT`
3. `http:` in the file, then `defaults:`
4. the built-in constant

Which file to read follows the same shape: `-c/--config-path`, then
`SISMATIC_SERVER__CONFIG`, then `configuration.yaml` in the working directory.

Flags carry no clap-side defaults, deliberately — a default supplied by the command
line would be indistinguishable from one you typed, and would outrank every layer
below it on every run.

```console
$ sismatic-server --help
  -c, --config-path <PATH>             Path to server configuration file
  -i, --inventory-config-path <PATH>   Path to devices configuration file
  -H, --host <HOST>                    Host to serve on
  -p, --port <PORT>                    Port to serve on
```

### Changing configuration while it runs

Most of the server document can be changed without restarting the process:

```text
GET   /v1/config          every setting as it stands
PATCH /v1/config          change some of them
POST  /v1/config/reload   read the config file again and apply it
```

A change takes effect before the response is written. Re-timing a field starts and
stops the poll loops for it; a new retention window is enforced at the next sweep,
and a lowered `max_memory` discards the oldest history immediately.

```sh
# poll everything every sixty seconds
curl -X PATCH localhost:8080/v1/config \
     -H 'content-type: application/json' \
     -d '{"sync": {"interval_secs": 60}}'

# watch one field closely and leave the rest alone
curl -X PATCH localhost:8080/v1/config \
     -H 'content-type: application/json' \
     -d '{"sync": {"fields": [{"name": "RUNNING_STATE", "interval_secs": 5},
                              {"name": "*", "interval_secs": 300}]}}'

# keep less, and cap the store harder
curl -X PATCH localhost:8080/v1/config \
     -H 'content-type: application/json' \
     -d '{"store": {"retain": "6h", "max_memory": "64MiB"}}'
```

The body speaks the same vocabulary as the config file — `30d`, `512MiB`, `never`,
`forever`, `unlimited`, `interval_secs: 0` for a field you want listed and not
polled — because it is parsed by the same code. A key you do not name is left alone,
and a misspelled key is a `400` rather than a setting that silently did nothing.

`GET /v1/config` returns a complete statement of every setting, and that body is
itself a valid `PATCH` body. So read-modify-write is safe to script: fetch it, change
one number, send it back, and nothing you did not touch moves.

**What cannot change.** `http` and both paths under `inventory` are reported and not
editable — one is the socket the server is already bound to, the others name the files
the device registry and its SSH sessions were built from. Naming any of them at the
value it already has is accepted (that is what keeps the whole document a valid
patch); changing one is a `409` saying a restart is what applies it.

Their *contents* are a different matter. `POST /v1/inventory/config/reset` re-reads
`inventory.config_path` and adopts it wholesale, and the `/v1/inventory` routes change
the fleet without touching any file — so a device no longer needs a restart to arrive.
Whether such a change outlives the process depends on `inventory.runtime_config_path`.

**A patch is not written to disk.** It changes the running process, and the file stays
the single source of truth — so a `PATCH` is an override that lasts until the next
reload or restart. Put a change you want to survive in the file.

#### Kubernetes

Keep the server config in a ConfigMap and mount it as the file the server reads.
kubelet rewrites that file within a minute or so of the ConfigMap changing, and
nothing tells the process — so a watcher calls the reload route:

```console
$ curl -X POST localhost:8080/v1/config/reload
```

That re-runs exactly the load the process ran at startup — file, then environment,
then the flags the command line carried — so the running settings converge on the
ConfigMap. It takes no body and is idempotent, which is what makes it safe to call on
every write to the mounted volume (a ConfigMap update produces more than one). Either
a sidecar watching the mount or a `kubectl exec` in a rollout hook will do.

| Status | Meaning                                                                                        |
| ------ | ---------------------------------------------------------------------------------------------- |
| `200`  | Applied. The body is every setting as it now stands.                                            |
| `400`  | The file parsed but a value could not be applied. Nothing was reloaded.                         |
| `409`  | `http` or an `inventory` path moved. Those need a restart, so nothing was reloaded — roll the deployment. |
| `500`  | The file could not be read or parsed. The server keeps running under the settings it had.       |

The `409` is the one worth automating around: it is the ConfigMap telling you this
particular change is a rolling restart rather than a reload.

> [!WARNING]
> There is no authentication in front of `/v1/config` yet, and unlike the rest of the
> API it changes what the server does. Keep it behind whatever fronts the service — a
> NetworkPolicy, an ingress rule, or by not exposing the port beyond the cluster.

## HTTP API

Interactive reference at **`/api`**, OpenAPI document at **`/api-docs/openapi.json`**.
A group id is accepted anywhere a device id is.

**Reads** — served from the store, so they never touch a device:

| Route                                                         | Returns                                     |
| ------------------------------------------------------------- | ------------------------------------------- |
| `GET /v1/reads`                                               | Which fields the routes below accept         |
| `GET /v1/reads/devices`                                       | The whole fleet's latest values, filtered and paged (`?fields=&devices=&group=&where=&limit=&after=`) |
| `GET /v1/reads/devices/{id}/fields`                           | One device's latest reads                    |
| `GET /v1/reads/devices/{id}/fields/{field}`                   | One field, as of the last poll               |
| `GET /v1/reads/devices/{id}/fields/{field}/history`            | One field over time (`?start=&end=&limit=`)  |
| `GET /v1/reads/groups`, `…/groups/{id}/fields[/{field}[/history]]` | The same four shapes, per group        |

**Writes** — recorded and answered `202 Accepted` with a `Location` header; the
intent relay contacts the device afterwards, and the `Location` route reports the
outcome:

| Route                                                  | Does                                            |
| ------------------------------------------------------ | ----------------------------------------------- |
| `GET  /v1/writes`                                      | Which names a write may address                  |
| `POST /v1/writes/devices/{id}/recording/start`         | Start recording (also `stop`, `pause`)           |
| `GET  /v1/writes/devices/{id}/recording`               | The desired recording state                      |
| `PUT  /v1/writes/devices/{id}/metadata/{field}`        | Write a metadata register (e.g. `title`)         |
| `PUT  /v1/writes/devices/{id}/settings/{field}`        | Write a device setting                           |
| `GET  /v1/writes/devices/{id}/history`                 | Writes submitted against this device             |
| `…/groups/{id}/…`                                      | Every device route above, fanned out across a group |
| `GET  /v1/writes/{id}`                                 | One write's outcome                              |

**Inventory** — change the fleet without touching a file:

| Route                                            | Does                                                      |
| ------------------------------------------------ | --------------------------------------------------------- |
| `GET/POST /v1/inventory/devices`                 | List or add devices                                        |
| `GET/PUT/DELETE /v1/inventory/devices/{id}`      | Read, replace, or remove one                               |
| `GET/POST /v1/inventory/groups`                  | List or add groups                                         |
| `GET/PUT/DELETE /v1/inventory/groups/{id}`       | Read, replace, or remove one                               |
| `GET  /v1/inventory/config/export`               | The running fleet as a loadable document (`?format=toml\|yaml\|json`) |
| `POST /v1/inventory/config/reset`                | Re-read `inventory.config_path` and adopt it wholesale      |

`config/export` is how a fleet edited at runtime becomes a file you can put in
version control: save it under the extension you asked for and the devices loader
reads it back unchanged. Every key is written out explicitly rather than left to
inherit, because a device exported with blank keys would re-resolve against whatever
`defaults` the importing file happens to carry — a different device, silently.

Passwords are **excluded** unless you ask for them with `?include_secrets=true`, which
means a plain export is not directly loadable: the credentials come back from
`defaults`, an environment variable, or a secret store. That trade is deliberate — an
export lands in shell history, ticket attachments and CI logs, and a plaintext recorder
password in any of those outlives every process that could have rotated it. Turn it on
only for an export going straight to a file a secret manager owns.

**Operations:** `GET /health_check`, plus `/v1/config` as described above.

## Concepts

### Warm versus cold connections

The premise is that SSH handshakes are expensive, so a preestablished — "warm" —
connection matters for runtime responsiveness.

Device connections are **lazy by default**: the connection is established when the
first instruction is sent. After that, the SSH layer keeps it alive until the device
itself terminates it for inactivity — five minutes on many devices — after which it
goes cold and is reestablished on the next instruction.

Setting **`eager`** opens connections up front instead, paying the full handshake at
startup for maximum responsiveness later. **`sis_keepalive_secs`** keeps them warm by
sending a benign instruction on an interval, resetting the device's inactivity timer.

`eager` is a *standing* intent to hold a warm connection, not a one-time connect at
startup. A device that is unreachable when the process starts — or that drops later —
would otherwise stay cold until the next real instruction. **`eager_retry_secs`**
closes that gap: while a device is eager but cold, a background task re-attempts the
handshake on this interval until the device answers and returns to the keepalive
cadence. Set it to `0` to give up after the first failed connect.

Because a device holds a single connection, every caller that wants an unreachable
device would otherwise pay its own `connect_secs` to rediscover the same fact — and
pay it serially, queued behind one another. A fleet poller running one loop per
(device, field) turns a single dead SMP into dozens of full connect timeouts per
round. **`cold_backoff_secs`** stops that: a failed dial is remembered, and callers
arriving before the window closes fail immediately instead of dialing. One dial per
window tests the device on everyone's behalf. This applies to every device, eager or
not, since it is a property of the connection rather than of the keep-warm intent.

The two settings are complementary rather than overlapping. For an eager device the
keepalive task deliberately dials *through* the backoff window — it is the component
whose job is re-testing a device that is down — so the re-dial cadence stays exactly
`eager_retry_secs`, and `cold_backoff_secs` only holds off everyone else in between.
For a lazy device there is no such task, so the window closing is what lets the next
instruction re-test the device.

### Device groups

A group bundles several devices behind a single id so they act as one. The motivating
case is more than one recorder in the same physical room that must start together:
address the group and every member receives the instruction at once, rather than one
after another.

A group is an `id`, the `devices` it contains, and a barrier policy. Each member must
name a device defined elsewhere in the same file. Group ids share the device id
namespace, so a group may not reuse a device's id, and any id resolves to at most one
device or group.

Sending to a group fans the instruction out concurrently — each member's exchange is
dispatched before any is awaited — over the members' own warm connections. A group
holds the same device handles the registry hands out, so grouping changes nothing
about a device's connection reuse or self-healing. A group run reports *every*
member's outcome: on success the members' replies tagged by device id, and on any
failure exactly which members failed and why, so a partial failure is surfaced
rather than hidden.

On the write side a group command is expanded into one row per member and all of them
are held until each is ready to go, so the group acts in unison. `barrier_timeout_secs`
bounds that wait; `barrier` says what happens when it expires with members missing —
`fail_batch` fails the whole batch, `dispatch_ready` sends to whoever arrived.

Across the facades, a group id is accepted anywhere a device id is:

- **HTTP**: `POST /v1/writes/groups/room-5/recording/start` records one write per
  member; `GET /v1/reads/groups` reports every group's state, and
  `GET /v1/inventory/groups` lists the configured groups.
- **CLI**: `sismatic command room-5 start` runs `start` on every member and prints one
  `device-id: value` line per member; `sismatic groups` lists group ids.
- **Python**: `sis.command("room-5", "start")` returns a `dict` keyed by member id (a
  single device still returns its scalar value); `sis.groups()` lists group ids.

### Field vetoes

A field can be unanswerable because an operator *said so* or because the device *keeps
saying so*. Only the first is configuration.

**`disabled_fields`** is declared and immutable — part of a device's identity. Editing
it produces a different device with a different internal uuid, which is what lets a
running registry tell a device that changed from one that was merely re-read. Use it
for fields a unit will refuse forever: an SMP whose stream 2/3 features are unlicensed
answers those queries with an error code every time, so naming them means they are
never asked for at all.

The inferred set is runtime *observation*: after **`auto_disable_after`** consecutive
refusals, a field is taken out of the schedule. It is held by the registry against a
device id and outlives any one device config — putting it in the file instead would
mean the system replaced a device, and dropped its warm SSH session, every time it
learned something. Only the sync driver counts refusals, because only a repeating
caller can observe "consecutive": a refused hand-issued write is evidence of nothing,
which keeps a one-off from disabling a field the fleet depends on. Any refusal counts,
not only `E13` — to a scheduler every code means the same thing, namely that the device
read the verb, decided against it, and will decide against it again.

**`self_heal_secs`** is how long a disabled field waits before being tried again; `0`,
the default, never retries.

The two halves close into a discovery loop:
`GET /v1/inventory/config/export?promote_auto_disabled_fields_to_disabled_fields=true`
writes each device's *inferred* vetoes into its `disabled_fields`, so committing the
result makes them permanent — and free, since a declared veto costs no poll loop at
all where an inferred one costs a timer tick. Only fields actually disabled are
promoted: one still being watched, refused once against a threshold of two, is
evidence of nothing yet.

### Retention and the memory budget

The store bounds what it keeps. This matters most for the in-memory store, whose
history would otherwise grow for as long as the process runs — making memory use a
function of uptime rather than of the fleet.

Prefer a **rolling window** for `retain`: it is the only form whose memory cost stops
growing. A fixed floor widens by a day every day, so it needs `max_memory` under it.

`max_memory` is a cap, not a reservation — nothing is allocated up front — and it is
enforced on **every write**, not at each cleanup, so a burst arriving between sweeps
cannot exhaust the machine. When the store is full the oldest reads go first, across
the whole fleet rather than per device.

The budget is a backstop, not the plan. `retain` is what an operator reasons about;
`max_memory` catches the case where the retention window turned out to cost more than
expected, because the bytes it costs depend on the fleet size and the poll schedule. A
`"*"` wildcard at 300s is roughly nine thousand reads per device per day, so a
fifty-device installation writes something like half a gigabyte a week — which is why
the default window is a day and not a month. If the budget starts discarding reads the
window would have kept, the server says so and names both settings.

The latest reading of each field is never expired or evicted. It is bounded by the
configuration rather than by uptime, and dropping it would make a recorder that went
quiet look like one that never reported.

## Other ways to drive it

The server is the main use case, but the same core is reachable two other ways. Both
read the **same devices document** described above.

### CLI

```console
$ sismatic --config devices.yaml ids
$ sismatic --config devices.yaml groups
$ sismatic --config devices.yaml query atrium-101 firmware
$ sismatic --config devices.yaml command room-5 start
$ sismatic --config devices.yaml register atrium-101 title "Week 4 — Lecture"
```

Run it from the flake with `nix run . -- --help`.

### Python

```console
$ pip install sismatic
```

```py
from sismatic import Sis

with Sis.from_file("devices.yaml") as sis:
    sis.register("atrium-101", "title", "Week 4 — Lecture")
    sis.command("room-5", "start")   # -> {'atrium-101': 'RcdrY1', 'annex-far': 'RcdrY1'}
# every SSH connection is closed on exit
```

`Sis.from_file` picks the deserializer from the extension; `Sis.from_config(mapping)`
takes an already-parsed `dict` shaped the same way, so you can source configuration
from INI, XML, a database row, or the environment with any parser that produces a
dictionary. The wheel ships a PEP 561 `py.typed` marker and a type stub, so editors
and `mypy` see full signatures including the accepted instruction names.

Full API docs: <https://metzenseifner.github.io/sismatic/> ·
[SDK README](./crates/sismatic-python-sdk/README.md)

## Architecture

The load-bearing rule is that **no frontend has a compile path to `sismatic-core`**.
The two halves of the system meet across a storage port, and only the composition root
knows the read and write traits are one object.

| Crate                                                             | Role                                                                    |
| ----------------------------------------------------------------- | ----------------------------------------------------------------------- |
| [`sismatic-server`](./crates/sismatic-server/README.md)           | **The composition root.** Reads both documents, starts sync, the relay, and the API on one runtime. |
| [`sismatic-core`](./crates/sismatic-core)                         | The SIS protocol model, the device/SSH layer, and the devices-config loader. |
| [`sismatic-http-api`](./crates/sismatic-http-api)                 | The read side: an actix-web application over the store's `ReadStore` port. |
| [`sismatic-api-types`](./crates/sismatic-api-types)               | The wire contract — serde DTOs, no logic, no I/O, no internal dependencies. |
| [`sismatic-store`](./crates/sismatic-store)                       | The storage port: `WriteStore`, `ReadStore`, and `Lifecycle`.             |
| [`sismatic-store-memory`](./crates/sismatic-store-memory)         | The in-memory adapter the server runs on today and the tests run on always. |
| [`sismatic-sync`](./crates/sismatic-sync)                         | The poll loops, and the only place the device model and the wire contract meet. |
| [`sismatic-intent-relay`](./crates/sismatic-intent-relay)         | Drains queued write intents from the outbox to devices.                  |
| [`sismatic-cli`](./crates/sismatic-cli)                           | The command-line facade.                                                 |
| [`sismatic-python-sdk`](./crates/sismatic-python-sdk/README.md)   | The blocking Python facade, shipped as an `abi3` wheel.                   |

## Development

Everything is a flake output, and CI is only a dispatcher over these — no build logic
lives in the workflow.

```console
$ nix develop                          # the pinned toolchain plus every check's dependencies
$ nix flake check                      # every check; must pass
$ nix flake check --all-systems
$ nix build .#checks.<system>.<name>   # one check
$ nix build .#server-release           # the publishable server tarball
$ nix run   .#build-wheel              # the portable wheel
$ nix run   .#docs -- serve            # the API doc site
```

Inside the dev shell the usual cargo workflow applies:

```console
$ cargo test --workspace
$ cargo clippy --workspace --all-targets
$ cargo fmt
```

> [!NOTE]
> `nix flake check` evaluates from a git-tracked source tree, so a new file must be
> `git add`-ed before the sandbox can see it.

## Documentation

| Where                                                                 | What                                              |
| --------------------------------------------------------------------- | ------------------------------------------------- |
| `/api` on a running server                                            | Interactive API reference, generated from the code |
| [Server README](./crates/sismatic-server/README.md)                   | The server's own notes on configuration and reload |
| [Python API reference](https://metzenseifner.github.io/sismatic/)     | The full `Sis` surface, built from [`docs/`](./docs) with `nix run .#docs` |
| [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/metzenseifner/sismatic) | Ask questions about this codebase |

## License

[ECL-2.0](./LICENSE) — the Educational Community License, Version 2.0.
Copyright 2026 Jonathan L. Komar.
