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
  await page.getByLabel("Controls").selectOption("unlocked");

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
  await page.getByLabel("Controls").selectOption("unlocked");

  // No second `stubApi`: routes belong to the page and outlive a reload.
  await page.reload();

  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.getByLabel("Per row")).toHaveValue("2");
  await expect(page.getByLabel("Controls")).toHaveValue("unlocked");
});

test("nothing on a fresh page can write to the fleet", async ({page}) => {
  const submitted = await stubApi(page);
  await page.goto(".");

  // Hall B is stopped, so it offers exactly one verb and its registers are
  // not sealed: whatever turns its controls off here is the lock and not the
  // seal, which is the distinction this test exists to keep.
  const hallB = card(page, "Hall B");
  const start = hallB.getByRole("button", {name: "Start recording"});

  await expect(page.getByLabel("Controls")).toHaveValue("locked");
  await expect(start).toBeDisabled();

  await hallB.getByText("Registers").click();
  await expect(hallB.getByLabel("Title", {exact: true})).toBeDisabled();
  await expect(hallB.getByRole("button", {name: /^Save/})).toBeDisabled();
  await expect(hallB.getByRole("button", {name: "Clear all"})).toBeDisabled();

  // The buttons are still there, and still saying what the card believes: a
  // locked page has to be read differently from an empty one. And the card
  // says why, rather than leaving a grey button to look broken.
  await expect(start).toBeVisible();
  await expect(
    hallB.getByText("Read-only — set Controls above to Unlocked."),
  ).toBeVisible();

  // `force`, because the point is what the page does when a tap lands on a
  // control it has turned off.
  await start.click({force: true});
  await hallB.getByRole("button", {name: "Clear all"}).click({force: true});

  // That neither of them left the browser is proven by what turns up once the
  // page *is* unlocked: `submitted` holds the allowed click and nothing in
  // front of it. Asserting an empty array here instead would pass while a
  // request was still in flight.
  await page.getByLabel("Controls").selectOption("unlocked");
  await start.click();
  await expect.poll(() => submitted).toMatchObject([
    {method: "POST", path: "/v1/writes/devices/Hall B/recording/start"},
  ]);
});

test("unlocking the controls arms the whole page, and says so", async ({
  page,
}) => {
  await stubApi(page);
  await page.goto(".");

  const hallB = card(page, "Hall B");
  const start = hallB.getByRole("button", {name: "Start recording"});
  const armed = page.getByText(/^Controls are unlocked/);

  await expect(armed).toBeHidden();

  await page.getByLabel("Controls").selectOption("unlocked");

  // The page-wide signal, for an operator who is about to click something.
  await expect(armed).toBeVisible();
  await expect(start).toBeEnabled();
  await hallB.getByText("Registers").click();
  await expect(hallB.getByLabel("Title", {exact: true})).toBeEnabled();

  // And locking it again puts every one of those back.
  await page.getByLabel("Controls").selectOption("locked");
  await expect(armed).toBeHidden();
  await expect(start).toBeDisabled();
  await expect(hallB.getByLabel("Title", {exact: true})).toBeDisabled();
});

test("a dashboard set up before the lock existed comes back locked", async ({
  page,
}) => {
  await stubApi(page);
  await page.goto(".");

  // What a build that had never heard of this setting left behind. The field
  // is absent rather than false, and absence has to read as locked — the
  // alternative is every existing dashboard silently staying armed through
  // the upgrade that was supposed to protect it.
  await page.evaluate(() => {
    window.localStorage.setItem(
      "sismatic.fleet.settings.v1",
      JSON.stringify({query: "", kind: "all", status: "all", columns: 2}),
    );
  });
  await page.reload();

  // The rest of that blob was honoured, so this is a read of it and not a
  // fall back to the defaults, which are locked anyway and would prove
  // nothing.
  await expect(page.getByLabel("Per row")).toHaveValue("2");
  await expect(page.getByLabel("Controls")).toHaveValue("locked");
});

