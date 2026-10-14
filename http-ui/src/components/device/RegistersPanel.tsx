import {useId, type ReactNode} from "react";
import {Button} from "../ui/Button";
import {Clapdown} from "../ui/Clapdown";
import {
  MAX_REGISTER_LEN,
  type RegisterDrafts,
  type RegisterRow,
} from "./registerDrafts";
import {writeErrorText} from "./writeErrors";
import styles from "./RegistersPanel.module.css";

type RegistersPanelProps = {
  drafts: RegisterDrafts;
  /**
   * True when the card believes a recording is in progress — paused included.
   * `admit` in `sismatic-store` refuses a metadata write in either state, so
   * the boxes are disabled rather than left to collect changes the server is
   * going to throw away with `metadata_frozen`.
   */
  frozen: boolean;
};

/**
 * The registers of one recorder or one group, as boxes to fill in before a
 * recording starts.
 *
 * Every row comes from the server's own catalog, so a register added to
 * `sismatic-core` appears here without a line changing. What each box holds is
 * a *draft*: nothing is written until Save, which is what makes "clear the lot
 * and look at it before committing" possible at all.
 */
export function RegistersPanel({drafts, frozen}: RegistersPanelProps) {
  const baseId = useId();
  const ready = !drafts.catalogPending && drafts.catalogError === null;

  return (
    <Clapdown title="Registers" note={noteFor(drafts, frozen)}>
      {frozen && (
        <p className={styles.frozen}>
          A recording is in progress, so these are sealed until it stops.
        </p>
      )}

      {drafts.catalogPending && <p>Loading the register list…</p>}

      {drafts.catalogError !== null && (
        <p className={styles.error} role="alert">
          Could not load the register list. {drafts.catalogError.message}
        </p>
      )}

      {ready && drafts.rows.length === 0 && (
        <p>This server reports no writable registers.</p>
      )}

      {/* Above the boxes, not below them. Fifteen rows is longer than a phone
          screen, so a footer would put Save off the bottom of the fold —
          reachable only by scrolling past the thing you were about to save. */}
      {drafts.rows.length > 0 && (
        <div className={styles.controls}>
          <Button
            type="button"
            onClick={() => {
              // Deliberately swallowed: a refusal is already on screen beside
              // this button, from `drafts.saveError`. `save` rejects because
              // Start has to be able to wait on it — see `LiveDeviceCard`.
              void drafts.save().catch(() => {});
            }}
            disabled={frozen || drafts.saving || drafts.unsavedCount === 0}
          >
            {drafts.saving ? "Saving…" : saveLabel(drafts.unsavedCount)}
          </Button>

          <Button
            type="button"
            variant="secondary"
            onClick={drafts.clearAll}
            disabled={frozen || drafts.saving}
          >
            Clear all
          </Button>

          {drafts.hasChanges && (
            <Button
              type="button"
              variant="secondary"
              onClick={drafts.discard}
              disabled={drafts.saving}
            >
              Discard changes
            </Button>
          )}
        </div>
      )}

      {drafts.saveError !== null && (
        <p className={styles.error} role="alert">
          {writeErrorText(drafts.saveError)}
        </p>
      )}

      {drafts.rows.length > 0 && (
        <ul className={styles.rows}>
          {drafts.rows.map(row => (
            <Row
              key={row.field}
              row={row}
              id={`${baseId}-${row.field}`}
              frozen={frozen}
              onChange={drafts.set}
              onClear={drafts.clear}
            />
          ))}
        </ul>
      )}
    </Clapdown>
  );
}

function Row({
  row,
  id,
  frozen,
  onChange,
  onClear,
}: {
  row: RegisterRow;
  id: string;
  frozen: boolean;
  onChange: (field: string, value: string) => void;
  onClear: (field: string) => void;
}) {
  return (
    <li className={styles.row}>
      <label className={styles.label} htmlFor={id}>
        <span className={styles.name}>{row.label}</span>
        {row.unsaved && (
          <span className={styles.mark} data-kind="unsaved">
            unsaved
          </span>
        )}
        {row.sent && (
          <span className={styles.mark} data-kind="sent">
            sent
          </span>
        )}
      </label>

      <div className={styles.control}>
        <input
          id={id}
          className={styles.input}
          type="text"
          value={row.draft}
          maxLength={MAX_REGISTER_LEN}
          disabled={frozen}
          title={row.description}
          autoComplete="off"
          onChange={event => onChange(row.field, event.target.value)}
        />
        <button
          type="button"
          className={styles.clear}
          onClick={() => onClear(row.field)}
          disabled={frozen || !row.clearable}
          title={`Clear ${row.label}`}
          aria-label={`Clear ${row.label}`}
        >
          ×
        </button>
      </div>

      {/* Shown only once the box has stopped agreeing with the fleet —
          otherwise every row would repeat its own value back underneath
          itself. It is what an empty box means that needs saying: cleared
          here, and still set over there until the save lands. */}
      {row.unsaved && (
        <p className={styles.current}>
          On the devices: {row.current === "" ? "(empty)" : row.current}
        </p>
      )}
    </li>
  );
}

/**
 * The one thing worth reading with the fold shut.
 *
 * A failed save comes first, and in the alarming color, because of where the
 * failure usually comes from: Start writes the unsaved registers before it
 * starts anything, so a refusal there means the recording did not begin and the
 * reason is inside a fold that may well be closed. The summary line is what
 * stops that from looking like a button that did nothing.
 */
function noteFor(drafts: RegisterDrafts, frozen: boolean): ReactNode {
  if (drafts.saveError !== null) {
    return <span className={styles.noteError}>Save failed</span>;
  }
  if (drafts.unsavedCount > 0) return `${drafts.unsavedCount} unsaved`;
  if (frozen) return "Sealed";
  return undefined;
}

function saveLabel(unsaved: number): string {
  if (unsaved === 0) return "Save";
  return unsaved === 1 ? "Save 1 register" : `Save ${unsaved} registers`;
}
