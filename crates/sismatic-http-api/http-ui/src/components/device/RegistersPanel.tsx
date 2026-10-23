import {useId, type ReactNode} from "react";
import {Button} from "../ui/Button";
import {Clapdown} from "../ui/Clapdown";
import {
  MAX_REGISTER_LEN,
  type RegisterDrafts,
  type RegisterRow,
} from "./registerDrafts";
import {writeErrorText} from "./writeErrors";
import type {ControlsMode} from "../fleet/fleetSettings";
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
  /**
   * The page-wide setting — see `controls` in `fleetSettings`.
   *
   * A second, unrelated reason the same boxes are off, and deliberately a
   * second prop rather than folded into `frozen`: they are reversed by
   * different people in different places. A seal lifts when the recording
   * stops, and no amount of clicking in this browser will lift it sooner; the
   * lock lifts from a select in the bar, and nothing on any device cares.
   * Saying only "sealed" for the second would send an operator to wait for a
   * recording to end over a setting they could change in a second.
   *
   * `hidden` is the state where this panel keeps every row and loses
   * everything that could write one — no Save, no Clear, no × — and says
   * nothing about their absence, since being shown less was the request.
   */
  controls: ControlsMode;
};

/**
 * What a box is: open to typing, turned off, or a value to read.
 *
 * One value rather than a pair of booleans, because the three are exclusive
 * and the pair has a fourth state that must never be rendered — `disabled`
 * and `readonly` together, a box greyed out *and* claiming to be the only
 * legible copy of its value.
 */
type RowState = "open" | "off" | "read";

/**
 * Which of the three a row is in.
 *
 * The seal and the page's lock collapse into one `off` on purpose: an input
 * cannot say which of them turned it off, and the paragraph above the rows
 * says it once for all of them. `hidden` wins over both — there is nothing to
 * turn off on a card with no controls on it, and a box nobody can type into
 * for two reasons is still just a value.
 */
function rowStateFor(controls: ControlsMode, frozen: boolean): RowState {
  if (controls === "hidden") return "read";
  if (controls === "locked" || frozen) return "off";
  return "open";
}

/**
 * The registers of one recorder or one group, as boxes to fill in before a
 * recording starts.
 *
 * Every row comes from the server's own catalog, so a register added to
 * `sismatic-core` appears here without a line changing. What each box holds is
 * a *draft*: nothing is written until Save, which is what makes "clear the lot
 * and look at it before committing" possible at all.
 *
 * With the page's controls hidden it is not an editor at all — the same rows,
 * holding what the fleet is taken to hold, with nothing on them to press. The
 * rows stay in that state because this is where a take's metadata reads as
 * one list: the Details fold carries the same registers scattered among forty
 * other fields, and only the ones some device has answered for.
 */
