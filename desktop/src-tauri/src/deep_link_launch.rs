//! Recovery and routing of the `buzz://` link that *launched* the app.
//!
//! Split out of `deep_link.rs` to keep that file under the repo's file-size
//! ratchet. The warm-open path, the dedup gate, and `handle_deep_link_url`
//! itself all stay there; this module owns the one question a launch link
//! raises and a warm open never does: at the moment the link arrives, does the
//! state its action needs actually exist yet?

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use tauri::Manager;
use url::Url;

use super::{deep_link_action, handle_deep_link_url};

/// When a `buzz://` action recovered from the command line at launch can
/// actually be *delivered* — as opposed to merely dispatched.
///
/// A launch link arrives at a moment the app did not choose: `setup`, at
/// `Ready`, with no window painted, no React tree mounted, and no workspace
/// applied. Every arm of [`handle_deep_link_url`] has a different tolerance for
/// that, so the disposition is a property of the action, decided once here
/// rather than guessed at each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchDelivery {
    /// Safe the instant the app object exists. These arms park their payload in
    /// [`super::PendingCommunityDeepLinks`] via `queue_community_deep_link`, and the
    /// frontend drains that queue when it mounts its listeners — so the
    /// delivery does not depend on anyone listening right now. Dispatching them
    /// at `setup` is also the only way they work at all during onboarding,
    /// where no community exists yet and `apply_workspace` is never called.
    Immediately,
    /// Must wait for `apply_workspace` (BUG-044).
    ///
    /// `restart-agent` with no `relay` param resolves its relay from the live
    /// runtime registry, which is empty on a cold start, and then falls through
    /// to `effective_agent_relay_url` → `state.relay_url_override`. That
    /// override is written by `commands::workspace::apply_workspace`, which
    /// needs React to mount — hundreds of milliseconds to seconds after
    /// `setup`. Starting an agent before it lands is not a race, it is a
    /// guarantee: the agent comes up on the `ws://localhost:3000` fallback,
    /// invisible on the real relay, and `restore.rs`'s "is any runtime with
    /// this pubkey alive?" check then suppresses the correct restore for the
    /// rest of the session. A link that does nothing is recoverable; an agent
    /// running on the wrong relay is not.
    AfterWorkspaceApply,
    /// Not deliverable at launch at all (BUG-045).
    ///
    /// These arms only `app.emit(...)`. They have no queue-and-drain backstop,
    /// and the frontend registers their listeners with a plain `listen` after
    /// React mounts (`desktop/src/shared/deep-link.ts`). An emit at launch
    /// therefore reaches nobody, and — worse — would let recovery write a
    /// success log and burn the one-shot latch over a payload it had just
    /// dropped. Recovery refuses them and says so, at WARN, naming the action.
    Unsupported,
}

/// Every action [`handle_deep_link_url`] accepts, paired with its launch
/// disposition.
///
/// This table and that `match` are two halves of one decision, so
/// `every_handled_action_has_a_launch_disposition` fails the build if an arm is
/// added here or there without the other. Unknown actions are refused rather
/// than replayed, so the failure mode of forgetting is a loud dropped link, not
/// an unbacked dispatch.
const LAUNCH_DELIVERY: &[(&str, LaunchDelivery)] = &[
    ("connect", LaunchDelivery::Immediately),
    ("join", LaunchDelivery::Immediately),
    ("add-community", LaunchDelivery::Immediately),
    ("message", LaunchDelivery::Unsupported),
    ("nostr-bind", LaunchDelivery::Unsupported),
    ("restart-agent", LaunchDelivery::AfterWorkspaceApply),
];

fn launch_delivery(action: &str) -> Option<LaunchDelivery> {
    LAUNCH_DELIVERY
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, delivery)| *delivery)
}

/// Launch deep links held between `setup` and `apply_workspace`, plus the
/// one-shot latch that stops the plugin's launch URL being read twice.
///
/// Same shape as [`super::PendingCommunityDeepLinks`] — a queue that outlives the
/// moment the link arrived — rather than a second, differently-shaped
/// mechanism. The difference is only who drains it: that one is drained by the
/// frontend over IPC, this one by `apply_workspace` on the Rust side, because
/// what it is waiting for is backend state, not a React tree.
#[derive(Default)]
pub(crate) struct PendingLaunchDeepLinks {
    urls: Mutex<VecDeque<String>>,
    /// Set the first time [`recover_cold_start_deep_link`] runs, so the launch
    /// URL can never be read out of the plugin twice (BUG-032). See that
    /// function for why replay, not loss, is the dangerous direction here.
    cold_start_recovered: AtomicBool,
}

