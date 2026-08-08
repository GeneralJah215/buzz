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

    await installMockBridge(page);
    await openSettings(page);

    // The nav entry never appears. Given a beat for the first poll to land, so
    // this is "absent after the answer", not "absent because nothing ran yet".
    await page.waitForTimeout(1_000);
    await expect(page.getByTestId("settings-nav-edge-sync")).toHaveCount(0);
    await expect(page.getByTestId("settings-edge-sync")).toHaveCount(0);

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
    const rowCount = await page.getByTestId("message-row").count();
    const badgeCount = await badges.count();
    expect(badgeCount).toBeGreaterThan(0);
    expect(badgeCount).toBeLessThan(rowCount);
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
