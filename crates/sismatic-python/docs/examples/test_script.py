from sismatic import Sis

# Use `Sis` as a context manager so its SSH connections and tokio runtime are
# torn down deterministically on exit, rather than during interpreter shutdown.
with Sis.from_toml('devices.toml') as sis:
    for id in sis.ids():
        print(sis.query(id, "unit_name"))
        print(sis.query(id, "running_state"))
        print(sis.query(id, "presenter"))
