from sismatic import Sis
import time
import logging
logging.basicConfig(level=logging.INFO)

sis = Sis.from_file("./devices.toml")
while True:
    print(sis.ids())
    time.sleep(5)
