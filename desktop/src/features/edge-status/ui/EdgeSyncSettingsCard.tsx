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
 * Deliberately keyed on evidence of a sidecar rather than on `!unavailable`.
 * `unavailable` starts `false` on a machine that has never contacted the
 * sidecar, so gating on it would show the section to everyone for the length of
 * one IPC round-trip and then take it away — a visible layout shift advertising
 * a feature the user does not have.
 *
 * A genuine fault DOES open the section. On a machine with no sidecar every
 * rejection is the `EDGE_UNAVAILABLE_MESSAGE` sentinel and `error` stays null,
 * so this cannot leak; but on a machine that HAS one, a 503 or a binding
 * mismatch is exactly what the operator opened settings to find out about, and
 * hiding the only surface that could report it would be the worst answer.
 */
export function isEdgeSyncSectionVisible(status: EdgeStatus): boolean {
  return status.summary !== null || status.error !== null;
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
  const { error, refresh, summary, waitingAuthors } = status;
  const [quarantined, setQuarantined] = React.useState<EdgeQuarantinedEvent[]>(
    [],
  );

  // Driven off `summary`'s identity: `useEdgeStatus` hands back a fresh object
  // on every successful poll, so the quarantine page refreshes on the same
  // cadence without a second timer of its own.
  React.useEffect(() => {
    if (summary === null) {
      setQuarantined([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const rows = await fetchEdgeQuarantinedEvents();
        if (!cancelled) {
          setQuarantined(rows);
        }
      } catch (caught) {
        if (!cancelled && isEdgeUnavailableError(caught)) {
          // The sidecar stopped between the summary poll and this call. The
          // section is on its way out; drop the rows rather than leave a stale
          // list with live Retry buttons pointed at a process that is gone.
          setQuarantined([]);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [summary]);

  if (summary === null && error === null) {
    return null;
  }

  return (
    <section className="space-y-4" data-testid="settings-edge-sync">
      <SettingsSectionHeader
        description="Messages are delivered to everyone on this machine the moment you send them. This is the separate question of whether they have also reached canonical history on the relay."
        title="Local sync"
      />

      {error ? (
        <p
          className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive"
          role="alert"
        >
          The local sidecar answered, but not with something this app could use:{" "}
          {error.message}
        </p>
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
