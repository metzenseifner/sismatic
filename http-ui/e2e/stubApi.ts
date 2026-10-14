// A fleet, served to the browser without a server.
//
// The tests stub `/v1/**` rather than starting `sismatic-server` beside them.
// Three reasons, in order of how much they matter:
//
//  1. a test that owns its fleet can assert on it. "Is Hall A recording" has an
//     answer here and does not depend on what some recorder on 10.0.150.1 was
//     doing when the suite ran — or on that recorder existing.
//  2. it keeps the suite runnable from a clean checkout with nothing but Node.
//     No cargo build, no devices file, no network.
//  3. it is the only way to produce the states that matter and are hard to
//     arrange for real: a group whose members disagree, a field no device has
//     ever answered, a recording already in progress.
//
// What it gives up is the contract itself — that the server really does answer
// in these shapes. That belongs in the Rust integration tests, which already
// own it, and in `pnpm gen:api`, which makes a drift between these shapes and
// the real ones a type error rather than a surprise at runtime.
import type {Page} from "@playwright/test";
import type {
  DeviceReads,
  FieldCatalog,
  GroupFieldStateList,
  GroupSummary,
  Read,
  ReadValue,
} from "../src/api/types";

const AT = "2026-10-13T09:00:00Z";

function read(device: string, field: string, value: ReadValue): Read {
  return {device, field, value, at: AT};
}

export const HALL_A: DeviceReads = {
  device: "Hall A",
  latest: [
    read("Hall A", "RUNNING_STATE", {type: "state", value: "started"}),
    read("Hall A", "TITLE", {type: "text", value: "Morning session"}),
    read("Hall A", "PRESENTER", {type: "text", value: "Jörg Müller"}),
    read("Hall A", "FIRMWARE", {type: "version", value: "2.11"}),
  ],
};

export const HALL_B: DeviceReads = {
  device: "Hall B",
  latest: [
    read("Hall B", "RUNNING_STATE", {type: "state", value: "stopped"}),
    read("Hall B", "TITLE", {type: "text", value: "Afternoon lab"}),
    read("Hall B", "FIRMWARE", {type: "version", value: "2.11"}),
  ],
};

/** A device that has never answered — an empty `latest`, not an absent row. */
export const HALL_C: DeviceReads = {device: "Hall C", latest: []};

/** A group whose members disagree, which is the only way to see "Mixed". */
export const HALLS: GroupFieldStateList = {
  group: "Halls",
  sync: "drifted",
  desired_recording_state: "recording",
  fields: [
    {
      group: "Halls",
      field: "RUNNING_STATE",
      sync: "drifted",
      uniform: false,
      members: [
        {
          device: "Hall A",
          sync: "in_sync",
          read: read("Hall A", "RUNNING_STATE", {type: "state", value: "started"}),
        },
        {
          device: "Hall B",
          sync: "drifted",
          read: read("Hall B", "RUNNING_STATE", {type: "state", value: "stopped"}),
        },
        {device: "Hall C", sync: "unknown", read: null},
      ],
    },
    {
      group: "Halls",
      field: "TITLE",
      sync: "unknown",
      uniform: false,
      members: [
        {
          device: "Hall A",
          sync: "unknown",
          read: read("Hall A", "TITLE", {type: "text", value: "Morning session"}),
        },
        {
          device: "Hall B",
          sync: "unknown",
          read: read("Hall B", "TITLE", {type: "text", value: "Afternoon lab"}),
        },
        {device: "Hall C", sync: "unknown", read: null},
      ],
    },
  ],
};

/**
 * What the group is configured to contain, which is not what the read routes
 * report. The two are deliberately allowed to disagree in these fixtures: a
 * group with no fields still has members here, which is the whole case the
 * inventory route exists to cover.
 */
const INVENTORY: GroupSummary[] = [
  {
    id: "Halls",
    members: ["Hall A", "Hall B", "Hall C"],
    barrier_timeout_secs: 8,
    barrier: "fail_batch",
  },
];