impl PendingLaunchDeepLinks {
    fn stash(&self, urls: Vec<String>) {
        let mut queue = self
            .urls
            .lock()
            .expect("pending launch deep-link queue poisoned");
        queue.extend(urls);
    }

    /// Take everything stashed. Draining rather than peeking is what makes a
    /// second `apply_workspace` — every workspace switch is one — a no-op
    /// instead of a replay.
    fn drain(&self) -> Vec<String> {
        let mut queue = self
            .urls
            .lock()
            .expect("pending launch deep-link queue poisoned");
        queue.drain(..).collect()
    }
}

/// Split the URLs the deep-link plugin is holding at launch into "dispatch
/// now", "dispatch after `apply_workspace`", and "refuse". Pure, so the whole
/// of BUG-032/BUG-044/BUG-045 is decidable without a live `tauri::AppHandle`.
///
/// `already_recovered` is the process-wide latch on
/// [`PendingLaunchDeepLinks::cold_start_recovered`]. It is swapped, not merely
/// read, so two concurrent callers cannot both win.
fn recover_cold_start_urls<E: std::fmt::Display>(
    already_recovered: &AtomicBool,
    current: Result<Option<Vec<Url>>, E>,
) -> RecoveredLaunchLinks {
    let mut recovered = RecoveredLaunchLinks::default();
    if already_recovered.swap(true, Ordering::SeqCst) {
        // `get_current` is a plain getter over a field that is never cleared,
        // so a second read would hand back the same launch URL and restart an
        // agent nobody asked about. Refuse rather than trust the caller.
        tracing::warn!(
            event = "deep_link_cold_start_recovery_repeated",
            "[GUARDRAIL] refusing to replay the launch deep link a second time"
        );
        return recovered;
    }
    let urls = match current {
        Ok(urls) => urls.unwrap_or_default(),
        Err(error) => {
            // A recovery failure is not a normal outcome and must not vanish:
            // it means a launch link was silently dropped, which is the exact
            // shape of BUG-032.
            tracing::error!(
                event = "deep_link_cold_start_recovery_failed",
                error = %error,
                "[GUARDRAIL] could not read the launch deep link; it is lost"
            );
            return recovered;
        }
    };
    for url in &urls {
        let action = deep_link_action(url.as_str());
        // Each branch logs what it actually did with the link, and nothing
        // more: the two refusals below say "dropped", not "recovered"
        // (BUG-045). A launch link is the only record that the request ever
        // existed, so a log line that overstates the outcome destroys the
        // evidence along with the payload.
        match launch_delivery(&action) {
            Some(LaunchDelivery::Immediately) => {
                tracing::info!(
                    event = "deep_link_cold_start_recovered",
                    action = %action,
                    "recovered a launch deep link; queued for the frontend"
                );
                recovered.immediately.push(url.to_string());
            }
            Some(LaunchDelivery::AfterWorkspaceApply) => {
                tracing::info!(
                    event = "deep_link_cold_start_deferred",
                    action = %action,
                    "recovered a launch deep link; holding it until the \
                     workspace relay is applied"
                );
                recovered.after_workspace_apply.push(url.to_string());
            }
            Some(LaunchDelivery::Unsupported) => {
                tracing::warn!(
                    event = "deep_link_cold_start_dropped",
                    action = %action,
                    "[GUARDRAIL] dropping a launch deep link: this action is \
                     delivered by event only, has no queue the frontend can \
                     drain later, and nothing is listening yet"
                );
            }
            None => {
                tracing::warn!(
                    event = "deep_link_cold_start_unknown_action",
                    action = %action,
                    "[GUARDRAIL] dropping a launch deep link with no launch \
                     disposition; add it to LAUNCH_DELIVERY"
                );
            }
        }
    }
    recovered
}

/// The outcome of [`recover_cold_start_urls`]: what to dispatch now, and what
/// to hold. Anything refused appears in neither — deliberately, so a caller
/// cannot dispatch it by forgetting to check a flag.
#[derive(Debug, Default, PartialEq, Eq)]
struct RecoveredLaunchLinks {
    immediately: Vec<String>,
    after_workspace_apply: Vec<String>,
}

/// Read the plugin's launch URLs once, dispatch what is safe now, stash the
/// rest. The production routing, with only the dispatcher injected, so a test
/// drives this exact function rather than a re-implementation of it.
fn route_cold_start_links<E: std::fmt::Display>(
    pending: &PendingLaunchDeepLinks,
    current: Result<Option<Vec<Url>>, E>,
    mut dispatch: impl FnMut(&str),
) {
    let recovered = recover_cold_start_urls(&pending.cold_start_recovered, current);
    // Stash before dispatching: `dispatch` re-enters `handle_deep_link_url`,
    // and leaving the stash unwritten across that call would let anything it
    // triggers observe an empty queue and conclude there is nothing pending.
    pending.stash(recovered.after_workspace_apply);
    for url in &recovered.immediately {
        dispatch(url);
    }
}

