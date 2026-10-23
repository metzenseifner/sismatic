// The state behind the Registers panel: one draft value per metadata register,
// and the rules for deciding which of them the fleet is still owed.
//
// Three facts meet in every row, which is why this is a hook rather than six
// `useState`s in a component:
//
//   1. what the catalog says exists    — `GET /v1/writes`
//   2. what the devices report holding  — the fleet reads the card is drawn from
//   3. what the operator has typed      — local, and the only one that is lost
//                                         on a reload
//
// A register is written when (3) differs from (2). Everything else in here is
// about what to do when (2) is not available — see `assumedText`, which is
// where the one genuinely arguable decision in this file lives.
import {useCallback, useMemo, useState} from "react";
import {
  useMetadataWrites,
  useWritesCatalog,
  type ApiCallError,
  type RegisterWrite,
  type WriteTarget,
} from "../../api/queries";
import {labelOf, type DeviceView, type FieldState} from "./deviceView";

/**
 * Longest value a settable register accepts — `MAX_VALUE_LEN` in
 * `sismatic-core`'s `protocol::instructions::register`.
 *
 * Enforced on the input box because the layers below only warn: the server logs
 * that the value "will be curtailed to 127" and sends it anyway. An operator
 * who can see the limit coming does not have to work out later why half a
 * sentence reached the recorder.
 *
 * Counted in characters, as `shorten` counts them. The two only diverge above
 * the BMP, where `maxLength` counts a UTF-16 pair as two and Rust counts it as
 * one — which errs towards refusing a character the device would have taken,
 * never towards letting one through that it would have cut.
 */
export const MAX_REGISTER_LEN = 127;

/** One register, as the panel draws it. */
export type RegisterRow = {
  /** Canonical name, the spelling the catalog, the reads and the URL all use. */
  field: string;
  /** `PRESENTER` → `Presenter`. */
  label: string;
  /** The catalog's one-liner for this register. */
  description: string;
  /** What is in the box. */
  draft: string;
  /** What the devices report, rendered: a value, "Differs across devices", "Not read yet". */
  current: string;
  /** Edited, and the fleet has not been told yet. */
  unsaved: boolean;
  /**
   * Submitted and accepted; no device has reported it back yet.
   *
   * Transient for every register, now that each one the catalog offers a write
   * for is also readable: the next sync either reports the value back and the
   * row settles, or it does not and the row keeps saying "Sent", which is then
   * a fact about the device rather than about the protocol.
   */
  sent: boolean;
  /** Whether emptying this box would change anything. */
  clearable: boolean;
};

/** Everything the Registers panel needs, and nothing a component should own. */
export type RegisterDrafts = {
  rows: RegisterRow[];
  /** How many registers a save would write. */
  unsavedCount: number;
  /** True once any box has been touched, saved or not — what "Discard" undoes. */
  hasChanges: boolean;
  catalogPending: boolean;
  catalogError: ApiCallError | null;
  saving: boolean;
  saveError: ApiCallError | null;
  set: (field: string, value: string) => void;
  clear: (field: string) => void;
  clearAll: () => void;
  discard: () => void;
  /**
   * Write every unsaved row. Resolves once all of them are accepted, and
   * rejects with the first refusal — which is what lets a start wait for the
   * metadata it is supposed to be recorded under.
   */
  save: () => Promise<void>;
};

