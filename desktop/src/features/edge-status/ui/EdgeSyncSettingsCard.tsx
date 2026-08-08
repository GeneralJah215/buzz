/**
 * The Local sync settings panel: where the two triage surfaces live.
 *
 * `QuarantineList` and `WaitingForAuthorNotice` are consulted when something is
 * already wrong, not read continuously, so they belong on a settings surface
 * rather than in the conversation. Putting them in the timeline would put a
 * standing amber block above the composer on any machine with one stuck agent
 * identity, for a condition whose fix is never in the chat.
 *
 * The section this renders into is hidden outright when the sidecar is absent
 * (`isEdgeSyncSectionVisible`, below), and this component refuses to render on
 * its own account as well. A user who has never heard of the edge sidecar must
 * see no trace of it — not an empty state, not an "unavailable" note, not a
 * disabled row.
 */

import * as React from "react";

import {
  EDGE_DELIVERY_STATE_WIRE_NAMES,
  fetchEdgeQuarantinedEvents,
  isEdgeUnavailableError,
  type EdgeDeliverySummary,
  type EdgeQuarantinedEvent,
} from "@/features/edge-status/api/edgeStatus";
import type { EdgeStatus } from "@/features/edge-status/hooks";
import { deliveryLabel } from "@/features/edge-status/lib/deliveryState";
import { QuarantineList } from "@/features/edge-status/ui/QuarantineList";
import { WaitingForAuthorNotice } from "@/features/edge-status/ui/WaitingForAuthorNotice";
import { SettingsSectionHeader } from "@/features/settings/ui/SettingsSectionHeader";
import { Button } from "@/shared/ui/button";

/**
 * Should the Local sync nav entry and panel exist at all?
 *
 * One latched bit: `status.hasAnswered`. Not `!unavailable` — that starts
 * `false` on a machine that has never contacted the sidecar, so gating on it
 * would show the section to everyone for one IPC round-trip and then take it
 * away. And not `summary !== null || error !== null` either, for two reasons
 * the latch fixes at once:
 *
 *   - `summary` goes null on the FIRST rejection, so a sidecar restart used to
 *     yank the section out from under an operator mid-triage — the nav filter
 *     in `SettingsView` would drop `edge-sync` and its fallback effect would
 *     navigate them to Profile during exactly the blip they were debugging.
 *   - `error !== null` made visibility rest on a byte-exact string. "No sidecar
 *     here" is one exact message; any drift in it turns an ordinary rejection
 *     on a machine that never installed the feature into a full "Local sync"
 *     section for a user who has never heard of it.
 *
 * `hasAnswered` is positive evidence instead: a payload came back, either one
 * this build read (`summary`) or one it could not (`EdgeStatusShapeError`).
 *
 * A non-sentinel rejection also opens the section, and deliberately so. It is
 * the one case where hiding costs more than showing: a sidecar that 503s from
 * app start and never once succeeds would otherwise leave its operator no
 * surface at all — no list, no banner, nothing but a console line — for a
 * machine that plainly HAS a sidecar and cannot talk to it. That is the exact
 * situation this panel exists for.
 *
 * The leak this was hardened against cannot reach an uninterested user:
 * `edge_post` answers with the sentinel both when no binding is configured and
 * on any transport failure, so a non-sentinel rejection implies a resolved
 * binding, which implies `BUZZ_EDGE_RELAY_URL` is set, which implies the user
 * opted in. Someone who has never heard of the sidecar cannot produce one.
 */
export function isEdgeSyncSectionVisible(status: EdgeStatus): boolean {
  return status.hasAnswered || status.error !== null;
}

/** Summary rows, in the order the outbox moves through them. */
const SUMMARY_ROWS: Array<{
  key: keyof EdgeDeliverySummary;
  state: string;
}> = [
  { key: "pending", state: EDGE_DELIVERY_STATE_WIRE_NAMES.pending },
  {
    key: "pendingViaDigest",
    state: EDGE_DELIVERY_STATE_WIRE_NAMES.pendingViaDigest,
  },
  { key: "claimed", state: EDGE_DELIVERY_STATE_WIRE_NAMES.claimed },
  { key: "syncedExact", state: EDGE_DELIVERY_STATE_WIRE_NAMES.syncedExact },
  {
    key: "syncedViaDigest",
    state: EDGE_DELIVERY_STATE_WIRE_NAMES.syncedViaDigest,
  },
  { key: "quarantined", state: EDGE_DELIVERY_STATE_WIRE_NAMES.quarantined },
];