/** A group the devices have never answered for — `fields` really is empty. */
export const SILENT_HALLS: GroupFieldStateList = {
  group: "Halls",
  sync: "unknown",
  desired_recording_state: "idle",
  fields: [],
};

const FIELDS: FieldCatalog = {
  fields: [
    {name: "RUNNING_STATE", aliases: [], description: "Current recording state."},
    {name: "TITLE", aliases: [], description: "Recording title."},
    {name: "PRESENTER", aliases: [], description: "Presenter name."},
    {name: "FIRMWARE", aliases: [], description: "Firmware version."},
    {name: "TIMEZONE", aliases: [], description: "Configured timezone."},
  ],
};

const WRITES = {
  commands: [
    {name: "STARTRECORDING", aliases: ["START"], description: "Start recording."},
    {name: "STOPRECORDING", aliases: ["STOP"], description: "Stop recording."},
    {name: "PAUSERECORDING", aliases: ["PAUSE"], description: "Pause recording."},
  ],
  metadata: [
    {name: "PRESENTER", aliases: [], description: "Presenter name."},
    {name: "TITLE", aliases: [], description: "Recording title."},
  ],
  settings: [{name: "TIMEZONE", aliases: [], description: "Configured timezone."}],
};

export type Fleet = {
  devices?: DeviceReads[];
  groups?: GroupFieldStateList[];
  /** Pass `[]` to model the inventory route being unreachable. */
  inventory?: GroupSummary[];
};

/**
 * Answer every `/v1` call this page makes, and record the writes it submits.
 *
 * Installed before `page.goto`, so the first render already has data and no
 * test has to wait out a loading state it did not mean to assert on. The
 * returned array is the write side: a test presses Start and then reads what
 * the page actually sent, which is the only honest way to check that — nothing
 * under `/v1/writes` reaches a device even in production.
 */
export type Submitted = {
  method: string;
  /**
   * The path with its escapes undone, which is what a test wants to read.
   *
   * `raw` keeps the form that went down the wire. The two differ for every id
   * with a space in it — `Hall C` travels as `Hall%20C` — and that is correct
   * rather than incidental: an unescaped space would not reach the route at
   * all. Worth one assertion of its own somewhere, which is why both are kept.
   */
  path: string;
  raw: string;
  body: unknown;
};

export async function stubApi(
  page: Page,
  fleet: Fleet = {},
): Promise<Submitted[]> {
  const submitted: Submitted[] = [];
  const devices = fleet.devices ?? [HALL_A, HALL_B, HALL_C];
  const groups = fleet.groups ?? [HALLS];
  const inventory = fleet.inventory ?? INVENTORY;

  const json = (body: unknown, status = 200) => ({
    status,
    contentType: "application/json",
    body: JSON.stringify(body),
  });

  // Mutually exclusive patterns rather than an order they happen to be
  // registered in: a glob with no trailing wildcard has to end there, so
  // `**/v1/reads` does not catch `/v1/reads/devices` and `**/v1/writes/**`
  // does not catch `/v1/writes`. Nothing here depends on which handler
  // Playwright would prefer.
  await page.route("**/v1/reads/devices**", route =>
    route.fulfill(json({devices, next: null})),
  );
  await page.route("**/v1/reads/groups**", route =>
    route.fulfill(json({groups, next: null})),
  );
  await page.route("**/v1/reads", route => route.fulfill(json(FIELDS)));
  await page.route("**/v1/inventory/groups", route =>
    route.fulfill(json({groups: inventory})),
  );
  await page.route("**/v1/writes", route => route.fulfill(json(WRITES)));

  await page.route("**/v1/writes/**", async route => {
    const request = route.request();
    const {pathname} = new URL(request.url());
    submitted.push({
      method: request.method(),
      path: decodeURIComponent(pathname),
      raw: pathname,
      body: request.postData() === null ? null : request.postDataJSON(),
    });
    await route.fulfill(
      json({batch: null, writes: [{id: "w1", device: "Hall A", epoch: 1}]}, 202),
    );
  });

  return submitted;
}
