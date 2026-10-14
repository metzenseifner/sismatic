// How the cards are shown, as opposed to which cards are shown.
//
// Separate from `fleetFilters` because the two do different things to a card: a
// filter decides whether it is on the page at all, and this decides how much of
// it is unfolded once it is. The distinction shows up in one place in
// particular — a hidden field is still searchable, because hiding Firmware from
// forty cards is a decision about screen space, not a statement that you no
// longer care what firmware they are running.
import {RUNNING_STATE, TITLE, labelOf} from "../device/deviceView";
import type {InstructionSummary} from "../../api/types";

/**
 * Where one field goes on a card.
 *
 * Three places rather than a pair of booleans, because a field is in exactly
 * one of them and the pair would admit a fourth state — pinned but hidden —
 * that means nothing and would have to be guarded against everywhere.
 */
export type Placement = "pinned" | "details" | "hidden";

/** The place a field takes when nothing has been said about it. */
export const DEFAULT_PLACEMENT: Placement = "details";

export type CardDisplay = {
  /**
   * The page-wide position of every card's Details fold. Changing it moves all
   * of them; a card folded or unfolded by hand afterwards keeps its own answer
   * until this moves again. See `Clapdown`.
   */
  detailsOpen: boolean;
  /**
   * Where each field goes, for the fields that are not in the default place.
   *
   * Sparse, so that "in the Details fold" is what a field does by arriving:
   * a device that starts reporting a name nobody has seen before shows it,
   * instead of being quietly dropped for having turned up after the setting
   * was made.
   */
  placement: ReadonlyMap<string, Placement>;
};

/**
 * The title is pinned, and nothing else is.
 *
 * Pinned rather than special-cased, which is the point: the card used to show
 * the recording title above the fold because it was written to, and there was
 * no way to ask it for anything else. Now that is one entry in a map and can be
 * removed like any other, while "is it recording, and what is it called" stays
 * the answer the page gives by default.
 */
export const DEFAULT_DISPLAY: CardDisplay = {
  detailsOpen: false,
  placement: new Map([[TITLE, "pinned"]]),
};

export function placementOf(
  display: CardDisplay,
  field: string,
): Placement {
  return display.placement.get(field) ?? DEFAULT_PLACEMENT;
}

/** One field the picker lists. */
export type PickableField = {field: string; label: string; description: string};

/**
 * The fields a card can be asked to show, from the server's own catalog.
 *
 * `RUNNING_STATE` is left out: it is the recording badge, which is the one
 * thing on a card that is never in question and never folded away. Everything
 * else is offered, the title included.
 */
export function pickableFields(
  catalog: InstructionSummary[] | undefined,
): PickableField[] {
  return (catalog ?? [])
    .filter(entry => entry.name !== RUNNING_STATE)
    .map(entry => ({
      field: entry.name,
      label: labelOf(entry.name),
      description: entry.description,
    }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

/** The map with one field moved — replaced, never mutated. */
export function withPlacement(
  placement: ReadonlyMap<string, Placement>,
  field: string,
  place: Placement,
): ReadonlyMap<string, Placement> {
  const next = new Map(placement);
  // The default is absence, so that a field put back where it started leaves no
  // trace — which is what keeps a sparse map from filling up with entries that
  // say nothing.
  if (place === DEFAULT_PLACEMENT) next.delete(field);
  else next.set(field, place);
  return next;
}

/**
 * Every field moved to one place at once, pins left alone.
 *
 * Pins survive a Hide all because a pin is the most deliberate thing anyone
 * says in this panel: it is worth more than a bulk action aimed at the fold
 * below it.
 */
export function withAllPlaced(
  placement: ReadonlyMap<string, Placement>,
  fields: PickableField[],
  place: Exclude<Placement, "pinned">,
): ReadonlyMap<string, Placement> {
  const next = new Map<string, Placement>();
  for (const {field} of fields) {
    const current = placement.get(field);
    if (current === "pinned") next.set(field, "pinned");
    else if (place !== DEFAULT_PLACEMENT) next.set(field, place);
  }
  return next;
}
