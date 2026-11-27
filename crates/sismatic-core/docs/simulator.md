# Simulated Extron SMPs

`sismatic-core`'s `simulator` feature ships a real SSH server that behaves like an
Extron SMP. It is the same code the integration tests bind, so what it answers is
what CI exercises — and because it is a server rather than a mock, everything above
it is the production stack: the real SIS codec, the real transport, the real poll
loops, the real HTTP API.

Two binaries, both behind `--features simulator`:

| | |
| --- | --- |
| `simulated-device` | **one** SMP, configured from a YAML file and the command line |
| `simulated-fleet` | **a building** — one SMP per device in a `devices.yaml`, from one process |

```console
$ cargo build -p sismatic-core --features simulator --bins
```

The feature is off by default, deliberately: `ssh` alone gives you the client
transport, and the device *server* must never link into the production front-ends.
So a plain `cargo build` produces neither binary.

---

## Why a simulated device can be trusted here

A simulated device is usually a liability: a second, hand-written copy of the
protocol that rots the moment a payload or a parser changes, and rots *quietly* —
the failure surfaces as a mysterious `exchange_timeout`, not a compile error.
Anything you prove against one is a fact about the mock.

This one has no request-bytes-to-reply-bytes table. Everything it answers is
derived from the instruction catalogs the client encodes with:

| Concern | Derived from |
| --- | --- |
| Which requests exist | `Query::ALL`, `Command::ALL`, `Register::ALL`, `Setting::ALL` |
| Their exact bytes | each enum's own `instruction()` constructor |
| Which config keys are legal | the catalogs' `FromStr` — aliases and case-insensitivity come free |
| Whether a value is well-formed | the *production* parser, run over the simulator's own output |

So a config names **fields**, not payloads, and "well-typed" means the real parser
accepts what the simulator would put on the wire. `--check-config` reports every
disagreement in one pass, and CI's drift guards assert there are none:

```console
$ cargo run -p sismatic-core --features simulator --bin simulated-device -- --check-config
<built-in config>: ok
```

Algebraically, a device is a partial map `Field → wire text`, where
`Field = names(Query) ∪ names(Register) ∪ names(Setting)` joined on the canonical
name. The catalogs are read and write *views* over that one namespace, so a write
lands in the cell a later query reads with no pairing table to maintain. See
`sismatic_core::simulator`'s module docs for the three-way split (read/write,
read-only, and the nine write-only RTMP push fields SIS offers no read of).

---

## One device

```console
$ simulated-device                                  # 127.0.0.1:2222, built-in config
$ simulated-device --licensed-streams 1             # a base unit
$ simulated-device --config ./lecture-hall.yaml \
    --field unit-name='Lecture Hall 2' --field running-state=1
$ simulated-device --licensed-streams 1 --print-config
```

Three layers, each beating the one before it: the compiled-in config, then a
`--config` file, then the individual flags. `--print-config` reports what a
combination resolved to, including every field it will refuse.

| Flag | |
| --- | --- |
| `--config` | device config (YAML). Defaults to the compiled-in fixture, so the binary runs from any directory |
| `--licensed-streams` | stream indices this unit has: `1`, `1,2`, `all`, `none`. Replaces the file's list |
| `--unlicensed-field` | refuse this field by name, whatever stream it belongs to. Repeatable |
| `--refusal-code` | the SIS code refusals answer with, without the `E`. Defaults to `13` |
| `--field NAME=VALUE` | seed one field's initial value. Repeatable, canonicalised through the catalog |
| `--username`, `--password` | the credentials it accepts |
| `--host`, `--port` | where it listens. Defaults to `127.0.0.1:2222` |
| `--stdout` | answer on stdout rather than the extended-data stream real hardware uses |
| `--check-config` / `--print-config` | validate, or report what resolved, and exit |

### The device config

```yaml
username: admin
password: extron

licensed_streams: [1, 2, 3]      # 1 Archive A, 2 Archive B (Dual Mode), 3 Confidence
unlicensed_fields: [SNMP_STATE]  # anything else this unit refuses, by name
refusal_code: 13                 # answered as `E13`

fields:
  FIRMWARE: "3.06"
  STREAM_1_NAME: Archive Channel A
  # ... every field in the catalog; --check-config names the ones you missed
```