export function RegistersPanel({
  drafts,
  frozen,
  controls,
}: RegistersPanelProps) {
  const baseId = useId();
  const ready = !drafts.catalogPending && drafts.catalogError === null;
  const state = rowStateFor(controls, frozen);

  return (
    <Clapdown title="Registers" note={noteFor(drafts, frozen, controls)}>
      {/* Amber for a seal and muted for a lock: one is the fleet refusing, the
          other is this browser being careful on purpose, and the second is the
          state the page spends its life in. Neither is said in the read state,
          where no write is being refused because none can be attempted. */}
      {state === "off" && (
        <p className={frozen ? styles.sealed : styles.locked}>
          {lockedText(frozen, controls === "locked")}
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
      {state !== "read" && drafts.rows.length > 0 && (
        <div className={styles.controls}>
          <Button
            type="button"
            onClick={() => {
              // Deliberately swallowed: a refusal is already on screen beside
              // this button, from `drafts.saveError`. `save` rejects because
              // Start has to be able to wait on it — see `LiveDeviceCard`.
              void drafts.save().catch(() => {});
            }}
            disabled={
              state === "off" || drafts.saving || drafts.unsavedCount === 0
            }
          >
            {drafts.saving ? "Saving…" : saveLabel(drafts.unsavedCount)}
          </Button>

          <Button
            type="button"
            variant="secondary"
            onClick={drafts.clearAll}
            disabled={state === "off" || drafts.saving}
          >
            Clear all
          </Button>

          {/* Not locked: throwing away drafts this browser is holding sends
              nothing anywhere, and it is the one way out of having typed into
              a card and then changed your mind. */}
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

      {/* Guarded like the buttons, and by the same reasoning: a refused save
          belongs to the panel that offered one. In the read state there is
          nothing left to retry it with, and the row it was about is already
          showing what the fleet holds. */}
      {state !== "read" && drafts.saveError !== null && (
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
              state={state}
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
  state,
  onChange,
  onClear,
}: {
  row: RegisterRow;
  id: string;
  /** Open, turned off, or a value to read — see `rowStateFor`. */
  state: RowState;
  onChange: (field: string, value: string) => void;
  onClear: (field: string) => void;
}) {
  /**
   * A row with no editor behind it: a value, and nothing that belongs to the
   * business of changing one.
   *
   * Nothing in this state can produce a draft, but arriving in it can leave
   * one behind — somebody typed a title and then hid the controls — and a page
   * whose controls have been taken away is the last place a local edit should
   * be sitting where a value belongs. So the box shows the fleet and the
   * marks that track an edit go with the editor. The draft is not thrown
   * away: `registerDrafts` is still holding it, and it is back in the box the
   * moment the page has controls again.
   */
  const reading = state === "read";
  const mark = reading ? null : markOf(row);
  /** Whether the fleet's own value is spelled out under the box. */
  const current = !reading && row.unsaved;

  // Both of those are *about* the box without being its name, so they are tied
  // to it as a description instead of sitting inside its `<label>`. A label
  // that wrapped the state word would name the field "Title unsaved" — a name
  // that is wrong, and that changes under a screen reader while its owner is
  // typing into the box it names. As a description it comes after "Title, edit
  // text", and anything that wants only the name is free to skip it.
  const described = [
    mark === null ? null : `${id}-mark`,
    current ? `${id}-current` : null,
  ].filter(value => value !== null);

  return (
    <li className={styles.row}>
      <div className={styles.labelLine}>
        <label className={styles.name} htmlFor={id}>
          {row.label}
        </label>
        {mark !== null && (
          <span id={`${id}-mark`} className={styles.mark} data-kind={mark}>
            {mark}
          </span>
        )}
      </div>

      <div className={styles.control}>
        <input
          id={id}
          className={styles.input}
          type="text"
          value={reading ? row.fleet : row.draft}
          maxLength={MAX_REGISTER_LEN}
          aria-describedby={
            described.length === 0 ? undefined : described.join(" ")
          }
          // `readonly` rather than `disabled`, which would have been one word
          // less: with the controls hidden this box is the card's only copy of
          // the value, and it has to stay selectable, copyable, and something
          // a screen reader going down the list will read out.
          readOnly={reading}
          disabled={state === "off"}
          title={row.description}
          autoComplete="off"
          onChange={event => onChange(row.field, event.target.value)}
        />
        {/* Gone rather than turned off in the read state. It is a button, and
            the point of that state is that there are none. */}
        {!reading && (
          <button
            type="button"
            className={styles.clear}
            onClick={() => onClear(row.field)}
            disabled={state === "off" || !row.clearable}
            title={`Clear ${row.label}`}
            aria-label={`Clear ${row.label}`}
          >
            ×
          </button>
        )}
      </div>

      {/* Shown only once the box has stopped agreeing with the fleet —
          otherwise every row would repeat its own value back underneath
          itself. It is what an empty box means that needs saying: cleared
          here, and still set over there until the save lands. */}
      {current && (
        <p id={`${id}-current`} className={styles.current}>
          On the devices: {row.current === "" ? "(empty)" : row.current}
        </p>
      )}
    </li>
  );
}

/**
 * The row's own state word, if it has one.
 *
 * One at a time rather than two independent flags on screen: `registerDrafts`
 * builds them exclusive — an edit is either still owed to the fleet or already
 * submitted — and only one of them can be the thing a box is described by.
 */
function markOf(row: RegisterRow): "unsaved" | "sent" | null {
  if (row.unsaved) return "unsaved";
  if (row.sent) return "sent";
  return null;
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
function noteFor(
  drafts: RegisterDrafts,
  frozen: boolean,
  controls: ControlsMode,
): ReactNode {
  // Nothing an editor would say, because in this state there is not one. A
  // failed save and an unsaved box are both about a panel with buttons; this
  // one has none, and "read-only" is the whole of what is true of it from
  // outside the fold.
  if (controls === "hidden") return "Read-only";
  if (drafts.saveError !== null) {
    return <span className={styles.noteError}>Save failed</span>;
  }
  if (drafts.unsavedCount > 0) return `${drafts.unsavedCount} unsaved`;
  // Sealed first where both are true: it is the one of the two that this
  // browser cannot do anything about, so it is the one worth knowing from
  // outside the fold.
  if (frozen) return "Sealed";
  if (controls === "locked") return "Read-only";
  return undefined;
}

/**
 * Why the boxes are off, in the words of whichever reason is actionable.
 *
 * Both reasons when both hold, and the lock named second, because that is the
 * order they have to be dealt with in: unlocking the page while a recording
 * runs still gets `metadata_frozen` from the server.
 */
function lockedText(frozen: boolean, readOnly: boolean): string {
  if (frozen && readOnly) {
    return "A recording is in progress, so these are sealed until it stops — and the dashboard is read-only besides.";
  }
  if (frozen) {
    return "A recording is in progress, so these are sealed until it stops.";
  }
  return "The dashboard is read-only. Set Controls above to Unlocked to edit these.";
}

function saveLabel(unsaved: number): string {
  if (unsaved === 0) return "Save";
  return unsaved === 1 ? "Save 1 register" : `Save ${unsaved} registers`;
}
