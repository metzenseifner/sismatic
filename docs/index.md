# sismatic

A blocking Python facade for driving [Extron](https://www.extron.com/) devices
over SSH using their Simple Instruction Set (SIS). It handles connection
pooling, the SSH handshake, and byte-level framing behind the scenes so you
address a pool of devices by id and send plain instructions.

```console
pip install sismatic
```

```py
from sismatic import Sis

with Sis.from_file("devices.toml") as sis:
    sis.register("atrium-101", "title", "Week 4 — Lecture")
    sis.command("atrium-101", "start")
```

See the **[API Reference](reference.md)** for the full `Sis` and `Alarm`
surface, generated from the type stub.

The Python package is a thin layer over the compiled Rust core. Source,
issues, and the other crates (CLI, web server) live in the
[GitHub repository](https://github.com/metzenseifner/sismatic).