Values are the device's **wire text** — what it would put on the channel, minus the
CR LF. There is no per-field type table: a value is well-formed iff the production
parser accepts it. The config is the map's *initial* value; register and setting
writes overwrite a field, and recording commands rewrite `RUNNING_STATE`.

### A field whose reply is a list

One field breaks the "wire text is a scalar" rule, because the device itself does:
`RECORDING_EVENTS` answers with the recorder's table — one JSON object per line,
the list terminated by a blank one. A list is not something a YAML scalar or a
`--field NAME=VALUE` flag can hold, so that field takes a **document** instead,
folded into a single JSON object under one key:

```yaml
fields:
  RECORDING_EVENTS:
    recording_events:
      - db_id: 1
        start: 1767879511
        identifier: sim-smp-351_20260108-143831
        creator: c900011
        title: Lecture 1
        state: 11
        media_present: "true"      # the device really does send a string
        package: /var/internal/sim-smp-351_20260108-143831
      - db_id: 2                   # a sparse row: absent, not zeroed
        start: 1767883456
        identifier: sim-smp-351_20260108-154417
        package: /var/internal/sim-smp-351_20260108-154417
```

`recording_events: []` is a recorder that has never recorded, and it is not the
same as leaving the field out: an empty list is an *answer* (the bare terminator),
while an absent field is silence, which reaches the client as a command timeout.

A real recorder holds hundreds of rows, which belong in a file rather than in a
device config. The same document, named:

```yaml
fields:
  RECORDING_EVENTS:
    file: ./recordings.json
```

The file is the identical `{"recording_events": [...]}` document, read with the
same parser as the config, so `.json` and `.yaml` both work. A relative path is
resolved against the **process working directory**, not the config's own
directory — a device config can be a compiled-in string (`simulated-device` with
no `--config`) or a `--device-config-for` template, so there is not always a
directory to anchor to. An unreadable or malformed file fails the load, naming the
path, rather than leaving the field empty and claiming later that the recorder has
nothing.

Capturing one off a real device gives you lines, not a document. `jq` folds them:

```console
$ sismatic query hall-a recording_events > rows.jsonl
$ jq -s '{recording_events: .}' rows.jsonl > recordings.json
```

Only the `db_id` of a row is required; `start`, `state` and `media_present` stay
absent when they are absent, because a firmware that does not report one is making
a different claim than a `0`, and keys this type does not name are preserved
verbatim. `--check-config` runs the rendered table through the client's own parser
like every other field, so a document the client could not read is reported here
rather than as a mysteriously empty list later.

### Licences, and why a refusal is not silence

An SMP sells its second and third stream as licensed options, and a unit without
the licence does not answer those fields with a default — it **refuses** them, with
a SIS error code, every time.

`licensed_streams: [1]` therefore refuses fourteen fields across streams 2 and 3 —
the names, the enable states, the push URLs, the push enables and the live states —
without any of them being listed: which stream a field belongs to is read out of
its canonical name, so a stream field added to a catalog is classified the moment
it is named.

The licence is a veto *ahead of* the field lookup rather than an absent field, and
it covers writes as much as reads: a unit without Dual Mode will not be talked into
naming its second stream.

```console
$ sismatic query base-unit stream_2_name
Error: device refused `STREAM_2_NAME` with E13: invalid parameter (number is out of range)
```

A refusal is deliberately *not* the same as an unset field, which the simulator
answers with silence. The client routes the two differently — a refusal is a
completed exchange that happens to say no, so it feeds `auto_disable_after`, while
silence is indistinguishable from a broken connection and feeds `exchange_timeout`
— so a simulator that conflated them could not drive the mechanism it exists to
test. That mechanism is the whole reason the licence is modelled at all.

---

## A building

`simulated-fleet` takes the same `devices.yaml` the server does and serves a
simulated SMP for every device in it, in one process:

