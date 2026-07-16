import time
from sismatic import Sis

import logging
logging.basicConfig(level=logging.DEBUG)

sis = Sis.from_toml('devices.toml')
for id in sis.ids():
    print(sis.query(id, "running_state"))
    print(sis.query(id, "unit_name"))
    print(sis.query(id, "presenter"))
    sis.register(id, "title", "Jonathan and Ed Abuse Extron over SIS")
    sis.register(id, "subject", "Testing Sismatic")
    sis.command(id, "start")
    print(sis.query(id, "running_state"))
    time.sleep(10)
    print(sis.query(id, "running_state"))
    print(sis.query(id, "title"))
    print(sis.query(id, "subject"))
    sis.command(id, "pause")
    time.sleep(5)
    sis.command(id, "start")
    time.sleep(10)
    sis.command(id, "stop")
