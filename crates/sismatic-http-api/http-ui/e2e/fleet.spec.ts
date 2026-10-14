// A smoke test over the fleet page: enough to prove the setup works and to be
// a pattern for the rest, not an attempt to cover the page.
//
// What each one asserts is deliberately something a unit test cannot: that the
// browser laid it out, painted it, and that clicking it reaches the API. The
// filtering and settings logic is pure and is tested without a browser.
import {expect, test} from "@playwright/test";
import {badge, card} from "./locators";
import {SILENT_HALLS, stubApi} from "./stubApi";

test("the fleet renders a card per group and device", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  await expect(page.getByRole("heading", {name: "Sismatic Fleet Overview"})).toBeVisible();

  // Groups first, then devices, each in the server's id order.
  await expect(page.getByRole("article")).toHaveCount(4);
  await expect(page.getByRole("heading", {level: 3})).toHaveText([
    "Halls",
    "Hall A",
    "Hall B",
    "Hall C",
  ]);
});

test("the recording state is visible without opening anything", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  await expect(badge(page, "Hall A", "Recording")).toBeVisible();
  // The title is pinned by default, so it is readable with the folds shut.
  await expect(card(page, "Hall A").getByText("Morning session")).toBeVisible();

  // A group whose members disagree says so rather than picking a side.
  await expect(badge(page, "Halls", "Mixed")).toBeVisible();
  // And a device that has never answered is "Unknown", not absent.
  await expect(badge(page, "Hall C", "Unknown")).toBeVisible();
});

test("the text filter searches every field, members included", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  await page.getByLabel("Filter by text").fill("afternoon");

  // Hall B holds that title — and so the group holding Hall B matches too.
  // That is the intended answer, not a leak: a search that found the recorder
  // but hid the room it is in would be the surprising one.
  await expect(page.getByRole("heading", {level: 3})).toHaveText([
    "Halls",
    "Hall B",
  ]);
  await expect(page.getByText("2 of 4")).toBeVisible();
});

test("only groups, or only devices", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  await page.getByLabel("Show").selectOption("group");
  await expect(page.getByRole("heading", {level: 3})).toHaveText(["Halls"]);

  await page.getByLabel("Show").selectOption("device");
  await expect(page.getByRole("heading", {level: 3})).toHaveText([
    "Hall A",
    "Hall B",
    "Hall C",
  ]);
});

test("a register is written before the recording it belongs to starts", async ({
  page,
}) => {
  const submitted = await stubApi(page);
  await page.goto(".");

  // Hall C has answered nothing, so its registers are not sealed.
  const hallC = card(page, "Hall C");

  await hallC.getByText("Registers").click();
  await hallC.getByLabel("Presenter", {exact: true}).fill("Jane Doe");

  await hallC.getByRole("button", {name: "Start recording"}).click();

  // The order is the point: metadata is sealed the instant a recording is
  // admitted, so the register has to be accepted first or the take is
  // recorded under the previous title.
  await expect.poll(() => submitted.length).toBeGreaterThanOrEqual(2);
  expect(submitted[0]).toMatchObject({
    method: "PUT",
    path: "/v1/writes/devices/Hall C/metadata/PRESENTER",
    body: {value: "Jane Doe"},
  });
  expect(submitted[1]).toMatchObject({
    method: "POST",
    path: "/v1/writes/devices/Hall C/recording/start",
  });

  // The space in "Hall C" has to be escaped to reach the route at all, and
  // every id in the shipped devices file has one.
  expect(submitted[0].raw).toContain("Hall%20C");
});

test("the theme switch reaches the document, not the components", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  const root = page.locator("html");
  // "System" is the absence of a choice: no attribute, so the stylesheet's
  // media query decides.
  await expect(root).not.toHaveAttribute("data-theme", /.+/);

  await page.getByLabel("Theme").selectOption("dark");
  await expect(root).toHaveAttribute("data-theme", "dark");

  // And the background really changed, which is the part no unit test sees.
  const background = await page.evaluate(() =>
    getComputedStyle(document.body).backgroundColor,
  );
  expect(background).toBe("rgb(18, 18, 20)");
});

test("settings survive a reload", async ({page}) => {
  await stubApi(page);
  await page.goto(".");

  await page.getByLabel("Theme").selectOption("dark");
  await page.getByLabel("Per row").selectOption("2");

  // No second `stubApi`: routes belong to the page and outlive a reload.
  await page.reload();

  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.getByLabel("Per row")).toHaveValue("2");
});

test("a group names its devices even when none of them has answered", async ({
  page,
}) => {
  // The case this exists for: nothing is reachable, so `GET /v1/reads/groups`
  // comes back with `fields: []` and cannot name a single member.
  await stubApi(page, {devices: [], groups: [SILENT_HALLS]});
  await page.goto(".");

  const halls = card(page, "Halls");
  await expect(halls.getByText(/3 devices/)).toBeVisible();

  await halls.getByText("Details").click();
  for (const id of ["Hall A", "Hall B", "Hall C"]) {
    await expect(halls.getByText(id, {exact: true})).toBeVisible();
  }
  // Three member rows, and nothing pretending to know their state: the group's
  // own badge reads Unknown, as does every member's.
  await expect(halls.getByRole("listitem")).toHaveCount(3);
  await expect(badge(page, "Halls", "Unknown")).toBeVisible();
});

test("an unreachable inventory degrades instead of blanking the page", async ({
  page,
}) => {
  await stubApi(page, {devices: [], groups: [SILENT_HALLS], inventory: []});
  await page.goto(".");

  // The card is still drawn and still says what it knows — it just cannot say
  // who is in the group, which is what the reads alone could ever manage.
  const halls = card(page, "Halls");
  await expect(halls).toBeVisible();
  await expect(halls.getByText(/0 devices/)).toBeVisible();
});