```console
$ simulated-fleet /etc/sismatic/devices.yaml --host 127.0.0.1 --port-base 2401
simulated fleet from /etc/sismatic/devices.yaml
  3 unit(s), 1 group(s), listening; device template <built-in>, replies on stderr
  passwords masked — pass --show-credentials to print them

atrium-101
  uuid              7ec7598f-fac9-51ae-86bd-6eecd90afe82
  listening         127.0.0.1:2401   (declared 10.0.0.7:22023)
  credentials       admin / [REDACTED]
  licensed streams  1, 2, 3
  refused fields    none

annex-7
  uuid              6847b748-789f-5a49-99b4-795ef1d81b66
  listening         127.0.0.1:2402   (declared 10.0.0.8:22023)
  credentials       admin / [REDACTED]
  licensed streams  1, 2, 3
  refused fields    1 with E13 — SNMP_PRIVATE_COMMUNITY_STRING

groups
  north-wing        atrium-101, annex-7   barrier=dispatch_ready timeout=8s
```

### The inventory is not re-parsed

The file is read by `devices::config::load` — the same function the server's
composition root calls. So `defaults` inheritance, the one id namespace for devices
and groups, credential presence and the canonicalisation of `disabled_fields` all
behave here exactly as they do in production, because they *are* production. A
second reader of this schema would be a second thing to keep in step, and the
failure it produced would be a fleet that simulates a document nobody wrote.

### Placement

| Flag | |
| --- | --- |
| `--host ADDR` | bind every unit here instead of where the file puts it |
| `--port-base N` | assign ports from `N` upward, in document order |

Neither is on by default, so a bare run binds the fleet exactly as the document
describes it — which works only where those addresses are local. A real devices file
names the recorders' own addresses, so `--host 127.0.0.1` is what makes somebody
else's fleet servable here.

`--port-base 0` is special and useful: every unit gets a kernel-assigned port, so no
two can collide and nothing has to be reserved. The report names the ports that were
actually handed out — it is the only place to learn them, and they differ on every
run, so pass a real base if you want them stable between restarts.

### Pointing the server at it

`sismatic-server` has the same two overrides on the dialing side, under names that
leave its own `--host` (where the HTTP API listens) alone:

| `simulated-fleet` | `sismatic-server` |
| --- | --- |
| `--host ADDR` | `--device-host ADDR` |
| `--port-base N` | `--device-port-base N` |

Both programs place devices through one function,
`sismatic_core::devices::placement::Placement::place`, so the same document and the
same pair of values put every device in the same place on both sides:

```console
$ simulated-fleet devices.yaml --host 127.0.0.1 --port-base 2401
$ sismatic-server -c configuration.yaml --device-host 127.0.0.1 --device-port-base 2401
```

Three things differ, and each is deliberate:

- **The server refuses `--device-port-base 0`.** For a listener, `0` means "kernel,
  choose"; a client cannot follow that, so it would dial port 0.
- **Only the dial address moves.** The API, `GET /v1/inventory`, exports and
  persisted runtime state all keep the file's own address, so a test run cannot leak
  `127.0.0.1` into a file that goes back to production. The server logs a warning at
  startup, then one `device placement` line per device showing `declared` and
  `dialed`.
- **Ports are pinned per id, not re-counted.** The file's devices get `base + 0, 1,
  2, …` in document order at startup. A device added at runtime takes the next port
  after the highest one assigned, and a removed device's port is never reused, so the
  remaining devices keep the ports the simulator gave them.

### What crosses over from the inventory, and what does not

A devices file describes how a *server* should talk to recorders. Most of it is
therefore client-side policy with no device-side meaning, and is read only to be
reported:

| From the document | Becomes |
| --- | --- |
| `id` | the unit's `UNIT_NAME` and `SYSTEMNAME`, so a client can tell the units apart |
| `username`, `password` | the credentials the unit accepts |
| `disabled_fields` | the unit's refusal set — the operator's claim about a recorder, made true of the simulated one |
| `host`, `port` | reported as `declared`, and overridden by the flags above |
| `eager`, `sis_keepalive_secs`, `eager_retry_secs`, `connect_secs`, `exchange_secs`, `cold_backoff_secs`, `auto_disable_after`, `self_heal_secs` | **nothing.** A real SMP has no idea how often you intend to poll it. These matter in the devices file you point the *server* at |
| `groups` | reported, not simulated. A group is a name the server keeps over devices, not something the hardware knows about |

The `disabled_fields` derivation is the one worth knowing about: point a server at
this fleet with that veto *removed* from its own copy of the file and you can watch
it rediscover the fact from scratch, which is the `auto_disabled_fields` mechanism
running against something that genuinely refuses.

---

## Templates: defaults, overrides, and several configurations in one fleet

A **device template** is the single-device config from the first half of this page —
`fields:`, plus a licence block. The fleet applies one to every unit.

### The ladder

```
  built-in fixture                       every unit, when nothing else is said
    └─ --device-config FILE              every unit
         └─ --device-config-for ID=FILE  that unit only
