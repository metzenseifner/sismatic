# Development

This project uses Nix Flakes to provide a consistent development environment
across machines.

## Option A

For a fast inner loop: 

- use `maturin develop` inside `nix develop`

The Nix Flake devShell idempotently creates a Python virtual environment (venv)
called "sis-venv" at the root directory, which ensures both the Flake
dependencies and Python utilities (python, pip) are correctly added to the PATH
upon direnv activation. Running maturin develop will load the latest sismatic package
into Python so that it suffices to simply `import sismatic`.

## Option B

There's also `nix run .#build-wheel`, which drops a portable wheel in `./dist`.
Then you'd install the python package wheel:
```py
nix build .#wheel  # -> ./result/sismatic-*.whl
python -m venv .venv && source .venv/bin/activate
pip install ./result/sismatic-*.whl
```
