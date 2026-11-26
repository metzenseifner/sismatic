import {useState} from "react";
import {Button} from "../ui/Button";
import type {ApiCallError} from "../../api/queries";
import type {RecordingStatus} from "./deviceView";
import {writeErrorText} from "./writeErrors";
import styles from "./RecordingControl.module.css";

type RebootControlProps = {
  /** What the card believes the fleet is doing — see `LiveDeviceCard`. */
  status: RecordingStatus;
  /** True from the confirming click until the server answers. */
  pending: boolean;
  /** True once the server has accepted a reboot from this card. */
  accepted: boolean;
  error: ApiCallError | null;
  /** The locked state of the page-wide `controls` setting. */
  readOnly: boolean;
  onReboot: () => void;
};

/**
 * Restart the device, behind a second click.
 *
 * Two steps, because this is the one button on the card that cannot be taken
 * back: the device drops off the network for as long as it takes to boot, and
 * a misclick next to Stop should not cost that. The first click only arms it.
 *
 * Disabled while the card believes a recording is in progress, because the
 * server refuses then (`recording_in_progress` in `sismatic-store`'s admission
 * table), and offering a button the table refuses would be offering a 409.
 * "Mixed" counts too: a group reboot is refused all-or-nothing when any member
 * is recording. "Unknown" does not, for the same reason the registers stay
 * open then: the server's refusal is the backstop if the guess is wrong.
 */
export function RebootControl({
  status,
  pending,
  accepted,
  error,
  readOnly,
  onReboot,
}: RebootControlProps) {
  const [armed, setArmed] = useState(false);
  const recording = status === "recording" || status === "paused" || status === "mixed";
  const disabled = readOnly || pending || recording;

  return (
    <div className={styles.control}>
      <div className={styles.buttons}>
        {armed && !disabled ? (
          <>
            <Button
              type="button"
              variant="primary"
              onClick={() => {
                setArmed(false);
                onReboot();
              }}
            >
              Reboot now
            </Button>
            <Button type="button" variant="secondary" onClick={() => setArmed(false)}>
              Cancel
            </Button>
          </>
        ) : (
          <Button
            type="button"
            variant="secondary"
            disabled={disabled}
            onClick={() => setArmed(true)}
          >
            {pending ? "Rebooting…" : "Reboot…"}
          </Button>
        )}
      </div>

      {armed && !disabled && (
        <p className={styles.note}>
          The device goes offline until it has restarted.
        </p>
      )}

      {/* Only when the lock is not already the reason, which the recording
          control's note says once for the whole card. */}
      {recording && !readOnly && (
        <p className={styles.note}>Stop the recording to reboot.</p>
      )}

      {accepted && !pending && error === null && (
        <p className={styles.note}>
          Reboot queued. The device is unreachable while it restarts.
        </p>
      )}

      {error !== null && (
        <p className={styles.error} role="alert">
          {writeErrorText(error)}
        </p>
      )}
    </div>
  );
}