export function useRegisterDrafts(
  target: WriteTarget,
  view: DeviceView,
): RegisterDrafts {
  const catalog = useWritesCatalog();
  const writes = useMetadataWrites(target);

  // Sparse on purpose: a field is in here only once someone has typed in its
  // box. Every other box falls back to what the devices report, so an
  // untouched card keeps following the five-second refetch instead of freezing
  // whatever happened to be on screen when it mounted.
  const [changes, setChanges] = useState<Record<string, string>>({});
  // What the server has answered `202` for, per field. `changes` minus this is
  // the set of writes the fleet is still owed.
  const [submitted, setSubmitted] = useState<Record<string, string>>({});

  const registers = catalog.data?.metadata;

  const rows = useMemo<RegisterRow[]>(
    () =>
      (registers ?? []).map(register => {
        const field = register.name;
        const state = view.values.get(field);
        const assumed = assumedText(state);
        const changed = Object.hasOwn(changes, field);
        const draft = changed ? changes[field] : (assumed ?? "");
        // A box is a no-op when it says what the fleet is taken to hold. Where
        // even that is unavailable — a group reading "Differs across devices" —
        // the box is written as typed, including an empty one, which is how
        // such a register gets cleared at all.
        const matchesFleet = assumed !== null && assumed === draft;

        return {
          field,
          label: labelOf(field),
          description: register.description,
          draft,
          current: state?.display ?? "Not read yet",
          unsaved: changed && !matchesFleet && draft !== submitted[field],
          sent: changed && !matchesFleet && draft === submitted[field],
          // An already-empty box is still worth a Clear in the one case where
          // the fleet is known not to be empty but no single value can be
          // shown: a group reading "Differs across devices", whose box starts
          // empty. Everywhere else an empty box means the fleet is taken to be
          // empty too, so there is nothing left for a Clear to ask for.
          clearable: draft !== "" || (!matchesFleet && !changed),
        };
      }),
    [registers, changes, submitted, view.values],
  );

  // Stable across keystrokes, unlike `rows`, so `clearAll` is not rebuilt on
  // every character typed into a box.
  const fields = useMemo(
    () => (registers ?? []).map(register => register.name),
    [registers],
  );

  const set = useCallback((field: string, value: string) => {
    setChanges(current => ({...current, [field]: value}));
  }, []);

  /** Empty one box. The save that follows clears that register on the devices. */
  const clear = useCallback((field: string) => set(field, ""), [set]);

  /**
   * Empty every box at once — "next recording, nothing carried over from the
   * last one". Registers the fleet is already taken to hold empty come out of
   * this with nothing to save, so the save that follows writes only what is
   * really still there.
   */
  const clearAll = useCallback(() => {
    setChanges(Object.fromEntries(fields.map(field => [field, ""])));
  }, [fields]);

  /** Forget every change and follow the fleet again. */
  const discard = useCallback(() => {
    setChanges({});
    setSubmitted({});
    writes.reset();
  }, [writes]);

  const save = useCallback(async () => {
    const pending: RegisterWrite[] = rows
      .filter(row => row.unsaved)
      .map(row => ({field: row.field, value: row.draft}));
    if (pending.length === 0) return;

    // Marked submitted only after the whole run is accepted. A refusal
    // part-way therefore leaves every row unsaved, including the ones that did
    // land, and the retry rewrites them — which costs one repeated write of a
    // register that already holds that value, and keeps the panel from ever
    // claiming an edit reached a device when it did not.
    await writes.mutateAsync(pending);
    setSubmitted(current => ({
      ...current,
      ...Object.fromEntries(pending.map(write => [write.field, write.value])),
    }));
  }, [rows, writes]);

  return {
    rows,
    unsavedCount: rows.filter(row => row.unsaved).length,
    hasChanges: Object.keys(changes).length > 0,
    catalogPending: catalog.isPending,
    catalogError: catalog.error,
    saving: writes.isPending,
    saveError: writes.error,
    set,
    clear,
    clearAll,
    discard,
    save,
  };
}

/**
 * The text every device behind the card is taken to hold.
 *
 * A register nothing has answered for is taken to be **empty**, which is an
 * assumption and not a fact. It is the one that makes the panel behave the way
 * it looks: a register with no value shows an empty box, so clearing that box
 * has to count as changing nothing, or Clear would mark a row unsaved for a
 * value the operator never saw and never touched.
 *
 * What it costs is narrow and worth stating. Until a read lands — a device that
 * has not been polled yet, or one that did not answer — an empty box is
 * indistinguishable from a value we cannot see, so Clear cannot queue the write
 * that would empty it. Typing something and saving still works; only the clear
 * waits on a read. This used to be permanent for `LANGUAGE` and `PUBLISHER`,
 * which were writable with no query behind them; both now read back like every
 * other register, so no row is stuck in that state by construction.
 *
 * `null` is kept for the one case where emptiness is positively ruled out: a
 * group whose members hold different values, which is a thing we *know* rather
 * than a thing we have not heard. Clearing that is a real operation and stays
 * available. A non-text read keeps `null` too — not a value an input box can
 * round-trip, so nothing about it should be assumed either way.
 */
function assumedText(state: FieldState | undefined): string | null {
  if (state === undefined) return ""; // this card has heard nothing at all
  if (state.mixed) return null; // known to differ, so known not to be empty
  if (state.read === undefined) return ""; // asked for, and no member answered
  return state.read.type === "text" ? state.read.value : null;
}
