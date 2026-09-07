# Server

The Sismatic Server starts up an HTTP Server, database, and a set of threads
that synchronous device state with the database.

## Run

From within `sismatic-server`,

```rust
cargo run -- -c configuration.yaml
```

## Configuration

By default, a `configuration.yaml` (or other toml) file is read from the current working directory. Optionally, you can provide input on the command line.

### Devices Configuration Path

The devices configuration describes your device topology, and will be passed to
the Sismatic Core as-is. The path is relative to the server configuration file
itself. The CLI parameter is relative to the process's working directory. The
devices configuration is intentionally kept separate to reduce complexity of
the configuration by separating concerns, and also because it could potentially
be shared by other processes.

### Sync

The sync section configuration controls the synchronization runtime—threads
that query the devices directly. You can control each field that should be
queried and provide both global and field-specific intervals to query. The
wildcard character, asterisk (`*` must be single-quoted in YAML), may be used
to as a catch-all to avoid spelling out each fields. Specific fields hold
higher precedence than the wildcard, meaning that specific fields' settings
will override wildcard settings.

### Store

The store section bounds what the store keeps. It matters most for the
in-memory store, whose history would otherwise grow for as long as the process
runs — making memory use a function of uptime rather than of the fleet.

```yaml
store:
  retain: 24h          # the oldest read to keep
  cleanup_interval: 5min
  max_memory: 256MiB
```

**`retain`** — the oldest read worth keeping, in any of three forms:

| Form | Examples | Meaning |
| --- | --- | --- |
| Rolling window | `30d`, `2 weeks`, `1h 30min`, `12h` | Keep the last *N* of history, measured from now at each cleanup |
| Fixed floor | `2026-01-01T00:00:00Z`, `2026-01-01` | Keep everything at or after that instant; a bare date is midnight UTC |
| Off | `forever` | Never expire anything |

Durations use [systemd's time-span
spelling](https://www.freedesktop.org/software/systemd/man/systemd.time.html) —
terms of `<number><unit>`, summed, with `s`/`min`/`h`/`d`/`w`/`M`/`y` and their
long forms. Prefer a rolling window: it is the only form whose memory cost
stops growing. A fixed floor widens by a day every day, so it needs
`max_memory` under it.

A bare `0` is rejected here rather than guessed at, because "off" and "keep
nothing" are opposite readings of it and only one of them is safe. Write
`forever`.

**`cleanup_interval`** — how often expired reads are actually deleted, and so
also how far past `retain` one can linger. `never` (or `0`) starts no sweeper
at all, which leaves `max_memory` as the only bound. Same duration spelling as
`retain`.

**`max_memory`** — the store's byte budget, and the setting that keeps an
unattended server alive. `512MiB`, `2GB`, `1.5GiB`, a plain number of bytes, or
`unlimited`. `KiB`/`MiB`/`GiB`/`TiB` and the bare `K`/`M`/`G`/`T` are powers of
1024 (as in `systemd`'s `MemoryMax=`); `KB`/`MB`/`GB`/`TB` are powers of 1000.

It is a cap, not a reservation — nothing is allocated up front — and it is
enforced on **every write**, not at each cleanup, so a burst arriving between
sweeps cannot exhaust the machine. When the store is full, the oldest reads go
first, across the whole fleet rather than per device.

The budget is a backstop, not the plan. `retain` is what an operator reasons
about; `max_memory` catches the case where the retention window turned out to
cost more than expected, because the bytes it costs depend on the fleet size
and the poll schedule. If the budget starts discarding reads the window would
have kept, the server says so and names both settings.

The latest reading of each field is never expired or evicted. It is bounded by
the configuration rather than by uptime, and dropping it would make a recorder
that went quiet look like one that never reported.

Every key also accepts a plain integer — seconds for the durations, bytes for
the size — which is what makes the section reachable from the environment:

```sh
SISMATIC_SERVER__STORE__RETAIN=7d
SISMATIC_SERVER__STORE__CLEANUP_INTERVAL=10min
SISMATIC_SERVER__STORE__MAX_MEMORY=2GiB
```

### HTTP

The http section configures the HTTP server. Devices are protected from HTTP
floods (DDoS) because queries access the database, not the devices directly.
This means that data queried is as up-to-date as the latest query by the
synchronization runtime.
