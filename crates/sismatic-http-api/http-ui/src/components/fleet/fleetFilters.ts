// What the control bar's three filters mean, and the text search behind the
// free-text box.
//
// Filtering happens here, in the browser, rather than as query parameters on
// the fleet routes. Every card is already in hand — both routes are walked to
// their last page before anything is drawn — so a filter is a question about
// data this page already holds, and asking the server again would be slower
// and would leave the box lagging a keystroke behind the keyboard. The cost is
// that it does not scale past "the whole fleet fits in a browser", which is the
// assumption the overview page is built on anyway.
import {
  STATUS_LABELS,
  type DeviceView,
  type RecordingStatus,
} from "../device/deviceView";

export type Filters = {
  /** Free text, matched against every field behind a card. */
  query: string;
  /** Groups, devices, or both. */
  kind: "all" | "group" | "device";
  /** One recording state, or all of them. */
  status: "all" | RecordingStatus;
};

/** Everything shown, which is where the page starts and what Clear returns to. */
export const NO_FILTERS: Filters = {query: "", kind: "all", status: "all"};

/** Whether anything is being hidden — what makes a count worth showing. */
export function isFiltered(filters: Filters): boolean {
  return (
    filters.query.trim() !== "" ||
    filters.kind !== "all" ||
    filters.status !== "all"
  );
}

/**
 * A card together with the strings a text search looks through.
 *
 * Paired up ahead of time because the two change on different clocks: the
 * fields depend on the fleet and are rebuilt on a refetch, the query changes on
 * every keystroke. Rebuilding every card's fields per character typed is the
 * one thing in here that would actually be felt.
 */
export type Searchable = {view: DeviceView; fields: string[]};

export function searchable(views: DeviceView[]): Searchable[] {
  return views.map(view => ({view, fields: fieldsOf(view)}));
}

export function applyFilters(
  rows: Searchable[],
  filters: Filters,
): DeviceView[] {
  const byFacet = rows.filter(
    row =>
      (filters.kind === "all" || row.view.kind === filters.kind) &&
      (filters.status === "all" || row.view.status === filters.status),
  );

  const query = normalize(filters.query).trim();
  if (query === "") return byFacet.map(row => row.view);
  const terms = query.split(/\s+/);

  // Three readings of the same query, narrowest first, each tried only once
  // the one before it has come back empty:
  //
  //   1. the whole query as a phrase. "hall a" means Hall A, and any reading
  //      that splits it has thrown away the adjacency that was typed;
  //   2. every term, each somewhere, in any field. "hall morning" finds the
  //      card holding one word in its id and the other in its title;
  //   3. every term in order but not necessarily together. "hb" finds Hall B,
  //      and a dropped letter still finds something rather than nothing.
  //
  // Narrowest-first is what stops the loose readings swallowing the precise
  // ones. Reading 3 alone would let three letters match almost any card
  // carrying twenty fields; reading 2 alone lets a one-letter term — the "a"
  // in "hall a" — match every card on the page. A single-word query makes
  // readings 1 and 2 the same question, asked twice and answered the same way.
  const phrase = byFacet.filter(row =>
    row.fields.some(field => field.includes(query)),
  );
  if (phrase.length > 0) return phrase.map(row => row.view);

  const scattered = byFacet.filter(row =>
    matchesEvery(row.fields, terms, within),
  );
  if (scattered.length > 0) return scattered.map(row => row.view);

  return byFacet
    .filter(row => matchesEvery(row.fields, terms, inOrder))
    .map(row => row.view);
}

/**
 * Every term has to match, each one somewhere.
 *
 * Per term rather than against the whole card at once, so two words find a card
 * that has one of them in its id and the other in a register — which is how
 * people search when they half-remember two things about a recorder.
 */
function matchesEvery(
  fields: string[],
  terms: string[],
  match: (term: string, field: string) => boolean,
): boolean {
  return terms.every(term => fields.some(field => match(term, field)));
}

function within(term: string, field: string): boolean {
  return field.includes(term);
}

/** `term`'s characters appear in `field` in order, not necessarily together. */
function inOrder(term: string, field: string): boolean {
  let at = 0;
  for (let i = 0; i < field.length && at < term.length; i++) {
    if (field[i] === term[at]) at += 1;
  }
  return at === term.length;
}

/**
 * Everything about one card a filter box can reasonably be expected to find.
 *
 * Kept as a list of fields rather than one joined string so that no term can
 * match across a boundary: "hall firmware" should find a card whose id is one
 * and whose detail label is the other, not a card where those words happen to
 * end up adjacent once everything is concatenated.
 */
function fieldsOf(view: DeviceView): string[] {
  const fields = [view.id, view.kind, STATUS_LABELS[view.status]];
  if (view.title !== null) fields.push(view.title);
  for (const row of view.details) fields.push(row.label, row.value);
  for (const member of view.members ?? []) {
    fields.push(member.id, STATUS_LABELS[member.status]);
    if (member.title !== null) fields.push(member.title);
  }
  return fields.map(normalize);
}

/**
 * Case- and accent-insensitive, both sides.
 *
 * The accent folding matters more than it looks: a presenter called Müller
 * sits in a register typed by whoever was holding the keyboard, and a filter
 * box that cannot find a name because of an umlaut is a filter box people stop
 * using.
 */
function normalize(text: string): string {
  return text
    .normalize("NFD")
    .replace(/\p{Diacritic}/gu, "")
    .toLowerCase();
}
