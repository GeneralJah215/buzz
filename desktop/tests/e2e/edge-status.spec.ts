import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

/**
 * SPEC-2026-08-05 acceptance item 18, through the assembled application.
 *
 * The node tests mount the edge components directly. This spec exercises the
 * part they structurally cannot: that the surfaces are actually REACHED by the
 * real app — the provider is mounted in the tree, `MessageRow` renders the
 * badge, the settings section is registered in all five places it has to be,
 * and the nav entry appears and disappears with the sidecar.
 *
 * The IPC at the far end is the `mockIPC` bridge, not a live Rust process, so
 * this is not a full-stack integration test. What it does cover is every
 * frontend seam between a Tauri command name and a pixel, which is where the
 * defects this checkpoint was opened for actually live.
 *
 * The first test is the load-bearing one: with no `edgeStatus` seed the bridge
 * rejects with the same sentinel a default install returns, and NOTHING about
 * the feature may be visible.
 */

const GENERAL_CHANNEL_ID = "9a1657ac-f7aa-5db0-b632-d8bbeb6dfb50";

const EDGE_SEED = {
  summary: {
    pending: 2,
    pendingViaDigest: 4,
    claimed: 0,
    syncedExact: 9,
    syncedViaDigest: 1,
    quarantined: 1,
  },
  quarantined: [
    {
      eventId:
        "8e39cba681211b3782d0e4483e9343719b9b7be66515252da5491f26421896b1",
      channelId: GENERAL_CHANNEL_ID,
      author:
        "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      createdAt: 1_780_000_000,
      attempts: 4,
      reason: "upstream rejected: created_at outside ingest window",
      carriedByDigest: false,
      demotionReason: null,
      updatedAt: 1_780_000_600,
    },
  ],
  waitingAuthors: [
    {
      author:
        "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      pending: 2,
      ancestorBlocked: 0,
      pendingViaDigest: 4,
      oldestPendingAt: 1_780_000_000,
      oldestClaimableAt: 1_780_000_400,
      // No blocked rows, so no blocked age — never a borrowed one.
      oldestAncestorBlockedAt: null,
    },
  ],
} as const;

async function openGeneralChannel(page: import("@playwright/test").Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("message-row").first()).toBeVisible({
    timeout: 15_000,
  });
}

/**
 * Records whether the edge-sync nav entry or panel was EVER attached to the
 * document, rather than whether it is attached now.
 *
 * A poll-and-assert-absence check cannot see a flash: a regression to gating on
 * `!unavailable` (which starts `false`) puts the nav entry on screen for exactly
 * one IPC round-trip — well under 100 ms — and then removes it, and every
 * "expect count 0" afterwards passes. `MutationObserver` queues a record for a
 * node that is added and removed inside a single task, so the flash is caught
 * even though no poll interval could ever have sampled it.
 */
async function watchForEdgeSyncFlash(page: import("@playwright/test").Page) {
  await page.addInitScript(() => {
    const selector =
      '[data-testid="settings-nav-edge-sync"],[data-testid="settings-edge-sync"]';
    const scope = window as unknown as { __edgeSyncEverInDom?: boolean };
    scope.__edgeSyncEverInDom = false;
    const matches = (node: Node) =>
      node instanceof Element &&
      (node.matches(selector) || node.querySelector(selector) !== null);
    const observer = new MutationObserver((records) => {
      for (const record of records) {
        const touched = [...record.addedNodes, ...record.removedNodes];
        if (touched.some(matches)) {
          scope.__edgeSyncEverInDom = true;
          observer.disconnect();
          return;
        }
      }
    });
    observer.observe(document, { childList: true, subtree: true });
  });
}

async function openSettings(page: import("@playwright/test").Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("open-settings").click();
  await page.getByTestId("profile-popover-settings").click();
  await expect(page.getByTestId("settings-view")).toBeVisible();
}

