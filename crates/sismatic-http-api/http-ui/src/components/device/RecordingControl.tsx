import {Button} from "../ui/Button";
import type {ApiCallError, RecordingVerb} from "../../api/queries";
import type {RecordingStatus} from "./deviceView";
import {isBenign, writeErrorText} from "./writeErrors";
import styles from "./RecordingControl.module.css";

type RecordingControlProps = {
  /** What the card believes the fleet is doing — see `LiveDeviceCard`. */
  status: RecordingStatus;
  /** The verb being submitted, from the click until the server answers. */
  pending: RecordingVerb | null;
  /** True while a verb has been accepted and no device has reported it back. */
  awaiting: boolean;
  error: ApiCallError | null;
  /** The page-wide lock — see `readOnly` in `fleetSettings`. */
  readOnly: boolean;
  onCommand: (verb: RecordingVerb) => void;
};

/**
 * The recording lifecycle, as the one or two buttons that state allows.
 *
 * One control that cycled start → pause → start would be fewer buttons and one
 * transition short: a recording in progress has two ways out, and hiding
 * either behind the other is not a thing an operator can live with mid-lecture.
 * So the state picks the primary action and, where there is a second one worth
 * having, offers it beside it.
 *
 * Under the page-wide lock the same buttons are drawn and turned off, rather
 * than taken away. Two reasons, and the first is the one that decided it: a
 * monitoring page is read by someone whose eyes are going down a column of
 * cards, and forty cards changing height the moment the lock is lifted is the
 * worst possible time to move a Stop button. The second is that the buttons
 * are themselves a reading of the state — "Stop" offered means this card
 * believes it is recording — which is worth keeping legible while locked.
 */
export function RecordingControl({
  status,
  pending,
  awaiting,
  error,
  readOnly,
  onCommand,
}: RecordingControlProps) {
  const actions = actionsFor(status);

  return (
    <div className={styles.control}>
      <div className={styles.buttons}>
        {actions.map((action, index) => (
          <Button
            key={action.verb}
            type="button"
            variant={index === 0 ? "primary" : "secondary"}
            disabled={readOnly || pending !== null}
            onClick={() => onCommand(action.verb)}
          >
            {pending === action.verb ? action.busyLabel : action.label}
          </Button>
        ))}
      </div>

      {/* Said on the card and not only in the bar, because the bar is off the
          top of the screen by the time anyone has scrolled to the card they
          care about — and a greyed-out Stop with no stated reason is exactly
          the button that looks broken.

          Kept to one line at a card's width, which is what settled the
          wording: this repeats under every card on the page, and the version
          that wrapped turned a forty-card fleet into forty two-line
          apologies. */}
      {readOnly && (
        <p className={styles.note}>Read-only — set Controls above to Unlocked.</p>
      )}

      {awaiting && pending === null && (
        <p className={styles.note}>
          Accepted. The badge moves when the device reports back.
        </p>
      )}

      {error !== null && (
        <p
          className={styles.error}
          data-benign={isBenign(error) || undefined}
          role={isBenign(error) ? "status" : "alert"}
        >
          {writeErrorText(error)}
        </p>
      )}
    </div>
  );
}

/** One button: which verb it submits, and what it says before and during. */
type Action = {verb: RecordingVerb; label: string; busyLabel: string};

const START: Action = {
  verb: "start",
  label: "Start recording",
  busyLabel: "Starting…",
};
// The same verb as START. A paused recording is resumed by starting it —
// `(Paused, Start) => Ok(Recording)` in `sismatic-store`'s admission table —
// and only the word changes, because "Start" over a paused take reads like it
// would throw the take away.
const RESUME: Action = {
  verb: "start",
  label: "Resume recording",
  busyLabel: "Resuming…",
};
const STOP: Action = {
  verb: "stop",
  label: "Stop recording",
  busyLabel: "Stopping…",
};
const PAUSE: Action = {verb: "pause", label: "Pause", busyLabel: "Pausing…"};

/**
 * Which buttons a state gets, primary first.
 *
 * Reads off `admit` in `sismatic-store`, the table that decides what the server
 * will actually accept: from stopped only a start is admitted, from recording a
 * stop and a pause, from paused a resume and a stop. Offering a button the
 * table refuses would be offering a 409.
 */
function actionsFor(status: RecordingStatus): Action[] {
  switch (status) {
    case "stopped":
      return [START];
    case "recording":
      return [STOP, PAUSE];
    case "paused":
      return [RESUME, STOP];
    // Nothing has answered yet, or a group's recorders disagree. Both ways out
    // are meaningful — bring them all up, or bring them all down — so both are
    // offered rather than one of them guessed at.
    default:
      return [START, STOP];
  }
}