export function EdgeSyncSettingsCard({ status }: { status: EdgeStatus }) {
  const { error, hasAnswered, refresh, summary, waitingAuthors } = status;
  const [quarantined, setQuarantined] = React.useState<EdgeQuarantinedEvent[]>(
    [],
  );
  // The quarantine page has its own failure mode and therefore needs its own
  // error slot. It used to have none: a non-sentinel rejection was caught and
  // dropped, so a disk-I/O error left "Sync failed: 2" in the summary beside an
  // empty list, with nothing on screen saying the list had failed to load. The
  // one surface whose entire job is answering "is anything stuck?" must not
  // answer it with silence.
  const [quarantineError, setQuarantineError] = React.useState<Error | null>(
    null,
  );

  // Driven off `summary`'s identity: `useEdgeStatus` hands back a fresh object
  // on every successful poll, so the quarantine page refreshes on the same
  // cadence without a second timer of its own.
  React.useEffect(() => {
    if (summary === null) {
      setQuarantined([]);
      setQuarantineError(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const rows = await fetchEdgeQuarantinedEvents();
        if (!cancelled) {
          setQuarantined(rows);
          setQuarantineError(null);
        }
      } catch (caught) {
        if (cancelled) {
          return;
        }
        // The sidecar stopped between the summary poll and this call. The
        // section is on its way out; drop the rows rather than leave a stale
        // list with live Retry buttons pointed at a process that is gone.
        setQuarantined([]);
        if (isEdgeUnavailableError(caught)) {
          setQuarantineError(null);
          return;
        }
        // Anything else — SQLite, a 503, an `EdgeStatusShapeError` whose whole
        // purpose is to never be mistaken for the benign case. Log it with
        // context and put it in the banner; never swallow it.
        console.warn(
          "[GUARDRAIL] edge_quarantined_events failed; reporting the fault instead of showing an empty list",
          caught,
        );
        setQuarantineError(
          caught instanceof Error ? caught : new Error(String(caught)),
        );
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [summary]);

  // Same latch the nav entry uses, so the panel and its nav row appear and
  // disappear together and a sidecar blip cannot blank one of them.
  if (!hasAnswered) {
    return null;
  }

  const faults = [
    error === null
      ? null
      : {
          key: "status",
          text: `The local sidecar answered, but not with something this app could use: ${error.message}`,
        },
    quarantineError === null
      ? null
      : {
          key: "quarantine",
          text: `The list of events that gave up could not be loaded, so anything below it is incomplete: ${quarantineError.message}`,
        },
  ].filter((fault) => fault !== null);

  return (
    <section className="space-y-4" data-testid="settings-edge-sync">
      <SettingsSectionHeader
        description="Messages are delivered to everyone on this machine the moment you send them. This is the separate question of whether they have also reached canonical history on the relay."
        title="Local sync"
      />

      {faults.length > 0 ? (
        <div
          className="space-y-1 rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive"
          data-testid="edge-sync-fault"
          role="alert"
        >
          {faults.map((fault) => (
            <p key={fault.key}>{fault.text}</p>
          ))}
        </div>
      ) : null}

      {summary ? (
        <dl
          className="grid grid-cols-2 gap-2 sm:grid-cols-3"
          data-testid="edge-delivery-summary"
        >
          {SUMMARY_ROWS.map(({ key, state }) => (
            <div
              className="rounded-lg border border-border/70 bg-background/70 p-3"
              key={key}
            >
              <dt className="text-xs text-muted-foreground">
                {deliveryLabel(state)}
              </dt>
              <dd className="text-sm font-semibold tabular-nums">
                {summary[key]}
              </dd>
            </div>
          ))}
        </dl>
      ) : null}

      <WaitingForAuthorNotice authors={waitingAuthors} />

      {quarantined.length > 0 ? (
        <div className="space-y-2">
          <h3 className="text-xs font-medium text-foreground">
            Gave up reaching canonical history
          </h3>
          <QuarantineList events={quarantined} />
        </div>
      ) : null}

      <Button onClick={refresh} size="sm" type="button" variant="outline">
        Refresh
      </Button>
    </section>
  );
}
