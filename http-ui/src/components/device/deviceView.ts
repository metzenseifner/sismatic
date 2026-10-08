// Turns the two API shapes (one device's reads, one group's field states) into
// the single shape the card draws. The card never sees the wire format, so it
// stays the same whether it is showing one recorder or a whole room.
import type {
  DesiredRecordingState,
  DeviceReads,
  GroupFieldStateList,
  GroupSyncState,
  Read,
  ReadValue,
} from "../../api/types";

export const RUNNING_STATE = "RUNNING_STATE";
export const TITLE = "TITLE";

/** What the badge shows. "mixed" only happens for a group whose devices disagree. */
export type RecordingStatus = "recording" | "paused" | "stopped" | "unknown" | "mixed";

export type DetailRow = { label: string; value: string };

export type MemberRow = {
  id: string;
  status: RecordingStatus;
  title: string | null;
  sync: GroupSyncState;
};

export type DeviceView = {
  kind: "device" | "group";
  id: string;
  /** Essential: is it recording? */
  status: RecordingStatus;
  /** Essential: the recording title. `null` means none is set or none has been read yet. */
  title: string | null;
  /** True for a group whose devices report different titles. */
  titleMixed: boolean;
  /** The newest read time behind this card, as an ISO 8601 string. */
  updatedAt: string | null;
  /** Everything else, shown under "Details". */
  details: DetailRow[];
  /** Groups only. */
  members?: MemberRow[];
  sync?: GroupSyncState;
  requested?: DesiredRecordingState | null;
};

// ─── Adapters ────────────────────────────────────────────────────────────────

export function fromDeviceReads(device: DeviceReads): DeviceView {
  const byField = new Map(device.latest.map((read) => [read.field, read]));

  return {
    kind: "device",
    id: device.device,
    status: statusOf(byField.get(RUNNING_STATE)?.value),
    title: titleOf(byField.get(TITLE)?.value),
    titleMixed: false,
    updatedAt: newest(device.latest.map((read) => read.at)),
    details: device.latest
      .filter((read) => read.field !== RUNNING_STATE && read.field !== TITLE)
      .sort(byFieldName)
      .map((read) => ({ label: labelOf(read.field), value: formatValue(read.value) })),
  };
}

export function fromGroupState(group: GroupFieldStateList): DeviceView {
  const fields = new Map(group.fields.map((field) => [field.field, field]));
  const running = fields.get(RUNNING_STATE);
  const titles = fields.get(TITLE);

  // Member ids come from whichever field lists them; every field lists the
  // same members, so the first one is enough.
  const memberIds = group.fields[0]?.members.map((member) => member.device) ?? [];

  const members: MemberRow[] = memberIds.map((id) => {
    const runningMember = running?.members.find((member) => member.device === id);
    const titleMember = titles?.members.find((member) => member.device === id);
    return {
      id,
      status: statusOf(runningMember?.read?.value),
      title: titleOf(titleMember?.read?.value),
      sync: runningMember?.sync ?? "unknown",
    };
  });

  // A member that has reported nothing is not evidence of disagreement. A group
  // where two devices report "Morning session" and a third has answered nothing
  // holds one title, not mixed titles — and "mixed" is an alarming word to show
  // for a fleet where some recorder is always unreachable. The silence is still
  // visible, per member below and in `sync`, which is where it belongs. The
  // server's own `uniform` flag reads the same way: vacuously true until two
  // members have reported.
  const reportedStatuses = new Set(
    members.map((member) => member.status).filter((status) => status !== "unknown"),
  );
  const reportedTitles = new Set(
    members.flatMap((member) => (member.title === null ? [] : [member.title])),
  );

  const allReads = group.fields.flatMap((field) =>
    field.members.flatMap((member) => (member.read ? [member.read] : [])),
  );

  return {
    kind: "group",
    id: group.group,
    status:
      reportedStatuses.size === 0
        ? "unknown"
        : reportedStatuses.size === 1
          ? [...reportedStatuses][0]
          : "mixed",
    title: reportedTitles.size === 1 ? [...reportedTitles][0] : null,
    titleMixed: reportedTitles.size > 1,
    updatedAt: newest(allReads.map((read) => read.at)),
    details: group.fields
      .filter((field) => field.field !== RUNNING_STATE && field.field !== TITLE)
      .sort(byFieldName)
      .map((field) => {
        // Silent members are skipped for the same reason the status roll-up
        // skips them: "Differs across devices" should mean the devices that
        // answered disagree, not that one of them is quiet.
        const values = new Set(
          field.members.flatMap((member) =>
            member.read ? [formatValue(member.read.value)] : [],
          ),
        );
        return {
          label: labelOf(field.field),
          value:
            values.size === 0
              ? "Not read yet"
              : values.size === 1
                ? [...values][0]
                : "Differs across devices",
        };
      }),
    members,
    sync: group.sync,
    requested: group.desired_recording_state ?? null,
  };
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function statusOf(value: ReadValue | undefined): RecordingStatus {
  if (value?.type !== "state") return "unknown";
  switch (value.value) {
    case "started":
      return "recording";
    case "paused":
      return "paused";
    case "stopped":
      return "stopped";
    default:
      return "unknown";
  }
}

function titleOf(value: ReadValue | undefined): string | null {
  if (value?.type !== "text") return null;
  const title = value.value.trim();
  return title === "" ? null : title;
}

/** Plain-English text for any read value. */
export function formatValue(value: ReadValue): string {
  switch (value.type) {
    case "text":
    case "version":
    case "ack":
    case "mac":
      return value.value;
    case "port":
    case "number":
      return String(value.value);
    case "flag":
      return value.value ? "Yes" : "No";
    case "state":
      return value.value.charAt(0).toUpperCase() + value.value.slice(1);
    case "alarms":
      return value.value.length === 0
        ? "None"
        : value.value.map((alarm) => `${alarm.name} (${alarm.level})`).join(", ");
  }
}

/** `FIRMWARE_VERSION` → `Firmware version`. */
export function labelOf(field: string): string {
  const words = field.toLowerCase().replaceAll("_", " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

function newest(timestamps: string[]): string | null {
  // ISO 8601 strings in the same time zone sort correctly as plain text.
  return timestamps.length === 0 ? null : timestamps.reduce((a, b) => (a > b ? a : b));
}

function byFieldName(a: { field: string }, b: { field: string }): number {
  return a.field.localeCompare(b.field);
}

export type { Read };