test("an edited register is described as unsaved, not renamed", async ({
  page,
}) => {
  await stubApi(page);
  await page.goto(".");
  await page.getByLabel("Controls").selectOption("unlocked");

  const hallB = card(page, "Hall B");
  await hallB.getByText("Registers").click();

  const title = hallB.getByLabel("Title", {exact: true});
  await title.fill("Afternoon lab, take 2");
  await expect(hallB.getByText("unsaved", {exact: true})).toBeVisible();

  // Both of those lines are *about* the box; neither is its name. The version
  // this replaced put the mark inside the `<label>`, which renamed the field
  // to "Title unsaved" halfway through someone typing into it — wrong as a
  // name, and a name that moves under a screen reader at the worst moment.
  await expect(title).toHaveAccessibleName("Title");
  await expect(title).toHaveAccessibleDescription(
    "unsaved On the devices: Afternoon lab",
  );
});

test("hiding the controls leaves a card with nothing to press", async ({
  page,
}) => {
  await stubApi(page);
  await page.goto(".");

  const hallB = card(page, "Hall B");

  await page.getByLabel("Controls").selectOption("hidden");

  // Not turned off — gone. The read-only state's reasons for keeping a
  // greyed-out Stop on the card all assume somebody is going to unlock the
  // page; this is the state for a page nobody will, so the line that explains
  // the greyed-out version goes too. Nothing on a card mentions controls it
  // does not have.
  await expect(hallB.getByRole("button", {name: /recording/})).toHaveCount(0);
  await expect(
    hallB.getByText("Read-only — set Controls above to Unlocked."),
  ).toHaveCount(0);

  // The registers stay, and stay readable: the fold, the labels, and what the
  // devices report in each one.
  await hallB.getByText("Registers").click();
  const title = hallB.getByLabel("Title", {exact: true});
  await expect(title).toHaveValue("Afternoon lab");
  // `readonly` and not `disabled`, which is the difference between a value and
  // a control that happens to be off — and with the controls hidden this box
  // is the card's only copy of the value.
  await expect(title).toBeEnabled();
  await expect(title).not.toBeEditable();
  await expect(hallB.getByRole("button", {name: /^Save/})).toHaveCount(0);
  await expect(hallB.getByRole("button", {name: "Clear all"})).toHaveCount(0);

  // The whole card, with both folds open: not one button on it. The folds
  // themselves are `<details>`, so opening them is not a button either.
  await hallB.getByText("Details").click();
  await expect(hallB.getByRole("button")).toHaveCount(0);

  // Remembered like the rest of the bar — the point of the state is a page
  // that is left somewhere, and a reload must not hand the buttons back.
  await page.reload();
  await expect(page.getByLabel("Controls")).toHaveValue("hidden");
  await expect(hallB.getByRole("button", {name: /recording/})).toHaveCount(0);

  // And read-only puts them back, in the state read-only draws them in.
  await page.getByLabel("Controls").selectOption("locked");
  const start = hallB.getByRole("button", {name: "Start recording"});
  await expect(start).toBeVisible();
  await expect(start).toBeDisabled();
});

test("a draft stops being shown as a value once the controls are hidden", async ({
  page,
}) => {
  await stubApi(page);
  await page.goto(".");

  const hallB = card(page, "Hall B");
  await page.getByLabel("Controls").selectOption("unlocked");
  await hallB.getByText("Registers").click();

  const title = hallB.getByLabel("Title", {exact: true});
  await title.fill("Afternoon lab, take 2");
  // `exact`, because the fold's own summary line says "1 unsaved" too.
  await expect(hallB.getByText("unsaved", {exact: true})).toBeVisible();

  // A page with no controls on it reads as a read of the fleet, so it shows
  // the fleet: the box goes back to what Hall B reports and the mark that
  // tracks an edit goes with the editor that made it. A wall display sitting
  // there showing somebody's half-typed title as though it were the recording
  // metadata is the exact failure this page is otherwise careful about.
  await page.getByLabel("Controls").selectOption("hidden");
  await expect(title).toHaveValue("Afternoon lab");
  await expect(hallB.getByText("unsaved", {exact: true})).toHaveCount(0);

  // Nothing was thrown away, though. The draft was never the fleet's to lose,
  // and a setting in the bar is not an answer to "did you mean to discard
  // that?".
  await page.getByLabel("Controls").selectOption("unlocked");
  await expect(title).toHaveValue("Afternoon lab, take 2");
  await expect(hallB.getByText("unsaved", {exact: true})).toBeVisible();
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