```

With no flags, every unit is the compiled-in fixture: the same file
`simulated-device` uses, which covers the instruction catalog exactly. Two
consequences worth saying out loud. Every unit is **identical** apart from its name
and credentials — including `MAC_ADDRESS`, `IDENTIFIER`, `PART_NUMBER` and `DATE`,
so anything keyed on those sees one device several times. And `SSH_PORT` answers the
template's `22023`, not the port the unit is listening on: that field is what the
device *reports about itself*, which has no connection to where this program put the
listener.

### Who decides what

Not a single ladder, because different settings have different owners. This table is
the contract:

| | Decided by | |
| --- | --- | --- |
| which units exist | the inventory | one per `devices:` entry |
| where a unit listens | the inventory, then `--host` / `--port-base` | |
| `UNIT_NAME`, `SYSTEMNAME` | the **inventory**, from `id` | beats the template, which does not know the room names |
| credentials | the **inventory** | beats the template: the server has to be able to log in with what its own devices file says |
| every other field's value | the **template** in force | the ladder above |
| the licence | the **template**, unless `--licensed-streams` is given | that flag is fleet-wide and beats every template |
| refusals by name | the inventory's `disabled_fields` when non-empty, else the template's `unlicensed_fields` | replacement, not merge — see below |
| the refusal code | the **template** | `13` unless it says otherwise |

Two of those are easy to trip over.

**`--licensed-streams` is fleet-wide and absolute.** Giving it flattens every
template's licence to one value, which is what you want for "make the whole building
base units" and emphatically not what you want for a mixed fleet. Leave it off and
each unit's licence is its own template's.

**A non-empty `disabled_fields` replaces the template's refusal set** rather than
merging with it. Every override in the simulator's `Overrides` replaces wholesale,
for the reason stated there — merging makes a *narrower* value impossible to express
— and one rule across the layer is worth more than a special case here. The
consequence: a unit that needs both the document's refusals and its template's must
name them in one place, not one each.

### Several configurations in one fleet

This is what `--device-config-for` is for. A base unit beside fully-loaded ones:

```console
$ simulated-fleet devices.yaml --port-base 2701 \
    --device-config-for annex-7=./base-unit.yaml
```

```
hall-a
  listening         127.0.0.1:2701
  licensed streams  1, 2, 3
  refused fields    none

annex-7
  listening         127.0.0.1:2703
  template          ./base-unit.yaml
  licensed streams  1
  refused fields    14 with E13 — RTMP_2_BACKUP_URL, RTMP_2_STATE, RTMP_2_URL, RTMP_3_BACKUP_URL, +10 more
```

The flag is repeatable, so a fleet can carry as many configurations as it has
templates: older firmware in one wing, alarms already firing in another, a base unit
in the overflow room. A template carries its own licence block, which is why there
is no per-device licence flag — per-device variation goes through a template, and
one mechanism is easier to reason about than two that overlap.

The licence and refusals in the report are read off the device that was actually
*built*, not off the plan, so a mixed fleet is legible rather than merely intended.
The `template` line appears only for a unit that differs from the fleet-wide one; on
a uniform fleet it would be the same line under every unit.

An id that names no device in the inventory is an error listing the ids that do
exist, rather than a flag that silently applies to nothing.

### Templates are checked once each

Every distinct template is parsed and cross-checked against the instruction catalog
before anything is bound — once per template, not once per unit, because a template
is shared and N units would repeat one finding N times.

```
warning: device template partial.yaml has 58 issue(s); every unit in the fleet inherits them
  missing field `ACTIVE_ALARMS`: in the catalog but not the config
  ... and 54 more
  a missing field is answered with silence, which a client sees as a command timeout
