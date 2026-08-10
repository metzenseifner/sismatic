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

### HTTP

The http section configures the HTTP server. Devices are protected from HTTP
floods (DDoS) because queries access the database, not the devices directly.
This means that data queried is as up-to-date as the latest query by the
synchronization runtime.