/// Hand every stashed launch link to `dispatch`, exactly once each.
fn drain_launch_links(pending: &PendingLaunchDeepLinks, mut dispatch: impl FnMut(&str)) {
    for url in pending.drain() {
        tracing::info!(
            event = "deep_link_launch_dispatched",
            action = deep_link_action(&url),
            "dispatching a held launch deep link now that the workspace is applied"
        );
        dispatch(&url);
    }
}

/// Replay a `buzz://` link that arrived on the command line before anything was
/// listening (BUG-032).
///
/// # The gap
///
/// `tauri-plugin-deep-link` scans `std::env::args()` inside its *own* plugin
/// `setup` (2.4.9 `src/lib.rs`:73-84), which Tauri runs during
/// `Builder::build()`. The app's `setup` runs later, at `Ready`. And
/// `on_open_url` is a plain `listen` with no replay (ibid. :515-527). So on a
/// cold start the URL is emitted with no listener attached and is simply lost —
/// the app launches and then does nothing at all. The argv loop BUG-030 removed
/// never covered this: it only ran for a *second* instance.
///
/// # What `get_current` actually is
///
/// On desktop it is a getter over `DeepLink::current`, a
/// `Mutex<Option<Vec<Url>>>` created empty in the plugin's `setup` and written
/// only by `handle_cli_arguments` (the argv scan, and the argv the
/// single-instance plugin forwards) and, on macOS, by `RunEvent::Opened`. That
/// means:
/// - it is **per-process memory** — nothing is persisted to disk or registry,
///   so it can never hand back a link from a *previous* launch;
/// - it returns `Ok(None)` when the app was launched normally;
/// - it is **never cleared**, so it returns the same URL on every call for the
///   lifetime of the process.
///
/// That last property is the danger. Loss is the original bug; *replay* would
/// be worse — a second read minutes later would restart an agent nobody asked
/// to restart, and the dedup window is five seconds, far too short to catch it.
/// So this runs at most once per process, latched, and a second attempt is
/// refused and logged rather than served.
///
/// # What this call does *not* do
///
/// It does not dispatch everything it recovers. Recovering a link is cheap;
/// acting on one at `Ready` is not, because `setup` runs before the workspace
/// relay override exists. [`LaunchDelivery`] carries that reasoning per action;
/// [`dispatch_launch_deep_links`] is the other half.
///
/// Dispatch goes through [`handle_deep_link_url`] like every other path, so
/// BUG-030's dedup gate still applies: if the plugin also delivers the same URL
/// by another route, the second delivery is suppressed and logged, not acted on
/// twice. `restart-agent` links carry a single-use `token`, so an identical URL
/// is provably the same request.
#[cfg(desktop)]
pub(crate) fn recover_cold_start_deep_link(app: &tauri::AppHandle) {
    use tauri_plugin_deep_link::DeepLinkExt;

    let current = app.deep_link().get_current();
    let pending = app.state::<PendingLaunchDeepLinks>();
    route_cold_start_links(&pending, current, |url| handle_deep_link_url(app, url));
}

/// Deliver the launch deep links [`recover_cold_start_deep_link`] held back.
///
/// Called from `commands::workspace::apply_workspace`, after the
/// `managed_agent_restore_pending` latch is swapped *and* after launch-time
/// agent restore has finished awaiting — the same ordering every other piece of
/// launch-time agent work already respects, and for the same two reasons:
///
/// 1. `state.relay_url_override` is set earlier in `apply_workspace`, so by
///    here a `restart-agent` link resolves the workspace's real relay instead
///    of the `ws://localhost:3000` fallback (BUG-044).
/// 2. `start_managed_agent` awaits `ensure_relay_mesh_for_record` *before*
///    taking `managed_agents_store_lock`. Dispatching while restore's snapshot
///    phase is still in flight could land in that await and produce two live
///    harnesses for one nsec under two different runtime keys — which neither
///    the runtime registry check nor BUG-030's URL dedup would catch, because
///    the keys differ. Waiting for restore to finish removes the overlap
///    entirely rather than adding a guard for it.
///
/// Idempotent: the queue is drained, so the second and later `apply_workspace`
/// calls (every workspace switch is one) find nothing and do nothing.
pub(crate) fn dispatch_launch_deep_links(app: &tauri::AppHandle) {
    let pending = app.state::<PendingLaunchDeepLinks>();
    drain_launch_links(&pending, |url| handle_deep_link_url(app, url));
}

#[cfg(test)]
#[path = "deep_link_launch_tests.rs"]
mod tests;