```

A gap is a warning and not a failure, for the reason `simulated-device` gives: a
missing field goes unanswered, and a client seeing a command timeout is more useful
to debug against than a device that refused to start. Findings are summarized and
capped so a warning cannot bury the report that follows it.

---

## The report

| Flag | |
| --- | --- |
| `--dry-run` | print the placement and exit, binding nothing |
| `--json` | the report as JSON rather than a table |
| `--show-credentials` | print each unit's password |
| `--stdout` | answer on stdout rather than the extended-data stream real hardware uses |

The input is a real devices file, so its credentials are real, and a report that
printed them unasked would put them in terminal scrollback, CI logs and screenshots
for everyone who ever ran this. Passwords are therefore masked unless
`--show-credentials` is given; usernames are not, being no secret and needed to read
the report. Masking is a *rendering* decision and nothing more — the units accept
exactly the credentials the document gives them either way.

In JSON the masked form is `password: null`. The key stays, so the shape does not
change with the flag, but it is `null` rather than a plausible placeholder, because
a consumer that piped `[REDACTED]` into a generated devices file would get a fleet
nothing can log in to. A top-level `credentials` field says `"masked"` or `"shown"`,
so a script can assert before it trusts the field.

Logs go to stderr, deliberately, so `--json` leaves stdout a clean document.

### Closing the loop

The report carries each unit's `declared` and `listening` address side by side, so
the devices file that points a server at the *simulated* fleet can be generated from
the one that describes the real one:

```console
$ simulated-fleet devices.yaml --host 127.0.0.1 --port-base 2401 --dry-run --json \
  | jq -r '.units[] | "  - id: \(.id)\n    host: \(.listening.host)\n    port: \(.listening.port)"'
```

That emits no secret, and a generated file can inherit the credentials from its own
`defaults:` block.

---

## Two files, one name

This crate ships both schemas, and they are easy to swap:

| | |
| --- | --- |
| the **positional** argument | a *devices file* — the inventory. `devices:`, each with an `id` and a `host`. Which recorders exist |
| `--device-config`, `--device-config-for` | a *device template* — one SMP's field table. `fields:`, plus its licence block. What one recorder answers |

Passing one where the other belongs is caught in both directions, and the error says
which kind of file it got rather than listing the keys it wanted:

```console
$ simulated-fleet crates/sismatic-core/tests/fixtures/device.yaml
... unknown field `username`, expected one of defaults, device, devices, group, groups

this looks like a *device template*, not a devices file — the two schemas share a name and this crate ships both.
  a devices file lists the recorders that exist: `devices:`, each with an `id` and a `host`.
  a device template is one SMP's field table: `fields:`, plus its licence block.

  to serve a fleet *using* this template:
    simulated-fleet <devices.yaml> --device-config crates/sismatic-core/tests/fixtures/device.yaml
```

## Two failures it answers rather than merely reports

Binding a real fleet as-written fails, because a devices file names the recorders'
own addresses and those are not local interfaces — the error says so and names
`--host`. And flattening a real fleet onto one host collides, because every recorder
sits on the same standard port; that is caught *before* the first bind, as one
finding rather than one per unit:

```console
$ simulated-fleet devices.yaml --host 127.0.0.1
1 address(es) would be bound more than once:
  127.0.0.1:22023  <-  Hall A, Hall B, Hall C
  pass --port-base to lay the fleet out on consecutive ports, or --port-base 0 to let the kernel choose
