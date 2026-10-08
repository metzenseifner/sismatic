import type { RecordingStatus } from "../device/deviceView";
import styles from "./RecordingBadge.module.css";

const LABELS: Record<RecordingStatus, string> = {
  recording: "Recording",
  paused: "Paused",
  stopped: "Not recording",
  unknown: "Unknown",
  mixed: "Mixed",
};

/**
 * The one thing a person scanning a wall of cards must not miss.
 * Color is never the only signal: the label and the dot shape say it too.
 */
export function RecordingBadge({ status }: { status: RecordingStatus }) {
  return (
    <span className={styles.badge} data-status={status}>
      <span className={styles.dot} aria-hidden="true" />
      {LABELS[status]}
    </span>
  );
}
