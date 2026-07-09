from sismatic import Sis

sis = Sis.from_toml('devices.toml')
for id in sis.ids():
    print(sis.query(id, "unit_name"))
    print(sis.query(id, "running_state"))
    print(sis.query(id, "presenter"))
