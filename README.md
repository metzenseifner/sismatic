# sismatic

An open-sourced library for working with the Simple Instruction Set used by many Extron devices.
The name comes from SIS + automatic, because it handles the SIS machinery behind the scenes without human control.

## Why?

There are several reasons why this library is worthwhile:

1. Relies on a stable, unchanging protocol.
2. Hides complexities of managing connections.
3. Hides complexities of byte-level communication.

## Crates

- core
- [python](./crates/sismatic-python/README.md)
- web
- cli

## Configuration Example

```toml
# Global settings
[defaults]
port = 22023
connect_secs = 5
command_secs = 3
eager = true # eagerly establish connections to pay the cost of the full SSH handshake up front (default: false; connect upon first instruction)
sis_keepalive_secs = 120 # interval at which to send a probe instruction to SMPs to keep connection open; SMPs idle timer is set to 5 minutes by default. (default: 120)

[[device]]
id = "atrium-101"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[device]]
id = "annex-far"
host = "10.0.0.8"
username = "admin"
password = "extron"
connect_secs = 10   # override default connect timeout
command_secs = 8    # override default command timeout
```