```

## Shape

Everything between the file and the sockets is pure. `plan` turns a resolved
document plus a `Placement` into units, `collisions` says whether that plan can be
bound at all, `overrides_for` maps a unit onto the simulator's own `Overrides`,
`build` resolves the template layers and parses, and the renderers turn the result
into text or JSON. None of them touch the network, the clock or the filesystem,
which is why the unit tests can check the decisions that matter — the port
arithmetic, the collision it must catch, every row of the precedence table above,
that a masked report carries no secret in either format — without binding anything.
The effects are `main`'s: read the inventory, read the templates, bind, print, wait.

A planned unit holds the crate's own `Password` rather than a `String`, so it is
redacted in `Debug`, zeroized on drop, and read only through `expose_secret` — the
audit point this binary reaches for exactly twice, once to build the simulator's
overrides and once for a report that was asked to unmask. The plan type has no
`Serialize` impl and cannot acquire one by accident: the compiler refuses, because
`Password` has none. Everything that renders goes through a view type where the
secret is an `Option` that one function fills.

---

## Pointing a server at a simulated fleet

A fleet is only half the picture; the point is to run the real server against it.
Write the server a devices file naming the simulated addresses (or generate it, as
above), and a configuration:

```yaml
inventory:
  config_path: devices.yaml

# The poll schedule. A field is polled on its own interval, so the one worth
# watching can be watched closely while the forty nobody is watching stay cheap.
sync:
  interval_secs: 30        # what an entry below inherits
  fields:
    - "*"                  # every field core can query (quoted: a bare * is a YAML alias)
    - name: RUNNING_STATE
      interval_secs: 2     # ...except the one worth watching closely
    - name: ACTIVE_ALARMS
      interval_secs: 5
    - name: RECORDING_EVENTS
      interval_secs: 900   # a whole table per poll, and it changes per recording
    - name: MAC_ADDRESS
      interval_secs: 0     # ...and the one that never changes. 0 means never

store:
  retain: 2h               # how far back history goes

http:
  host: 127.0.0.1
  port: 8080
```

```console
$ sismatic-server --config-path ./configuration.yaml
$ curl -s localhost:8080/health_check -o /dev/null -w '%{http_code}\n'
200
```

Give it a few seconds: every `eager` device is dialed at startup, and the first poll
has to land before there is anything to read. `/health_check` answers 200 as soon as
the process is routing requests and deliberately says nothing about whether the
recorders are reachable — a liveness probe that failed because one was unplugged
would restart a server that was working perfectly.

Relative paths inside the config file are anchored to that file's own directory, and
every key is also reachable as `SISMATIC_SERVER__<SECTION>__<KEY>`. Every section is
`deny_unknown_fields`, so a typo fails startup naming the key rather than quietly
leaving a default in place; `sismatic-server --env-schema` prints the whole surface
as JSON.

`http://localhost:8080/api` is the Scalar UI over the server's own OpenAPI document,
generated from the handlers rather than written beside them — the quickest way to
see what the fleet can now be asked.

---

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `cannot bind 10.0.150.1:22023` | the document's addresses are the real recorders'. Pass `--host 127.0.0.1` |
| `N address(es) would be bound more than once` | every recorder is on the standard port. Pass `--port-base` |
| `unknown field 'username', expected one of defaults, device, devices…` | that is a device template, not a devices file. See *Two files, one name* |
| `cannot read device template …: missing field 'fields'` | the inverse: a devices file passed to `--device-config` |
| `--device-config-for names 'x', which is not a device in …` | the id must match the inventory; the error lists the ones that do |
| every unit answers the same `MAC_ADDRESS` / `IDENTIFIER` | they share the built-in template. Give the ones that matter a `--device-config-for` |
| `SSH_PORT` is 22023, not the port it is on | that field is what the device reports about itself, not where this program bound it |
| a unit refuses a field you expected to work | its template's licence, or the inventory's `disabled_fields`. The report's `refused fields` line is the whole set |
| `no read of 'X' for device 'Y'` on the server | that field is not in the poll schedule yet, or the first poll has not landed. `PATCH /v1/config` with a `fields` list **replaces** the schedule — keep `"*"` in it |
| ports change every run | `--port-base 0` asks the kernel to choose. Pass a real base for stable ports |
