// What the browser does with the CSS, which is the part no unit test reaches.
//
// Every assertion here is a number that was wrong at some point while this page
// was being built: a column count that silently came out one short, a control
// bar that spanned the window instead of the grid, a cards-per-row setting that
// a phone obeyed.
import {expect, test} from "@playwright/test";
import {HALL_A, HALLS, stubApi} from "./stubApi";
import type {DeviceReads} from "../src/api/types";

/** Enough cards that a six-column row is actually full. */
const FLEET: DeviceReads[] = Array.from({length: 14}, (_, i) => ({
  ...HALL_A,
  device: `Room ${String(i + 1).padStart(2, "0")}`,
}));

/**
 * How many tracks the grid resolved to — the only honest count.
 *
 * Found through the cards rather than by matching on a style attribute: the
 * grid is whatever they are laid out in, and a test that went looking for a
 * string in `style=` would start passing vacuously the day that changed.
 */
const columnCount = (page: import("@playwright/test").Page) =>
  page.evaluate(() => {
    const grid = document.querySelector("article")?.parentElement;
    if (!grid) throw new Error("no cards on the page");
    return getComputedStyle(grid).gridTemplateColumns.split(/\s+/).length;
  });

test.describe("desktop", () => {
  test.use({viewport: {width: 1440, height: 1000}});

  test("a requested column count is a ceiling, never a promise", async ({page}) => {
    await stubApi(page, {devices: FLEET, groups: [HALLS]});
    await page.goto(".");
    await expect(page.getByRole("article").first()).toBeVisible();

    // Five is everything a 1440px window has room for without taking a card
    // below `CARD_FLOOR`, so "Up to 6" comes back as five — the floor refusing
    // to squish rather than a bug. Six needs about 1700px of window; see the
    // test below, which pins where that happens.
    const ROOM_FOR = 5;

    for (const n of [2, 3, 4, 5, 6]) {
      await page.getByLabel("Per row").selectOption(String(n));
      const got = await columnCount(page);

      // Never more than was asked for.
      expect(got).toBeLessThanOrEqual(n);
      // And exactly n while there is room — not n - 1. The track is
      // deliberately a pixel under the width that divides evenly, because
      // asking `auto-fill` for the exact figure leaves its floor() sitting on
      // the boundary.
      expect(got).toBe(Math.min(n, ROOM_FOR));
    }
  });

  test("a wider window never shows fewer cards per row", async ({page}) => {
    await stubApi(page, {devices: FLEET, groups: [HALLS]});
    await page.goto(".");
    await expect(page.getByRole("article").first()).toBeVisible();

    const widths = [768, 1024, 1152, 1280, 1440, 1920, 2560];
    const counts: number[] = [];
    for (const width of widths) {
      await page.setViewportSize({width, height: 900});
      counts.push(await columnCount(page));
    }

    // Monotonic. It was not: the gutters grew with the viewport while
    // `--max-width` held the content still, so the row ran out of room the
    // wider the window got. Measured before the gutter was fixed, this came
    // back [2, 2, 3, 2, 2, 2, 2] — three columns in a narrow band around
    // 1152px and two on every monitor larger than that.
    expect(counts).toEqual([...counts].sort((a, b) => a - b));

    // And the page uses the room it is given, rather than holding a narrow
    // column of cards in the middle of a wide screen.
    expect(counts.at(-1)).toBe(4);
  });

  test("six per row needs the window `--max-width` was sized for", async ({
    page,
  }) => {
    await stubApi(page, {devices: FLEET, groups: [HALLS]});
    await page.goto(".");
    await expect(page.getByRole("article").first()).toBeVisible();
    await page.getByLabel("Per row").selectOption("6");

    // `--max-width` is 106.5rem because that is what six cards at `CARD_FLOOR`
    // need, gutters and container padding included. The consequence is a hard
    // edge rather than a gentle one, and it is worth pinning: a window a few
    // pixels under gives five, and nothing about the page says why.
    await page.setViewportSize({width: 1690, height: 900});
    expect(await columnCount(page)).toBe(5);

    await page.setViewportSize({width: 1700, height: 900});
    expect(await columnCount(page)).toBe(6);
  });

  test("the control bar is exactly as wide as the grid", async ({page}) => {
    await stubApi(page, {devices: FLEET, groups: [HALLS]});
    await page.goto(".");
    await expect(page.getByRole("article").first()).toBeVisible();

    const edges = await page.evaluate(() => {
      const bar = document
        .querySelector("[class*='bar']")!
        .getBoundingClientRect();
      const cards = [...document.querySelectorAll("article")].map(c =>
        c.getBoundingClientRect(),
      );
      return {
        bar: [Math.round(bar.left), Math.round(bar.right)],
        cards: [
          Math.round(Math.min(...cards.map(c => c.left))),
          Math.round(Math.max(...cards.map(c => c.right))),
        ],
      };
    });
    expect(edges.bar).toEqual(edges.cards);
  });
});

test.describe("phone", () => {
  test.use({viewport: {width: 390, height: 844}});

  test("a column count is ignored where it cannot be honoured", async ({page}) => {
    await stubApi(page, {devices: FLEET, groups: [HALLS]});
    await page.goto(".");
    await expect(page.getByRole("article").first()).toBeVisible();

    await page.getByLabel("Per row").selectOption("6");
    // One, whatever was asked for. The stylesheet declines to apply a count
    // below 48rem rather than dividing the width six ways.
    expect(await columnCount(page)).toBe(1);
  });
});
