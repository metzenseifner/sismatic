import {useId, useMemo, useState} from "react";
import {Clapdown} from "../ui/Clapdown";
import {Button} from "../ui/Button";
import {STATUS_LABELS} from "../device/deviceView";
import {isFiltered, type Filters} from "./fleetFilters";
import {
  pickableFields,
  placementOf,
  withAllPlaced,
  withPlacement,
  DEFAULT_DISPLAY,
  type CardDisplay,
  type PickableField,
  type Placement,
} from "./fleetDisplay";
import {
  COLUMN_CHOICES,
  CONTROLS_CHOICES,
  KIND_CHOICES,
  PLACEMENT_CHOICES,
  STATUS_CHOICES,
  type ControlsMode,
  type FleetSettings,
} from "./fleetSettings";
import {
  MAX_POLL_MS,
  MIN_POLL_MS,
  useReadsCatalog,
} from "../../api/queries";
import {THEME_CHOICES, type ThemeMode} from "../../styles/theme";
import styles from "./ControlBar.module.css";

/** Poll intervals offered by name, in milliseconds. */
const POLL_PRESETS = [1_000, 2_000, 5_000, 10_000, 30_000, 60_000, 300_000];

// Keyed by the union, so a new kind or placement is a type error here until
// somebody has decided what to call it on screen.
const KIND_LABELS: Record<Filters["kind"], string> = {
  all: "Groups and devices",
  group: "Groups only",
  device: "Devices only",
};

const PLACEMENT_LABELS: Record<Placement, string> = {
  pinned: "Pinned",
  details: "In details",
  hidden: "Hidden",
};

const CONTROLS_LABELS: Record<ControlsMode, string> = {
  hidden: "Hidden",
  locked: "Read-only",
  unlocked: "Unlocked",
};

const THEME_LABELS: Record<ThemeMode, string> = {
  system: "System",
  light: "Light",
  dark: "Dark",
};

type ControlBarProps = {
  /**
   * Everything the bar controls, as one value.
   *
   * One object and one callback rather than a pair per control: these are read
   * and written together, stored together, and restored together, so splitting
   * them into eight props would have the page reassemble on every change what
   * this already has in hand.
   */
  settings: FleetSettings;
  onChange: (settings: FleetSettings) => void;
  /** How many cards the filters let through, and how many there are in all. */
  shown: number;
  total: number;
};

/**
 * The page's own controls: what to show, how to show it, and whether this
 * browser may write to the fleet at all.
 *
 * Everything here is state the page holds rather than anything it sends: the
 * filters never reach the server — see `fleetFilters` — and no control in this
 * bar submits a thing. One of them does decide what the *cards* are allowed to
 * submit — and whether they offer anything to submit with at all — which is
 * why the bar says so about itself in the colour of its own border rather than
 * leaving the answer to forty greyed-out buttons.
 */
