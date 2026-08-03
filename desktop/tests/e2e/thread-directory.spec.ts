import { expect, test } from "@playwright/test";

import { waitForAnimations } from "../helpers/animations";
import {
  installMockBridge,
  waitForMockLiveSubscription,
} from "../helpers/bridge";

// This test deliberately starts with only the channel id. The mock emitter
// holds the root id just long enough to attach replies, and every subsequent
// discovery uses the rendered title rather than an event-id selector.
const CHANNEL_ID = "1c7e1c02-87bb-5e88-b2da-5a7a9432d0c9";
const CHANNEL_NAME = "engineering";
const GENERATED_TITLE = "Thread directory acceptance root";
const GENERATED_ROOT_CONTENT = `> 1. # ${GENERATED_TITLE}`;
const SHARED_TITLE = "Release readiness";

async function expectThreadDirectoryState(
  page: import("@playwright/test").Page,
  expected: { title: string; pinned: boolean; archived: boolean },
) {
  await expect
    .poll(() =>
      page.evaluate(
        (expectedState) =>
          window.__BUZZ_E2E_SIGNED_EVENTS__?.some((event) => {
            if (event.kind !== 40009) return false;
            try {
              const state: unknown = JSON.parse(event.content);
              return (
                typeof state === "object" &&
                state !== null &&
                (state as Record<string, unknown>).title ===
                  expectedState.title &&
                (state as Record<string, unknown>).pinned ===
                  expectedState.pinned &&
                (state as Record<string, unknown>).archived ===
                  expectedState.archived
              );
            } catch {
              return false;
            }
          }),
        expected,
      ),
    )
    .toBe(true);
}

