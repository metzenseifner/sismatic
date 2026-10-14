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

/** One field of a group, with every member's latest value — a row of `fields`. */
type GroupFieldState = GroupFieldStateList["fields"][number];

export const RUNNING_STATE = "RUNNING_STATE";
export const TITLE = "TITLE";

/** What the badge shows. "mixed" only happens for a group whose devices disagree. */
export type RecordingStatus = "recording" | "paused" | "stopped" | "unknown" | "mixed";

/**
 * How each status is written for a reader.
 *
 * Lives here rather than in the badge because three places need the same
 * words: the badge, the control bar's state filter, and the text a free-text
 * filter searches through. A card findable by typing "paused" has to be
 * findable by the word the badge actually shows.
 */
export const STATUS_LABELS: Record<RecordingStatus, string> = {
  recording: "Recording",
  paused: "Paused",
  stopped: "Not recording",
  unknown: "Unknown",
  mixed: "Mixed",
};

export type DetailRow = {
  /** Canonical field name — what a "which details to show" setting keys on. */
  field: string;
  /** `FIRMWARE_VERSION` → `Firmware version`. */
  label: string;
  value: string;
};

/**
 * What the fleet behind this card holds for one field.
 *
 * `read` is the decoded value when there is one answer to give — one device, or
 * a group whose answering devices agree on it. A group that disagrees has no
 * single value to put in an input box, so `read` is absent and `mixed` says
 * why; a field nothing has answered yet has neither. `display` is the same
 * three cases rendered for a reader.
 */
export type FieldState = {
  read?: ReadValue;
  /** The value, "Differs across devices", or "Not read yet". */
  display: string;
  /** True for a group whose answering devices disagree. */
  mixed: boolean;
};

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
  /**
   * Every field behind this card, keyed by its canonical name.
   *
   * The raw material `details` is rendered from, and what the Registers panel
   * seeds its input boxes from. Keyed rather than a list because the two ask
   * different questions: a panel wants one field by name ("what is TITLE?"),
   * a card wants all of them in a stable order.
   */
  values: Map<string, FieldState>;
  /** Groups only. */
  members?: MemberRow[];
  sync?: GroupSyncState;
  requested?: DesiredRecordingState | null;
};

// ─── Adapters ────────────────────────────────────────────────────────────────

export function fromDeviceReads(device: DeviceReads): DeviceView {
  // One device is its own quorum: every field it answered has exactly one
  // value, so nothing here can be `mixed`.
  const values = new Map<string, FieldState>(
    device.latest.map((read) => [
      read.field,
      { read: read.value, display: formatValue(read.value), mixed: false },
    ]),
  );

  return {
    kind: "device",
    id: device.device,
    status: statusOf(values.get(RUNNING_STATE)?.read),
    title: titleOf(values.get(TITLE)?.read),
    titleMixed: false,
    updatedAt: newest(device.latest.map((read) => read.at)),
    details: detailRows(values),
    values,
  };
}

/**
 * @param configured
 *   The group's members as the inventory lists them, when that has been
 *   fetched. It is the better source and takes precedence: a group whose
 *   devices have never answered has no fields at all, so the reads below can
 *   name nobody, and a card that cannot say what is in a group is least useful
 *   exactly when the devices are unreachable. Falls back to the reads so the
 *   adapter still works on its own.
 */
export function fromGroupState(
  group: GroupFieldStateList,
  configured?: readonly string[],
): DeviceView {
  const fields = new Map(group.fields.map((field) => [field.field, field]));
  const running = fields.get(RUNNING_STATE);
  const titles = fields.get(TITLE);

  const values = new Map<string, FieldState>(
    group.fields.map((field) => [field.field, rollUp(field)]),
  );

  // Within the reads, member ids come from whichever field lists them: every
  // field lists the same members, so the first one is enough. It is the
  // fallback rather than the source, because it is empty for precisely the
  // group an operator is squinting at — see `configured` above.
  const memberIds =
    configured ?? group.fields[0]?.members.map((member) => member.device) ?? [];

  // A member the inventory lists and the reads have never mentioned comes out
  // of this with "unknown" throughout, which is the honest answer and the one
  // the badge already knows how to draw.
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
    details: detailRows(values),
    values,
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

/**
 * One group field, rolled up to the single answer a card can show.
 *
 * Silent members are skipped rather than counted as disagreement: "Differs
 * across devices" should mean the devices that answered disagree, not that one
 * of them is quiet. The server's own `uniform` flag reads the same way —
 * vacuously true until two members have reported.
 */
function rollUp(field: GroupFieldState): FieldState {
  const reads = field.members.flatMap((member) => (member.read ? [member.read] : []));
  const distinct = new Set(reads.map((read) => formatValue(read.value)));

  if (reads.length === 0) return { display: "Not read yet", mixed: false };
  if (distinct.size > 1) return { display: "Differs across devices", mixed: true };
  // Every answering member said the same thing, so any one of them will do.
  return { read: reads[0].value, display: [...distinct][0], mixed: false };
}

/**
 * Every field a card could list, in field-name order so the list does not
 * reshuffle between refetches.
 *
 * `RUNNING_STATE` is the one exclusion, because it is the recording badge and
 * repeating it as a row would be saying the same thing twice. The title is
 * *not* excluded any more: where it goes is a setting now — pinned above the
 * fold by default — and this list is what that setting chooses from.
 */
function detailRows(values: Map<string, FieldState>): DetailRow[] {
  return [...values]
    .filter(([field]) => field !== RUNNING_STATE)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([field, state]) => ({
      field,
      label: labelOf(field),
      value: state.display,
    }));
}

export type { Read };
