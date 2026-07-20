# sismatic

An open-sourced library for working with the Simple Instruction Set used by many Extron devices.
The name comes from SIS + automatic, because it handles the SIS machinery behind the scenes without human control.

## Why?

There are several reasons why this library is worthwhile:

1. Relies on a stable, unchanging protocol.
2. Hides complexities of managing connections.
3. Hides complexities of byte-level communication.

## Crates

- [core](./crates/sismatic-core)
- [python](./crates/sismatic-python-sdk/README.md)
- [web](./crates/sismatic-web)
- [cli](./crates/sismatic-cli)

## Configuration Example

```toml
# Global settings
[defaults]
port = 22023
connect_secs = 5
command_secs = 3
eager = true # eagerly establish connections to pay the cost of the full SSH handshake up front (default: false; connect upon first instruction)
sis_keepalive_secs = 120 # while warm, interval at which to send a probe instruction to SMPs to keep the connection open; SMPs' idle timer is 5 minutes by default. 0 disables. (default: 120)
eager_retry_secs = 30 # while eager but cold, interval at which to retry connecting to a device that is unreachable or has dropped. 0 disables (give up after the first failed connect). (default: 30)

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

# Warm versus Cold Connections

The premise is that the SSH handshakes are expensive, so having a "warm" or
preestablished connection is important for runtime responsiveness. Device
connections are lazy by default, which means that connection is delayed and
first established when first instruction is sent. After a connection is
established, the SSH layer will automatically keep the connection alive until
the device itself terminates the connection due to inactivity (no instructions
sent for some interval). The default interval is 5 minutes on some devices
after which the connection will go "cold" again and be reestablished upon the
next command. If desired, it is possible to eagerly open connections to devices
by settings the `eager` configuration option. This will enable maximum
performance at runtime because it will cause connections to devices to open up
front i.e. pay the cost of the full SSH handshake at startup. To keep the
connections "warm", there is the `sis_keepalive_secs` setting, which sets an
interval in which a benign instruction is sent to the device to reset the
device's inactivity timer periodically.

`eager` is a standing intent to hold a warm connection, not just a one-time
connect at startup. A device that is unreachable when the process starts — or
that drops later — would otherwise stay cold until the next real command. The
`eager_retry_secs` setting closes that gap: while a device is eager but cold, the
background task re-attempts the SSH handshake on this interval until the device
answers again and returns to the warm keepalive cadence. Set it to `0` to restore
the old behavior of giving up after the first failed connect.
