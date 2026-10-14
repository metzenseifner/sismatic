import { STATUS_LABELS, type RecordingStatus } from "../device/deviceView";
import styles from "./RecordingBadge.module.css";

/**
 * The one thing a person scanning a wall of cards must not miss.
 * Color is never the only signal: the label and the dot shape say it too.
 */
export function RecordingBadge({ status }: { status: RecordingStatus }) {
  return (
    <span className={styles.badge} data-status={status}>
      <span className={styles.dot} aria-hidden="true" />
      {STATUS_LABELS[status]}
    </span>
  );
}
