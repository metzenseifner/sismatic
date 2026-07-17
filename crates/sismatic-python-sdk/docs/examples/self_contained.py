from sismatic import Sis
import logging
logging.basicConfig(level=logging.DEBUG)

devices = {
        "defaults": {
            "username": "joe",
            "password": "schmoe",
            "eager": True,
            "sis_keepalive_secs": 120,
            "port": 22023,
            },
        "device": [
            {"id": "Hall A", "host": "https://10.0.150.1"},
            {"id": "Hall B", "host": "https://10.0.150.2"}
            ],
        }
sis = Sis.from_config(devices)
print(sis.ids())
