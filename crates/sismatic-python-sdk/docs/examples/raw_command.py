# Send one raw SIS command and print the reply for a human to read.
# Edit HOST/USER/PASSWORD, PAYLOAD and UNTIL, then: python raw_command.py
from sismatic import Sis

PAYLOAD = "\x1bCN\r"  # ESC CN CR: read the unit name. Reboot would be "\x1b1BOOT\r".
UNTIL = "\r\n"         # stop reading once this appears in the reply

sis = Sis.from_config({
    "defaults": {"username": "admin", "password": "extron", "port": 22023},
    "devices": [{"id": "smp", "host": "10.0.0.50"}],
})

try:
    reply = sis.raw("smp", PAYLOAD, UNTIL)
    print(repr(reply))  # repr shows control characters, e.g. 'Boot1\r\n'
except RuntimeError as e:  # refusal (E13, E24, ...), timeout, or closed channel
    print("error:", e)
finally:
    sis.close()
