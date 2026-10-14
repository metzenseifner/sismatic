// Shared locators, because the obvious ones are wrong here.
//
// `getByRole("article").filter({hasText: "Hall A"})` looks right and matches
// two cards: the device, and the *group* that lists it as a member. A group
// card quoting its members is the whole point of the page, so the filter has to
// key on the card's own heading rather than on its text.
import type {Locator, Page} from "@playwright/test";

/** The one card whose own name is `id` — never a group that merely mentions it. */
export function card(page: Page, id: string): Locator {
  return page.getByRole("article").filter({
    has: page.getByRole("heading", {level: 3, name: id, exact: true}),
  });
}

/**
 * The recording badge on a card.
 *
 * Scoped to the header, where the badge lives, because the same word turns up
 * elsewhere on the card and `exact` is not enough to tell them apart: a
 * recording card also carries a "Stop recording" button and a "recording is in
 * progress" note, and a group whose state is unknown says "Unknown" again in
 * the Details fold, as the answer to "Devices in sync".
 */
export function badge(page: Page, id: string, label: string): Locator {
  return card(page, id).locator("header").getByText(label, {exact: true});
}