test("thread directory preserves shared state and archived history across restart", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await installMockBridge(page, { persistThreadDirectorySession: true });
  await page.goto("/");

  // The ordinary channel subscription is the existing mock live seam. Do not
  // pre-seed a root id in browser/client state: create it only after the live
  // transport is listening, then retain it only inside this evaluate callback
  // while its descendants are authored.
  await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
  await waitForMockLiveSubscription(page, CHANNEL_NAME);
  await page.evaluate(
    ({ channelName, title }) => {
      const emit = window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__;
      if (!emit) throw new Error("mock message emitter is not installed");

      const root = emit({ channelName, content: title });
      for (let index = 1; index <= 3; index += 1) {
        emit({
          channelName,
          content: `Thread directory acceptance reply ${index}`,
          parentEventId: root.id,
        });
      }
    },
    { channelName: CHANNEL_NAME, title: GENERATED_ROOT_CONTENT },
  );

  const disclosure = page.getByTestId(
    `thread-directory-disclosure-${CHANNEL_ID}`,
  );
  await disclosure.click();
  await expect
    .poll(() =>
      page.evaluate(() => {
        const entry = window.__BUZZ_E2E_COMMAND_LOG__
          ?.slice()
          .reverse()
          .find((candidate) => candidate.command === "get_thread_directory");
        if (!entry?.payload || typeof entry.payload !== "object") {
          return null;
        }
        const payload = entry.payload as Record<string, unknown>;
        return {
          channelId: payload.channelId,
          cursor: payload.cursor,
          directoryState: payload.directoryState,
          limitRows: payload.limitRows,
          hasEventId: "eventId" in payload,
          hasMessageId: "messageId" in payload,
          hasRootId: "rootId" in payload,
          hasThreadRootId: "threadRootId" in payload,
        };
      }),
    )
    .toEqual({
      channelId: CHANNEL_ID,
      cursor: null,
      directoryState: "active",
      limitRows: 25,
      hasEventId: false,
      hasMessageId: false,
      hasRootId: false,
      hasThreadRootId: false,
    });

  // Selector contract for the sidebar implementation:
  // - disclosure: thread-directory-disclosure-${channelId}
  // - active/archive containers: thread-directory-active|archived-${channelId}
  // - each row: thread-directory-item, found by visible resolved title
  // - row actions: accessible name "More actions for <title>"
  // - actions: Rename thread, Pin thread, Archive thread, Restore thread
  // - rename input: accessible label "Thread name"; pinned state: "Pinned"
  const active = page.getByTestId(`thread-directory-active-${CHANNEL_ID}`);
  const generatedItem = active
    .getByTestId("thread-directory-item")
    .filter({ hasText: GENERATED_TITLE });
  await expect(generatedItem).toBeVisible();

  await generatedItem.click();
  await expect
    .poll(() => {
      const url = new URL(page.url());
      return {
        messageId: url.searchParams.get("messageId"),
        threadRootId: url.searchParams.get("threadRootId"),
      };
    })
    .toMatchObject({
      messageId: expect.stringMatching(/\S/),
      threadRootId: expect.stringMatching(/\S/),
    });
  await expect
    .poll(() => {
      const url = new URL(page.url());
      return (
        url.searchParams.get("messageId") ===
        url.searchParams.get("threadRootId")
      );
    })
    .toBe(true);
  await expect(page.getByTestId("message-thread-panel")).toBeVisible();
  await expect(
    page.getByTestId("message-thread-panel").getByText(GENERATED_TITLE),
  ).toBeVisible();

  await page
    .getByRole("button", { name: `More actions for ${GENERATED_TITLE}` })
    .click();
  await page.getByRole("menuitem", { name: "Rename thread" }).click();
  await page.getByLabel("Thread name").fill(SHARED_TITLE);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(active.getByText(SHARED_TITLE)).toBeVisible();

  await page
    .getByRole("button", { name: `More actions for ${SHARED_TITLE}` })
    .click();
  await page.getByRole("menuitem", { name: "Pin thread" }).click();
  await expect(active.getByText("Pinned", { exact: true })).toBeVisible();
  await expectThreadDirectoryState(page, {
    title: SHARED_TITLE,
    pinned: true,
    archived: false,
  });

  // Opt-in mock persistence mirrors an app restart: both the root/replies and
  // shared kind-40009 state must be available to a fresh directory query.
  await page.reload();
  await page.getByTestId(`thread-directory-disclosure-${CHANNEL_ID}`).click();
  const activeAfterRestart = page.getByTestId(
    `thread-directory-active-${CHANNEL_ID}`,
  );
  const sharedItem = activeAfterRestart
    .getByTestId("thread-directory-item")
    .filter({ hasText: SHARED_TITLE });
  await expect(sharedItem).toBeVisible();
  await expect(
    activeAfterRestart.getByText("Pinned", { exact: true }),
  ).toBeVisible();

  await page
    .getByRole("button", { name: `More actions for ${SHARED_TITLE}` })
    .click();
  await page.getByRole("menuitem", { name: "Archive thread" }).click();
  await expect(
    activeAfterRestart
      .getByTestId("thread-directory-item")
      .filter({ hasText: SHARED_TITLE }),
  ).toHaveCount(0);

  await page.getByRole("button", { name: "Archived threads" }).click();
  const archived = page.getByTestId(`thread-directory-archived-${CHANNEL_ID}`);
  const archivedItem = archived
    .getByTestId("thread-directory-item")
    .filter({ hasText: SHARED_TITLE });
  await expect(archivedItem).toBeVisible();
  await archivedItem.click();
  const threadPanel = page.getByTestId("message-thread-panel");
  await expect(
    threadPanel.getByText("Thread directory acceptance reply 1"),
  ).toBeVisible();
  await expect(
    threadPanel.getByText("Thread directory acceptance reply 2"),
  ).toBeVisible();
  await expect(
    threadPanel.getByText("Thread directory acceptance reply 3"),
  ).toBeVisible();

  await page
    .getByRole("button", { name: `More actions for ${SHARED_TITLE}` })
    .click();
  await expectThreadDirectoryState(page, {
    title: SHARED_TITLE,
    pinned: false,
    archived: true,
  });

  await page.getByRole("menuitem", { name: "Restore thread" }).click();
  await expectThreadDirectoryState(page, {
    title: SHARED_TITLE,
    pinned: false,
    archived: false,
  });
  await expect(
    archived
      .getByTestId("thread-directory-item")
      .filter({ hasText: SHARED_TITLE }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Active threads" }).click();
  await expect(
    page
      .getByTestId(`thread-directory-active-${CHANNEL_ID}`)
      .getByTestId("thread-directory-item")
      .filter({ hasText: SHARED_TITLE }),
  ).toBeVisible();

  await waitForAnimations(page);
  await page.screenshot({
    path: "test-results/thread-directory-sidebar.png",
    clip: { x: 0, y: 0, width: 256, height: 720 },
  });
});
