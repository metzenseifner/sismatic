// Everything the control bar holds, and how it survives a reload.
//
// One object and one storage key, because the settings are read and written as
// a unit: the page starts from whatever the last visit left, so loading has to
// be finished before the first paint and cannot be spread over six keys read at
// six different moments.
//
// # Why every field is validated separately
//
// `localStorage` is not a database this code owns. What comes back may have
// been written by an older build, edited by hand in a devtools console, or
// truncated by a browser reclaiming space — and the one thing it must never do
// is throw during the first render, because that is a white page instead of a
// fleet. So each field is checked against what it is allowed to be and falls
// back on its own: a poll interval someone set to a thousandth of a second does
// not also cost them their pinned fields.
//
// The closed sets live here rather than in the bar for the same reason. The
// list the menu offers and the list the loader accepts have to be the same
// list, and the way to guarantee that is for there to be one.
import {
  DEFAULT_POLL_MS,
  MAX_POLL_MS,
  MIN_POLL_MS,
} from "../../api/queries";
import type {Columns} from "../layout/Grid";
import {DEFAULT_THEME, THEME_CHOICES, type ThemeMode} from "../../styles/theme";
import {NO_FILTERS, type Filters} from "./fleetFilters";
import {
  DEFAULT_DISPLAY,
  type CardDisplay,
  type Placement,
} from "./fleetDisplay";
import {useCallback, useState} from "react";

export const KIND_CHOICES: readonly Filters["kind"][] = [
  "all",
  "group",
  "device",
];

export const STATUS_CHOICES: readonly Filters["status"][] = [
  "all",
  "recording",
  "paused",
  "stopped",
  "mixed",
  "unknown",
];

export const PLACEMENT_CHOICES: readonly Placement[] = [
  "pinned",
  "details",
  "hidden",
];

/** Column caps offered. Each is a maximum, never a promise — see `Grid`. */
export const COLUMN_CHOICES: readonly Columns[] = ["auto", 1, 2, 3, 4, 5, 6];

/** Everything the bar controls, in one restorable piece. */
export type FleetSettings = {
  filters: Filters;
  display: CardDisplay;
  pollMs: number;
  columns: Columns;
  /**
   * Light, dark, or whatever the machine says. Stored with the rest because it
   * is a preference like any other — and the only thing kept here that no
   * component ever reads: it reaches the page as an attribute, and the
   * stylesheet takes it from there. See `styles/theme.ts`.
   */
  theme: ThemeMode;
};

export const DEFAULT_SETTINGS: FleetSettings = {
  filters: NO_FILTERS,
  display: DEFAULT_DISPLAY,
  pollMs: DEFAULT_POLL_MS,
  columns: "auto",
  theme: DEFAULT_THEME,
};

/**
 * Where the settings are kept, and under which reading of them.
 *
 * The version is in the key rather than in the value, so that a future change
 * of shape starts from the defaults instead of having to understand every
 * shape that came before — and leaves the old blob sitting there, readable, if
 * it turns out something was worth migrating after all.
 */
const STORAGE_KEY = "sismatic.fleet.settings.v1";

/**
 * The settings, and a setter that also writes them down.
 *
 * Written in the setter rather than in an effect, so that a change and its
 * record happen in the same turn: an effect would write a render later, which
 * is a window — short, but real — in which a reload loses the last thing that
 * was done.
 */
export function useFleetSettings(): [
  FleetSettings,
  (next: FleetSettings) => void,
] {
  // Lazy, so storage is read once on the first render rather than on every
  // one, and read *before* the first paint — no flash of defaults.
  const [settings, setSettings] = useState<FleetSettings>(loadSettings);

  const update = useCallback((next: FleetSettings) => {
    setSettings(next);
    saveSettings(next);
  }, []);

  return [settings, update];
}

export function loadSettings(): FleetSettings {
  const raw = readStorage(STORAGE_KEY);
  if (raw === null) return DEFAULT_SETTINGS;

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return DEFAULT_SETTINGS;
  }
  if (typeof parsed !== "object" || parsed === null) return DEFAULT_SETTINGS;
  const stored = parsed as Record<string, unknown>;

  return {
    filters: {
      query: typeof stored.query === "string" ? stored.query : "",
      kind: oneOf(KIND_CHOICES, stored.kind, "all"),
      status: oneOf(STATUS_CHOICES, stored.status, "all"),
    },
    display: {
      detailsOpen: stored.detailsOpen === true,
      placement: placementFrom(stored.placement),
    },
    pollMs: pollFrom(stored.pollMs),
    columns: COLUMN_CHOICES.includes(stored.columns as Columns)
      ? (stored.columns as Columns)
      : DEFAULT_SETTINGS.columns,
    theme: oneOf(THEME_CHOICES, stored.theme, DEFAULT_THEME),
  };
}

export function saveSettings(settings: FleetSettings): void {
  // Flat, and only the parts that are not derivable: a `Map` is not JSON, and
  // the default placement is absence, so what goes out is the handful of
  // fields someone actually moved.
  writeStorage(
    STORAGE_KEY,
    JSON.stringify({
      query: settings.filters.query,
      kind: settings.filters.kind,
      status: settings.filters.status,
      detailsOpen: settings.display.detailsOpen,
      placement: Object.fromEntries(settings.display.placement),
      pollMs: settings.pollMs,
      columns: settings.columns,
      theme: settings.theme,
    }),
  );
}

function oneOf<T extends string>(
  allowed: readonly T[],
  value: unknown,
  fallback: T,
): T {
  return typeof value === "string" && (allowed as readonly string[]).includes(value)
    ? (value as T)
    : fallback;
}

function pollFrom(value: unknown): number {
  return typeof value === "number" &&
    Number.isFinite(value) &&
    value >= MIN_POLL_MS &&
    value <= MAX_POLL_MS
    ? value
    : DEFAULT_POLL_MS;
}

/**
 * The stored field placements, or the default set if there are none.
 *
 * An empty object is *not* the same as a missing one, and the difference is
 * load-bearing: someone who unpinned the title and hid nothing has an empty
 * placement, and reading that as "nothing stored" would pin the title back on
 * every reload.
 *
 * Names this build has never heard of are kept rather than dropped. The
 * catalog belongs to the server and may gain or lose names between two visits;
 * a placement for a field that is not there today costs nothing and is right
 * again the moment it returns.
 */
function placementFrom(value: unknown): ReadonlyMap<string, Placement> {
  if (typeof value !== "object" || value === null) {
    return DEFAULT_DISPLAY.placement;
  }

  const placement = new Map<string, Placement>();
  for (const [field, place] of Object.entries(
    value as Record<string, unknown>,
  )) {
    // "In details" is the default and is stored as absence, so an entry
    // saying so is dropped on the way back in rather than kept as a no-op.
    if (place === "pinned" || place === "hidden") placement.set(field, place);
  }
  return placement;
}

// `localStorage` throws rather than returning null in more cases than it is
// given credit for: Safari in private browsing, storage disabled by policy, a
// full quota. A page that cannot remember a preference still has to show a
// fleet, so both sides swallow and carry on.

function readStorage(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStorage(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Nothing to do and nothing worth saying: the settings still apply for as
    // long as the page is open, which is the whole of what this session needs.
  }
}