export function ControlBar({
  settings,
  onChange,
  shown,
  total,
}: ControlBarProps) {
  const ids = useId();
  const {filters, display, pollMs, columns, theme, controls} = settings;
  /** The one state that can reach a device, and the only one worth shouting. */
  const armed = controls === "unlocked";

  /** One setting replaced, the rest carried over. */
  const set = <K extends keyof FleetSettings>(
    key: K,
    value: FleetSettings[K],
  ) => onChange({...settings, [key]: value});
  // Asked for here rather than threaded down from the page: it is one static
  // catalog behind one query key, so every card and this bar share the single
  // request whoever mounts first makes.
  const catalog = useReadsCatalog();
  const fields = useMemo(
    () => pickableFields(catalog.data?.fields),
    [catalog.data?.fields],
  );

  return (
    // The armed state is an attribute rather than a second class name, so the
    // stylesheet can key the whole bar off it — and so a test can ask the page
    // what it thinks it is, instead of matching on a hashed CSS module name.
    <div className={styles.bar} data-armed={armed ? "true" : undefined}>
      <div className={styles.row}>
        <input
          className={`${styles.field} ${styles.search}`}
          type="search"
          value={filters.query}
          placeholder="Filter by name, title, presenter, any field…"
          aria-label="Filter by text"
          autoComplete="off"
          onChange={event =>
            set("filters", {...filters, query: event.target.value})
          }
        />

        <div className={styles.group}>
          <label className={styles.label} htmlFor={`${ids}-kind`}>
            Show
          </label>
          <select
            id={`${ids}-kind`}
            className={styles.field}
            value={filters.kind}
            onChange={event =>
              set("filters", {
                ...filters,
                kind: event.target.value as Filters["kind"],
              })
            }
          >
            {KIND_CHOICES.map(kind => (
              <option key={kind} value={kind}>
                {KIND_LABELS[kind]}
              </option>
            ))}
          </select>
        </div>

        <div className={styles.group}>
          <label className={styles.label} htmlFor={`${ids}-status`}>
            State
          </label>
          <select
            id={`${ids}-status`}
            className={styles.field}
            value={filters.status}
            onChange={event =>
              set("filters", {
                ...filters,
                status: event.target.value as Filters["status"],
              })
            }
          >
            {STATUS_CHOICES.map(status => (
              <option key={status} value={status}>
                {status === "all" ? "Any state" : STATUS_LABELS[status]}
              </option>
            ))}
          </select>
        </div>

        {/* Only while something is being hidden. An unfiltered page counting
            itself out loud is noise; a filtered one that does not is how a
            card goes missing without anybody noticing. */}
        {isFiltered(filters) && total > 0 && (
          <p className={styles.count} role="status">
            {shown} of {total}
          </p>
        )}

        <div className={styles.settings}>
          {/* First in the cluster, because it is the one setting here that has
              to be read *before* anything on a card is clicked rather than
              after. A select and not a switch: there are three states, and two
              interactions to change it is the right price for the only control
              on this page that can arm a stop button.

              It is also the only place the hidden state is visible at all.
              That is the state's whole point — nothing on a card says a word
              about controls that are not there — and it is why this stays a
              named setting in the bar rather than, say, a URL parameter a
              kiosk is launched with and nobody can find again. */}
          <div className={styles.group}>
            <label className={styles.label} htmlFor={`${ids}-controls`}>
              Controls
            </label>
            <select
              id={`${ids}-controls`}
              className={styles.field}
              data-armed={armed ? "true" : undefined}
              value={controls}
              onChange={event =>
                set("controls", event.target.value as ControlsMode)
              }
            >
              {CONTROLS_CHOICES.map(mode => (
                <option key={mode} value={mode}>
                  {CONTROLS_LABELS[mode]}
                </option>
              ))}
            </select>
          </div>

          <div className={styles.group}>
            <label className={styles.label} htmlFor={`${ids}-details`}>
              Details
            </label>
            <select
              id={`${ids}-details`}
              className={styles.field}
              value={display.detailsOpen ? "open" : "shut"}
              onChange={event =>
                set("display", {
                  ...display,
                  detailsOpen: event.target.value === "open",
                })
              }
            >
              <option value="shut">Collapsed</option>
              <option value="open">Expanded</option>
            </select>
          </div>

          <div className={styles.group}>
            <label className={styles.label} htmlFor={`${ids}-columns`}>
              Per row
            </label>
            <select
              id={`${ids}-columns`}
              className={styles.field}
              value={String(columns)}
              onChange={event =>
                set(
                  "columns",
                  event.target.value === "auto"
                    ? "auto"
                    : Number(event.target.value),
                )
              }
            >
              {COLUMN_CHOICES.map(choice => (
                <option key={String(choice)} value={String(choice)}>
                  {choice === "auto" ? "Auto" : `Up to ${choice}`}
                </option>
              ))}
            </select>
          </div>

          <PollPicker
            pollMs={pollMs}
            onChange={ms => set("pollMs", ms)}
            selectId={`${ids}-poll`}
          />

          {/* The only control here whose value no component ever sees: it
              leaves as an attribute on the root element and the stylesheet
              does the rest. See `styles/theme.ts`. */}
          <div className={styles.group}>
            <label className={styles.label} htmlFor={`${ids}-theme`}>
              Theme
            </label>
            <select
              id={`${ids}-theme`}
              className={styles.field}
              value={theme}
              onChange={event =>
                set("theme", event.target.value as ThemeMode)
              }
            >
              {THEME_CHOICES.map(mode => (
                <option key={mode} value={mode}>
                  {THEME_LABELS[mode]}
                </option>
              ))}
            </select>
          </div>
        </div>
      </div>

      {/* Said out loud, and only in the state worth saying out loud. `status`
          rather than `alert`: it is announced when it appears, without
          interrupting whatever a screen reader was in the middle of. Nothing
          is drawn for the other two: read-only is the state the page spends
          its life in, and hidden is one somebody chose in order to be told
          less. */}
      {armed && (
        <p className={styles.armed} role="status">
          Controls are unlocked — Start, Stop, Pause and the registers on every
          card will reach the fleet.
        </p>
      )}

      <FieldPicker
        display={display}
        onChange={next => set("display", next)}
        fields={fields}
        pending={catalog.isPending}
        error={catalog.error === null ? null : catalog.error.message}
      />
    </div>
  );
}