test.describe("edge status surfaces", () => {
  test("with no sidecar the feature leaves no trace in settings", async ({
    page,
  }) => {
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));

    await watchForEdgeSyncFlash(page);
    await installMockBridge(page);
    await openSettings(page);

    // The nav entry never appears. Given a beat for the first poll to land, so
    // this is "absent after the answer", not "absent because nothing ran yet".
    await page.waitForTimeout(1_000);
    await expect(page.getByTestId("settings-nav-edge-sync")).toHaveCount(0);
    await expect(page.getByTestId("settings-edge-sync")).toHaveCount(0);

    // And it was never there for a frame either. Absence sampled once a second
    // is not the property this test is named for; this is.
    const everAttached = await page.evaluate(
      () =>
        (window as unknown as { __edgeSyncEverInDom?: boolean })
          .__edgeSyncEverInDom === true,
    );
    expect(everAttached).toBe(false);

    // Nothing anywhere on the settings surface mentions the feature.
    const body = await page.locator("body").innerText();
    expect(body).not.toContain("Local sync");
    expect(body).not.toContain("edge sidecar");

    // And the rejection is handled, not thrown into the page. An unhandled
    // rejection here is how "the sidecar is off" would become a red console on
    // every user's machine.
    expect(pageErrors).toEqual([]);
  });

  test("with a sidecar the quarantine list and waiting-for-author notice render", async ({
    page,
  }) => {
    await installMockBridge(page, { edgeStatus: { ...EDGE_SEED } });
    await openSettings(page);

    await page.getByTestId("settings-nav-edge-sync").click();
    const card = page.getByTestId("settings-edge-sync");
    await expect(card).toBeVisible({ timeout: 10_000 });

    // Waiting-for-author, driven by the seeded command response.
    await expect(
      card.getByLabel("Identities with events waiting to sync"),
    ).toContainText("2 events queued");

    // The four digest-carried rows are reported separately and are never added
    // into the waiting figure — no author is coming for them.
    await expect(card).toContainText(
      "4 more events will be carried to canonical history by this machine",
    );

    // Quarantine list, with a live retry control on a retryable row.
    await expect(card).toContainText(
      "upstream rejected: created_at outside ingest window",
    );
    await expect(
      card.locator('[aria-label^="Retry sync for event"]'),
    ).toBeVisible();

    // The two delivery axes stay separately labelled.
    const summary = card.getByTestId("edge-delivery-summary");
    await expect(summary).toContainText("Delivered locally");
    await expect(summary).toContainText("Synced to history");
  });

  test("a stuck own message is badged in the timeline", async ({ page }) => {
    await installMockBridge(page, {
      edgeStatus: { ...EDGE_SEED, deliveryStateForAll: "quarantined" },
    });
    await openGeneralChannel(page);

    const badges = page.getByTestId("message-delivery-state");
    await expect(badges.first()).toBeVisible({ timeout: 10_000 });
    await expect(badges.first()).toHaveText("Sync failed");

    // Only the current user's rows carry it. Every other identity's queue is
    // reported in Local sync settings, where there is room to say what to do.
    //
    // Asserted per row, not as `badgeCount < rowCount`: the sidecar is seeded
    // to report EVERY id as stuck, so a count comparison passes just as happily
    // with the ownership gate inverted — badges on everyone else's rows and
    // none on the viewer's is also "fewer badges than rows".
    const rows = page.getByTestId("message-row");
    // The first seed message in #general is from the active identity and the
    // second is from alice (the same fixture `identity-archive.spec.ts` uses).
    const mine = rows.first();
    const alices = rows.nth(1);
    await expect(mine.getByTestId("message-delivery-state")).toHaveCount(1);
    await expect(alices.getByTestId("message-delivery-state")).toHaveCount(0);
  });

  test("a quarantined message the digest is carrying does not read as stuck", async ({
    page,
  }) => {
    // BUG-023 through the assembled app: the same row, the same `quarantined`
    // label, and opposite advice. The quarantine list has always been able to
    // say "the edge is already carrying this upstream"; the badge could only
    // ever say "Sync failed", which reads as something to act on.
    await installMockBridge(page, {
      edgeStatus: {
        ...EDGE_SEED,
        deliveryStateForAll: "quarantined",
        carriedByDigestForAll: true,
      },
    });
    await openGeneralChannel(page);

    const badge = page.getByTestId("message-delivery-state").first();
    await expect(badge).toBeVisible({ timeout: 10_000 });
    await expect(badge).toHaveText("Sync failed, carried by digest");
    await expect(badge).toHaveAttribute("title", /nothing to retry/i);
  });

  test("a message that reached canonical history is not badged", async ({
    page,
  }) => {
    // The quiet rule. On a healthy machine nearly every row is `syncedExact`,
    // and a badge on all of them is a badge the operator stops reading.
    await installMockBridge(page, {
      edgeStatus: { ...EDGE_SEED, deliveryStateForAll: "syncedExact" },
    });
    await openGeneralChannel(page);
    await page.waitForTimeout(1_500);
    await expect(page.getByTestId("message-delivery-state")).toHaveCount(0);
  });
});
