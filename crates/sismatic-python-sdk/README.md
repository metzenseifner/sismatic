# sismatic

A blocking Python facade for driving [Extron](https://www.extron.com/) devices
over SSH using their Simple Instruction Set (SIS). The name is SIS + automatic:
it handles the SIS machinery — connection pooling, the SSH handshake, byte-level
framing — behind the scenes so you address a pool of devices by id and send
plain instructions.

The package is a thin Python layer over a compiled Rust extension
([`sismatic-core`](https://github.com/metzenseifner/sismatic/tree/main/crates/sismatic-core)),
shipped as an `abi3` wheel that covers CPython 3.9+ with a single build.

## Install

```console
pip install sismatic
```

## Quickstart

```py
from sismatic import Sis

# No connections are opened here; devices connect lazily on first use.
sis = Sis.from_file("devices.toml")

for device_id in sorted(sis.ids()):
    print(device_id)

sis.query("atrium-101", "firmware")          # read a built-in field
sis.register("atrium-101", "title", "Wk 4")  # write a metadata register
sis.command("atrium-101", "start")           # run a recorder command
```

Use optionally as a context manager to control the teardown of the session deterministically:

```py
with Sis.from_file("devices.toml") as sis:
    sis.command("atrium-101", "start")
# every SSH connection is closed on exit
```

## Configuration

### from_file

`from_file` picks the deserializer from the extension (`.toml`, `.json`,
`.yaml`/`.yml`). A config is a `defaults` table plus a list of devices:

TOML:

```toml
[defaults]
port = 22023
connect_secs = 5
command_secs = 3
eager = true              # open connections up front instead of on first use
sis_keepalive_secs = 120  # while warm, probe idle devices so connections stay warm
eager_retry_secs = 30     # while cold, retry connecting to unreachable eager devices

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
connect_secs = 10  # per-device override
command_secs = 8
```

YAML:

```yaml
---
defaults:
  username: "joe"
  password: "schmoe"
  eager: true
  sis_keepalive_secs: 120
  eager_retry_secs: 30

devices:
  - id: Hall A
    host: 10.0.150.1
  - id: Hall B
    host: 10.0.150.2
  - id: Hall C
    host: 10.0.150.3
```

### from_config

Not using a file? `Sis.from_config(mapping)` takes an already-parsed `dict`
shaped the same way, so you own the parsing. You can have a source config in
any format like INI, XML, a database row, or environment variables with any
parser that can produce a dictionary in the expected format.

Minimal Example with a configuration literal (no additonal parsing):

```py
from sismatic import Sis
import logging
logging.basicConfig(level=logging.DEBUG)

devices = {
  "defaults": {
    "username": "joe",
    "password": "schmoe",
    "eager": True,
    "sis_keepalive_secs": 120,
    "eager_retry_secs": 30,
    "port": 22023,
  },
  "devices": [
      {"id": "Hall A", "host": "https://10.0.150.1"},
      {"id": "Hall B", "host": "https://10.0.150.2"},
  ],
}
sis = Sis.from_config(devices)
print(sis.ids())
```

### Connections

Connections are lazy by default — the expensive SSH handshake is paid on a
device's first instruction. Set `eager` to pay it up front, and
`sis_keepalive_secs` to send a benign probe on an interval so a device's
inactivity timer never lets the connection go cold. Because `eager` means
"hold a warm connection over time," `eager_retry_secs` sets how often to
re-attempt the handshake for an eager device that is unreachable at startup or
has since dropped (`0` gives up after the first failure). See
[the design note on eager connections and SIS keepalive](https://github.com/metzenseifner/sismatic/blob/main/docs/sis-keepalive-eager-connections.md)
for the full rationale.

## API at a glance

```py
>>> from sismatic import Sis
>>> [m for m in dir(Sis) if not m.startswith('_')]
['close', 'command', 'from_config', 'from_file', 'from_toml', 'ids', 'query', 'register']
```

The wheel ships a PEP 561 `py.typed` marker and a type stub, so editors and
`mypy` see full signatures, including the exact accepted instruction names.
Full API docs: <https://metzenseifner.github.io/sismatic/>.

## Example: start a recording across a batch of devices

```py
# control_recording.py
# Starts recording and stamps a title across a batch of devices.

from dataclasses import dataclass
from sismatic import Sis

@dataclass(frozen=True)
class RecordingJob:
    """(device_id × title) as a product type. A job is exactly a pairing of
    the two — never one without the other — so the type says that, instead
    of the two strings being threaded separately through every call site as
    two positional arguments that happen to always travel together."""
    device_id: str
    title: str


def run_job(sis: Sis, job: RecordingJob) -> None:
    sis.register(job.device_id, "title", job.title)  # "title" — see Register::Title
    sis.command(job.device_id, "start")              # "start" — see Command::Start


def main() -> None:
    with Sis.from_file("devices.toml") as sis:
        jobs = [
            RecordingJob(device_id="atrium-101", title="Week 4 — Lecture"),
            RecordingJob(device_id="annex-far", title="Week 4 — Overflow Room"),
        ]
        for job in jobs:
            run_job(sis, job)


if __name__ == "__main__":
    main()
```

## License

ECL-2.0. Part of the [sismatic](https://github.com/metzenseifner/sismatic)
workspace.
