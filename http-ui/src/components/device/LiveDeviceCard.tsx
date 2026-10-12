// One card, wired to the write side.
//
// `DeviceCard` draws; this decides. It exists because the Start button and the
// Registers panel are not independent: a recording seals metadata the instant
// the server admits it, so the two have to share one piece of state — what is
// still typed into the boxes — and one order of operations.
import {useEffect, useMemo, useState} from "react";
import {
  useRecordingCommand,
  type RecordingVerb,
  type WriteTarget,
} from "../../api/queries";
import {DeviceCard} from "./DeviceCard";
import {RecordingControl} from "./RecordingControl";
import {RegistersPanel} from "./RegistersPanel";
import {useRegisterDrafts} from "./registerDrafts";
import type {CardDisplay} from "../fleet/fleetDisplay";
import type {DeviceView, RecordingStatus} from "./deviceView";

/** The reported status that confirms a verb this page submitted. */
const CONFIRMS: Record<RecordingVerb, RecordingStatus> = {
  start: "recording",
  stop: "stopped",
  pause: "paused",
};

/**
 * How long a verb this page had accepted is believed over the devices' own
 * reports.
 *
 * There has to be a limit. Nothing under `/v1/writes` touches a device, so an
 * accepted start is a row in an outbox; if the relay cannot reach the recorder
 * the write fails, the desired state is rolled back, and *nothing will ever
 * report what was asked for*. A belief with no expiry would sit there offering
 * Stop for a recording that never began, with the registers sealed behind it.
 * Fifteen seconds is long enough for a healthy device on the shipped relay
 * cadence, and short enough not to mislead for the rest of the afternoon.
 */
const BELIEF_MS = 15_000;

export function LiveDeviceCard({
  view,
  display,
}: {
  view: DeviceView;
  display: CardDisplay;
}) {
  const target = useMemo<WriteTarget>(
    () => ({kind: view.kind, id: view.id}),
    [view.kind, view.id],
  );

  const command = useRecordingCommand(target);
  const drafts = useRegisterDrafts(target, view);

  /** The verb being submitted, from the click until the server answers. */
  const [inFlight, setInFlight] = useState<RecordingVerb | null>(null);
  /** The last verb the server accepted, until a device confirms it or it ages out. */
  const [accepted, setAccepted] = useState<RecordingVerb | null>(null);

  // The badge shows what a device last *reported*; admission is decided against
  // what the outbox has *accepted*. Those two disagree for as long as the relay
  // takes to reach the device, and that gap is where "typed a title into a card
  // whose recording had already started" lives. A verb this page submitted and
  // had accepted is the one piece of the outbox's state the page knows for
  // free, so the buttons and the seal are driven by it while it lasts.
  //
  // Derived rather than stored, so the belief lapses the moment a device
  // reports what was asked for: from then on the device is the better
  // authority and this page has nothing left to add.
  const believed =
    accepted === null || view.status === CONFIRMS[accepted]
      ? null
      : CONFIRMS[accepted];
  const status = believed ?? view.status;
  const awaiting = believed !== null;

  // The hard expiry. `accepted` alone is the dependency, so the clock runs from
  // the moment the verb was accepted instead of restarting every time a device
  // reports something — a belief whose timer could be reset by the thing it is
  // meant to outlive would never expire.
  //
  // One consequence, taken knowingly: a recording stopped from the recorder's
  // own front panel within the window re-enters the "believed" branch, and the
  // buttons lag the badge until the clock runs out. Fifteen seconds of a stale
  // button beats the alternative, which is a page that cannot show an operator
  // their own start until a device gets round to mentioning it.
  useEffect(() => {
    if (accepted === null) return;
    const expiry = setTimeout(() => setAccepted(null), BELIEF_MS);
    return () => clearTimeout(expiry);
  }, [accepted]);

  const run = (verb: RecordingVerb) => {
    setInFlight(verb);
    void (async () => {
      // Anything still in the Registers boxes has to land *before* the start
      // does: a recording seals its metadata for the whole epoch, so an edit
      // submitted afterwards is refused with `metadata_frozen` and the take is
      // recorded under the previous title. Awaiting the writes' `202`s is
      // enough to fix both halves of that — the server admits in arrival
      // order, and the outbox drains per device in submission order, so the
      // recorder sees the registers before it sees the start.
      //
      // Only when the registers are writable at all, which is also the only
      // case where a start opens a new take: from paused, "start" is a resume
      // — `opens_recording` in `sismatic-store` does not count it — and the
      // epoch's metadata was sealed when it began. Flushing there could only
      // produce the `metadata_frozen` that would then block the resume, for
      // the sake of an edit that has nowhere to go.
      if (verb === "start" && !sealsMetadata(status)) await drafts.save();
      await command.mutateAsync(verb);
      setAccepted(verb);
    })()
      .catch(() => {
        // Already on screen: a refused register write under the Registers
        // fold — and on its summary line, which is visible with the fold shut —
        // and a refused verb under the buttons themselves.
      })
      .finally(() => setInFlight(null));
  };

  return (
    <DeviceCard
      view={view}
      display={display}
      actions={
        <RecordingControl
          status={status}
          pending={inFlight}
          awaiting={awaiting}
          error={command.error}
          onCommand={run}
        />
      }
      registers={
        <RegistersPanel drafts={drafts} frozen={sealsMetadata(status)} />
      }
    />
  );
}

/**
 * Whether metadata is sealed in this state.
 *
 * `admit` in `sismatic-store` refuses a metadata write while the desired state
 * is recording *or* paused — a paused recording is still a recording, and the
 * take in progress is no more writable than it was a moment ago. "Mixed" means
 * at least one member of a group is in one of those, and a group metadata write
 * is refused all-or-nothing rather than leaving the group half-written, so that
 * is sealed too.
 *
 * "Unknown" is not. Nothing has reported yet, which is exactly the moment an
 * operator is filling the registers in for the first time; disabling the boxes
 * then would be refusing the main thing this panel is for, on no evidence. The
 * server's own refusal is the backstop if the guess turns out to be wrong.
 */
function sealsMetadata(status: RecordingStatus): boolean {
  return status === "recording" || status === "paused" || status === "mixed";
}