/**
 * Where each field goes on every card: pinned above the fold, inside the
 * Details fold, or nowhere.
 *
 * Folded away, and behind the same `Clapdown` the cards use: forty rows across
 * the top of the page would cost more room than they save. The counts on the
 * summary line are what keep a hidden field from looking like a field the
 * server stopped reporting.
 *
 * Listed from the server's field catalog rather than from what the cards are
 * reporting, which is what makes it usable before a device has answered —
 * the moment a page is most likely to be set up. The cost is that the list
 * includes names this fleet may never report; the gain is that pinning one of
 * them works, and the pin is waiting when the value arrives.
 */
function FieldPicker({
  display,
  onChange,
  fields,
  pending,
  error,
}: {
  display: CardDisplay;
  onChange: (display: CardDisplay) => void;
  fields: PickableField[];
  pending: boolean;
  error: string | null;
}) {
  const ids = useId();

  let pinned = 0;
  let hidden = 0;
  for (const {field} of fields) {
    const place = placementOf(display, field);
    if (place === "pinned") pinned += 1;
    if (place === "hidden") hidden += 1;
  }

  const atDefault =
    display.placement.size === DEFAULT_DISPLAY.placement.size &&
    [...DEFAULT_DISPLAY.placement].every(
      ([field, place]) => display.placement.get(field) === place,
    );

  const placeAll = (place: "details" | "hidden") =>
    onChange({
      ...display,
      placement: withAllPlaced(display.placement, fields, place),
    });

  return (
    <Clapdown title="Fields" note={fieldNote(pinned, hidden)}>
      {pending && <p>Loading the field list…</p>}

      {error !== null && (
        <p className={styles.error} role="alert">
          Could not load the field list. {error}
        </p>
      )}

      {fields.length > 0 && (
        <div className={styles.row}>
          <Button
            type="button"
            variant="secondary"
            onClick={() => placeAll("details")}
            disabled={hidden === 0}
          >
            Show all
          </Button>
          <Button
            type="button"
            variant="secondary"
            onClick={() => placeAll("hidden")}
            disabled={hidden === fields.length - pinned}
          >
            Hide all
          </Button>
          <Button
            type="button"
            variant="secondary"
            onClick={() =>
              onChange({...display, placement: DEFAULT_DISPLAY.placement})
            }
            disabled={atDefault}
          >
            Reset
          </Button>
        </div>
      )}

      <ul className={styles.fields}>
        {fields.map(({field, label, description}) => (
          <li key={field} className={styles.fieldRow}>
            <label
              className={styles.fieldLabel}
              htmlFor={`${ids}-${field}`}
              title={description}
            >
              {label}
            </label>
            <select
              id={`${ids}-${field}`}
              className={styles.field}
              value={placementOf(display, field)}
              onChange={event =>
                onChange({
                  ...display,
                  placement: withPlacement(
                    display.placement,
                    field,
                    event.target.value as Placement,
                  ),
                })
              }
            >
              {PLACEMENT_CHOICES.map(place => (
                <option key={place} value={place}>
                  {PLACEMENT_LABELS[place]}
                </option>
              ))}
            </select>
          </li>
        ))}
      </ul>
    </Clapdown>
  );
}

