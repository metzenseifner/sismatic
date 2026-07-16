
#### Iterate over package public properties

```py
>>> from sismatic import Sis
>>> [m for m in dir(Sis) if not m.startswith('_')]
['command', 'from_toml', 'ids', 'query', 'register']
```

#### List Recorders (no network)

```py
from sismatic import Sis

sis = Sis.from_toml("devices.toml")
for device_id in sorted(sis.ids()):
  print(device_id)
```

#### Start a Recording

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


def run_job(sis: sismatic, job: RecordingJob) -> None:
    sis.register(job.device_id, "title", job.title)   # "title" — see Register::Title
    sis.command(job.device_id, "start")                # "start" — see Command::Start


def main() -> None:
    sis = Sis.from_toml("devices.toml")
    jobs = [
        RecordingJob(device_id="atrium-101", title="Week 4 — Lecture"),
        RecordingJob(device_id="annex-far", title="Week 4 — Overflow Room"),
    ]
    for job in jobs:
        run_job(sis, job)


if __name__ == "__main__":
    main()
```

