import {useId, type ReactNode} from "react";
import {Clapdown} from "../ui/Clapdown";
import {RecordingBadge} from "../ui/RecordingBadge";
import {
  DEFAULT_DISPLAY,
  placementOf,
  type CardDisplay,
} from "../fleet/fleetDisplay";
import {labelOf, TITLE, type MemberRow, type DeviceView} from "./deviceView";
import styles from "./DeviceCard.module.css";

type DeviceCardProps = {
  view: DeviceView;
  /**
   * Under the title: the recording controls. The slot lays itself out — it is
   * given the full width of the card, not a row of inline buttons — because
   * what goes in it is one component with its own notes and errors underneath.
   */
  actions?: ReactNode;
  /**
   * A fold of its own above "Details", for the registers. Kept a slot rather
   * than built in so that this file stays a drawing of a `DeviceView` and
   * nothing in it can write to a device; `LiveDeviceCard` is where the two
   * meet.
   */
  registers?: ReactNode;
  /**
   * How much of the card to unfold, and which detail rows to leave out. One
   * object for the page's answer to both, since it is one setting as far as
   * the control bar is concerned.
   */
  display?: CardDisplay;
};

/**
 * One recorder or one group of recorders.
 * Always visible: name, recording status, the controls, and whichever fields
 * have been pinned — the recording title, until someone says otherwise.
 * Behind "Registers": the metadata to record the next take under.
 * Behind "Details": everything else.
 */
export function DeviceCard({
  view,
  actions,
  registers,
  display = DEFAULT_DISPLAY,
}: DeviceCardProps) {
  const headingId = useId();
  const memberCount = view.members?.length ?? 0;

  // Split here rather than in the view model: where a field goes is this
  // page's business, and a field moved out of sight is still part of the card
  // as far as anything else is concerned — the text filter included.
  const details = view.details.filter(
    row => placementOf(display, row.field) === "details",
  );

  // The title keeps its own prominent treatment when it is pinned, so it is
  // not repeated among the rows below it.
  const titlePinned = placementOf(display, TITLE) === "pinned";

  // Read from `values` rather than from `details`, because a pin means *always
  // visible*: a field nobody has reported yet has no detail row, and a pinned
  // row that waited for one would leave an operator who had just pinned it
  // wondering where it went.
  const pinned = [...display.placement]
    .filter(([field, place]) => place === "pinned" && field !== TITLE)
    .map(([field]) => ({
      field,
      label: labelOf(field),
      value: view.values.get(field)?.display ?? "Not read yet",
    }))
    .sort((a, b) => a.label.localeCompare(b.label));

  return (
    <article className={styles.wrapper} aria-labelledby={headingId}>
      <div className={styles.card} data-status={view.status}>
        <header className={styles.header}>
          <div className={styles.identity}>
            <span className={styles.kind}>
              {view.kind === "group"
                ? `Group · ${memberCount} ${memberCount === 1 ? "device" : "devices"}`
                : "Device"}
            </span>
            <h3 id={headingId} className={styles.name}>
              {view.id}
            </h3>
          </div>
          <RecordingBadge status={view.status} />
        </header>

        {titlePinned && (
          <p
            className={styles.title}
            data-empty={view.title === null || undefined}
          >
            {view.title ??
              (view.titleMixed ? "Titles differ across devices" : "No title")}
          </p>
        )}

        {pinned.length > 0 && (
          <dl className={`${styles.list} ${styles.pinned}`}>
            {pinned.map(row => (
              <Row key={row.field} label={row.label} value={row.value} />
            ))}
          </dl>
        )}

        {actions && <div className={styles.actions}>{actions}</div>}

        {registers}

        <Clapdown title="Details" open={display.detailsOpen}>
          {view.kind === "group" && (
            <dl className={styles.list}>
              <Row label="Devices in sync" value={syncLabel(view.sync)} />
              <Row
                label="Requested state"
                value={requestedLabel(view.requested)}
              />
            </dl>
          )}

          {view.members && view.members.length > 0 && (
            <section>
              <h4 className={styles.subheading}>Devices</h4>
              <ul className={styles.members}>
                {view.members.map(member => (
                  <MemberItem key={member.id} member={member} />
                ))}
              </ul>
            </section>
          )}

          {details.length > 0 && (
            <section>
              {view.kind === "group" && (
                <h4 className={styles.subheading}>Fields</h4>
              )}
              <dl className={styles.list}>
                {details.map(row => (
                  <Row key={row.field} label={row.label} value={row.value} />
                ))}
              </dl>
            </section>
          )}

          <p className={styles.updated}>
            {view.updatedAt ? (
              <>
                Last read{" "}
                <time dateTime={view.updatedAt}>
                  {timeAgo(view.updatedAt)}
                </time>
              </>
            ) : (
              "Not read yet"
            )}
          </p>
        </Clapdown>
      </div>
    </article>
  );
}

function Row({label, value}: {label: string; value: string}) {
  return (
    <div className={styles.row}>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function MemberItem({member}: {member: MemberRow}) {
  return (
    <li className={styles.member}>
      <span className={styles.memberName}>
        {member.id}
        {member.sync === "drifted" && (
          <span className={styles.drift}> · drifted</span>
        )}
      </span>
      <RecordingBadge status={member.status} />
      <span className={styles.memberTitle}>{member.title ?? "No title"}</span>
    </li>
  );
}

function syncLabel(sync: DeviceView["sync"]): string {
  switch (sync) {
    case "in_sync":
      return "Yes";
    case "drifted":
      return "No, some devices drifted";
    default:
      return "Unknown";
  }
}

function requestedLabel(requested: DeviceView["requested"]): string {
  switch (requested) {
    case "recording":
      return "Recording";
    case "paused":
      return "Paused";
    case "idle":
      return "Idle";
    default:
      return "Nothing requested";
  }
}

const relative = new Intl.RelativeTimeFormat(undefined, {numeric: "auto"});

function timeAgo(iso: string): string {
  const seconds = Math.round((Date.parse(iso) - Date.now()) / 1000);
  if (Number.isNaN(seconds)) return iso;
  const abs = Math.abs(seconds);
  if (abs < 60) return relative.format(seconds, "second");
  if (abs < 3600) return relative.format(Math.round(seconds / 60), "minute");
  if (abs < 86400) return relative.format(Math.round(seconds / 3600), "hour");
  return new Date(iso).toLocaleString();
}