/** What the fold says about itself while shut. */
function fieldNote(pinned: number, hidden: number): string | undefined {
  const parts: string[] = [];
  if (pinned > 0) parts.push(`${pinned} pinned`);
  if (hidden > 0) parts.push(`${hidden} hidden`);
  return parts.length === 0 ? undefined : parts.join(" · ");
}

/**
 * The poll interval: presets, plus a box for anything else.
 *
 * A select with a "Custom" option rather than one number input with a
 * `<datalist>`. The single-control version is tidier and does not work: Safari
 * and iOS do not open a datalist on a number input, which would leave the
 * presets invisible on the devices this page is most likely to be read on.
 */
function PollPicker({
  pollMs,
  onChange,
  selectId,
}: {
  pollMs: number;
  onChange: (ms: number) => void;
  selectId: string;
}) {
  // Only needed to hold the box open on a value that happens to be a preset,
  // after someone has deliberately chosen Custom. Everywhere else the chosen
  // interval says for itself which of the two it is.
  const [custom, setCustom] = useState(false);
  const [seconds, setSeconds] = useState(() => String(pollMs / 1000));

  const onPreset = POLL_PRESETS.includes(pollMs) && !custom;

  const pick = (value: string) => {
    if (value === "custom") {
      setSeconds(String(pollMs / 1000));
      setCustom(true);
      return;
    }
    setCustom(false);
    onChange(Number(value));
  };

  // Committed only once the box holds something the page can actually poll at.
  // Anything else is left alone to be finished typing — clamping per keystroke
  // makes "15" impossible to type, since "1" would be corrected the moment it
  // appears — and `restore` puts the real value back if it never is.
  const type = (value: string) => {
    setSeconds(value);
    const ms = Math.round(Number(value) * 1000);
    if (
      value.trim() !== "" &&
      Number.isFinite(ms) &&
      ms >= MIN_POLL_MS &&
      ms <= MAX_POLL_MS
    ) {
      onChange(ms);
    }
  };

  const restore = () => setSeconds(String(pollMs / 1000));

  return (
    <div className={styles.group}>
      <label className={styles.label} htmlFor={selectId}>
        Poll every
      </label>
      <select
        id={selectId}
        className={styles.field}
        value={onPreset ? String(pollMs) : "custom"}
        onChange={event => pick(event.target.value)}
      >
        {POLL_PRESETS.map(ms => (
          <option key={ms} value={ms}>
            {pollLabel(ms)}
          </option>
        ))}
        <option value="custom">Custom…</option>
      </select>

      {!onPreset && (
        <input
          className={`${styles.field} ${styles.seconds}`}
          type="number"
          inputMode="decimal"
          min={MIN_POLL_MS / 1000}
          max={MAX_POLL_MS / 1000}
          step={1}
          value={seconds}
          aria-label="Poll interval in seconds"
          onChange={event => type(event.target.value)}
          onBlur={restore}
        />
      )}
      {!onPreset && <span className={styles.label}>s</span>}
    </div>
  );
}

function pollLabel(ms: number): string {
  if (ms < 60_000) return `${ms / 1000} s`;
  const minutes = ms / 60_000;
  return minutes === 1 ? "1 min" : `${minutes} min`;
}
