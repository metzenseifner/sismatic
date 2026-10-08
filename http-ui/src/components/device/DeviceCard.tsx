import {useId, type ReactNode} from "react";
import {RecordingBadge} from "../ui/RecordingBadge";
import type {MemberRow, DeviceView} from "./deviceView";
import styles from "./DeviceCard.module.css";

type DeviceCardProps = {
  view: DeviceView;
  /** Optional slot for buttons such as Start and Stop, added later. */
  actions?: ReactNode;
};

/**
 * One recorder or one group of recorders.
 * Always visible: name, recording status, recording title.
 * Behind "Details": everything else.
 */
export function DeviceCard({view, actions}: DeviceCardProps) {
  const headingId = useId();
  const memberCount = view.members?.length ?? 0;

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

        <p
          className={styles.title}
          data-empty={view.title === null || undefined}
        >
          {view.title ??
            (view.titleMixed ? "Titles differ across devices" : "No title")}
        </p>

        {actions && <div className={styles.actions}>{actions}</div>}

        <details className={styles.details}>
          <summary className={styles.summary}>Details</summary>

          <div className={styles.detailsBody}>
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

            {view.details.length > 0 && (
              <section>
                {view.kind === "group" && (
                  <h4 className={styles.subheading}>Fields</h4>
                )}
                <dl className={styles.list}>
                  {view.details.map(row => (
                    <Row key={row.label} label={row.label} value={row.value} />
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
          </div>
        </details>
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
