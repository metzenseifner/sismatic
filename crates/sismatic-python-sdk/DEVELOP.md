# Development

This project uses Nix Flakes to provide a consistent development environment
across machines.

## Option A

For a fast inner loop: 

- use `maturin develop` inside `nix develop`


## Option B

There's also `nix run .#build-wheel`, which drops a portable wheel in `./dist`.
Then you'd install the python package wheel:
```py
nix build .#wheel  # -> ./result/sismatic-*.whl
python -m venv .venv && source .venv/bin/activate
pip install ./result/sismatic-*.whl
```
