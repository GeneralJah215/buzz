//! Tests for `deep_link_launch.rs`, split out to keep that file under the
//! repo's file-size ratchet.

use url::Url;

use super::{
    drain_launch_links, launch_delivery, route_cold_start_links, LaunchDelivery,
    PendingLaunchDeepLinks, LAUNCH_DELIVERY,
};

/// A `buzz://restart-agent` link exactly as `agent-watchdog.ps1` fires it: one
/// pubkey, one single-use control token, and — this is the shape that made
/// BUG-044 bite — **no `relay` param**, verified against both copies of the
/// watchdog. That absence is what sends the request to
/// `effective_agent_relay_url`, and from there to whatever
/// `state.relay_url_override` holds at that instant.
fn restart_url(token: &str) -> String {
    const AGENT_PUBKEY: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    format!("buzz://restart-agent?pubkey={AGENT_PUBKEY}&token={token}")
}

// ---------------------------------------------------------------------------
// Source-shape helpers.
//
// Three of the assertions below are about *wiring*: which file calls what, in
// what order. Nothing in a unit test can observe that, because the functions
// involved take a `tauri::AppHandle<Wry>` that only a real app can produce. So
// these read the source — but they read the source with comments stripped and
// with the surrounding block resolved, so "the call exists" cannot be satisfied
// by a mention in a comment, by a call in the wrong `#[cfg]` block, or by a
// call placed before the thing it must follow. Each is labelled a wiring check,
// not a behaviour check; the behaviour is covered by the tests above them.
// ---------------------------------------------------------------------------

/// Read a crate source file, with `//` line comments blanked out.
///
/// Naive about `//` inside string literals — deliberately. The failure it can
/// cause is dropping real code from the search, which turns a "must appear
/// once" assertion into a loud failure, never into a silent pass.
fn code_of(relative_path: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(relative_path);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every non-test `.rs` file under `src/`, as
/// `(relative path, comment-stripped code)`.
///
/// Test modules are excluded because they name production functions in string
/// literals — including the assertion below. A test cannot call
/// `handle_deep_link_url` for real anyway: it needs an `AppHandle<Wry>`, which
/// only a running app produces, so excluding them hides no live call site.
fn crate_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("cannot read source directory") {
            let path = entry.expect("cannot stat source entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path
                    .file_stem()
                    .is_some_and(|stem| stem.to_string_lossy().ends_with("_tests"))
            {
                let relative = path
                    .strip_prefix(root)
                    .expect("source outside src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative.clone(), code_of(&relative)));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

/// Byte offset of `needle`'s single occurrence in `code`, panicking with the
/// real count when the assumption of uniqueness is wrong.
fn only_offset(code: &str, needle: &str, label: &str) -> usize {
    let count = code.matches(needle).count();
    assert_eq!(count, 1, "expected exactly one {label}, found {count}");
    code.find(needle).expect("counted one, found none")
}

/// Byte range of the brace-delimited block that opens at or after `from`.
fn block_after(code: &str, from: usize) -> std::ops::Range<usize> {
    let open = from + code[from..].find('{').expect("no block after marker");
    let mut depth = 0usize;
    for (offset, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open..open + offset;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from offset {open}");
}

#[test]
fn every_deep_link_dispatch_call_site_is_a_declared_one() {
    // BUG-030 was two registrations calling `handle_deep_link_url` for one OS
    // activation, and it survived the BUG-009 and BUG-015 investigations
    // unnoticed because nothing asserted the count. The previous version of
    // this test counted `lib.rs` alone while stating the invariant crate-wide,
    // so it stayed green once a second call site appeared outside that file.
    // This walks every non-test `.rs` file under `src/` instead, and names the
    // reason each declared site exists.
    //
    // If this fails because a delivery path was added on purpose: the dedup
    // gate will keep it correct, but read the BUG-030 notes in `lib.rs` first —
    // on Windows the single-instance plugin already feeds argv to the deep-link
    // plugin for you.
    let expected: &[(&str, usize)] = &[
        // The `on_open_url` registration: the one warm-open path.
        ("lib.rs", 1),
        // The definition itself.
        ("deep_link.rs", 1),
        // The two launch dispatchers: `recover_cold_start_deep_link` for what
        // is safe at `setup`, `dispatch_launch_deep_links` for what is not.
        ("deep_link_launch.rs", 2),
    ];
    let mut unexpected = Vec::new();
    for (path, code) in crate_sources() {
        let count = code.matches("handle_deep_link_url(").count();
        let allowed = expected
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, count)| *count)
            .unwrap_or(0);
        if count != allowed {
            unexpected.push(format!("{path}: {count} (expected {allowed})"));
        }
    }
    assert!(
        unexpected.is_empty(),
        "handle_deep_link_url call sites moved: {unexpected:?}"
    );
}

// ---------------------------------------------------------------------------
// BUG-032 / BUG-044 / BUG-045: a cold start must not swallow the link that
// launched the app, must not replay it, must not act on it before the
// workspace exists, and must not claim to have delivered what it dropped.
// ---------------------------------------------------------------------------

/// What `DeepLink::get_current()` hands back on desktop: `Ok(None)` for a
/// normal launch, `Ok(Some(urls))` when argv carried a `buzz://` link.
fn plugin_current(urls: Option<&[&str]>) -> Result<Option<Vec<Url>>, String> {
    Ok(urls.map(|list| list.iter().map(|raw| Url::parse(raw).unwrap()).collect()))
}

/// Run the real launch routing over `urls`, returning what it dispatched
/// immediately and what it held back for `apply_workspace`.
///
/// This drives `route_cold_start_links` — the function `recover_cold_start_deep_link`
/// itself calls, with only the `handle_deep_link_url` closure swapped for a
/// recorder — rather than a local re-implementation of it.
fn route(pending: &PendingLaunchDeepLinks, urls: Option<&[&str]>) -> (Vec<String>, Vec<String>) {
    let mut dispatched = Vec::new();
    route_cold_start_links(pending, plugin_current(urls), |url| {
        dispatched.push(url.to_owned())
    });
    let mut held = Vec::new();
    drain_launch_links(pending, |url| held.push(url.to_owned()));
    (dispatched, held)
}

#[test]
fn a_cold_start_holds_a_restart_agent_link_until_the_workspace_is_applied() {
    // BUG-044, the whole of it. `setup` runs at `Ready`; `apply_workspace` sets
    // `state.relay_url_override` and needs React to mount, hundreds of
    // milliseconds to seconds later. A `restart-agent` link with no `relay`
    // param resolves its relay from the (empty, on a cold start) runtime
    // registry and then from that override — so dispatching at `setup` starts
    // the agent on `ws://localhost:3000`, invisible on the real relay, and
    // `restore.rs` then suppresses the correct restore for the whole session.
    let url = restart_url("one-shot-token");
    let (dispatched, held) = route(&PendingLaunchDeepLinks::default(), Some(&[&url]));
    assert!(
        dispatched.is_empty(),
        "a restart-agent link must not be dispatched at setup, got {dispatched:?}"
    );
    assert_eq!(
        held,
        vec![url],
        "a restart-agent link must be held for apply_workspace"
    );
}

#[test]
fn a_cold_start_dispatches_community_links_straight_away() {
    // These arms park their payload in `PendingCommunityDeepLinks`, which the
    // frontend drains when it mounts, so they do not need anyone listening now
    // — and holding them until `apply_workspace` would lose them outright
    // during onboarding, where no community exists and `apply_workspace` is
    // never called. An invite link on a fresh install is exactly that case.
    let links = [
        "buzz://connect?relay=wss://relay.example/",
        "buzz://join?relay=wss://relay.example/&code=abc123",
        "buzz://add-community?relay=wss://relay.example/&name=Example",
    ];
    for link in links {
        let (dispatched, held) = route(&PendingLaunchDeepLinks::default(), Some(&[link]));
        assert_eq!(dispatched, vec![link.to_owned()], "{link} must dispatch now");
        assert!(held.is_empty(), "{link} must not be held back");
    }
}

#[test]
fn a_cold_start_refuses_the_actions_that_have_no_backstop() {
    // BUG-045. `message` and `nostr-bind` only `app.emit(...)`: no
    // `queue_community_deep_link`, and the frontend registers their listeners
    // with a plain `listen` after React mounts. A launch-time emit reaches
    // nobody. Recovery therefore drops them *and says so* — the alternative
    // that was shipped, emitting into the void and then logging
    // `deep_link_cold_start_recovered`, destroys the evidence along with the
    // payload and burns the one-shot latch on a delivery that never happened.
    let links = [
        "buzz://message?channel=c1&id=e1",
        "buzz://nostr-bind?challenge_id=c&nonce=n",
    ];
    for link in links {
        let (dispatched, held) = route(&PendingLaunchDeepLinks::default(), Some(&[link]));
        assert!(dispatched.is_empty(), "{link} must not be dispatched");
        assert!(held.is_empty(), "{link} must not be held either");
    }
}

#[test]
fn a_cold_start_refuses_an_action_it_has_no_disposition_for() {
    // A new arm added to `handle_deep_link_url` without a `LAUNCH_DELIVERY`
    // row is unknown here. Refusing is the safe default: the cost is a dropped
    // launch link with a WARN naming the action, not a dispatch into a state
    // nobody decided was ready for it.
    let (dispatched, held) = route(
        &PendingLaunchDeepLinks::default(),
        Some(&["buzz://not-an-action?x=1"]),
    );
    assert!(dispatched.is_empty());
    assert!(held.is_empty());
}

#[test]
fn a_cold_start_without_a_link_does_nothing() {
    // The overwhelmingly common launch. Recovery must be inert here: inventing
    // a dispatch from an empty `current` would restart agents on every boot.
    for current in [None, Some(&[] as &[&str])] {
        let (dispatched, held) = route(&PendingLaunchDeepLinks::default(), current);
        assert!(dispatched.is_empty(), "a normal launch must dispatch nothing");
        assert!(held.is_empty(), "a normal launch must hold nothing");
    }
}

#[test]
fn a_held_link_is_dispatched_once_and_never_again() {
    // `apply_workspace` runs on every workspace switch, not only at launch, so
    // the hold has to be a drain and not a peek: a second call must find the
    // queue empty rather than fire the restart again, minutes or hours later
    // and far outside the 5 s dedup window.
    let pending = PendingLaunchDeepLinks::default();
    let url = restart_url("one-shot-token");
    let (_, held) = route(&pending, Some(&[&url]));
    assert_eq!(held, vec![url]);

    let mut second = Vec::new();
    drain_launch_links(&pending, |url| second.push(url.to_owned()));
    assert!(
        second.is_empty(),
        "a second apply_workspace must not replay the launch link, got {second:?}"
    );
}

#[test]
fn the_launch_link_is_never_read_out_of_the_plugin_twice() {
    // `get_current` is a getter over a field the plugin never clears, so it
    // keeps returning the launch URL for the life of the process. Replay is
    // worse than the original bug: a second read minutes later restarts an
    // agent nobody asked about, long past the 5 s dedup window.
    let pending = PendingLaunchDeepLinks::default();
    let url = restart_url("one-shot-token");
    assert_eq!(route(&pending, Some(&[&url])).1.len(), 1);

    let (dispatched, held) = route(&pending, Some(&[&url]));
    assert!(
        dispatched.is_empty() && held.is_empty(),
        "the same launch URL must never be recovered twice"
    );
}

#[test]
fn a_recovered_url_is_byte_identical_to_the_plugin_delivery_of_the_same_link() {
    // The dedup gate that makes recovery safe keys on the whole URL string, so
    // recovery and the plugin's own `on_open_url` delivery only collapse into
    // one action if both produce the same bytes. Recovery goes through
    // `Url::to_string`; the plugin hands `on_open_url` the same `Url`.
    let pending = PendingLaunchDeepLinks::default();
    let url = restart_url("one-shot-token");
    let (_, held) = route(&pending, Some(&[&url]));
    assert_eq!(
        held.first().map(String::as_str),
        Some(Url::parse(&url).unwrap().as_str()),
        "recovery must not renormalise the URL away from the plugin's form"
    );
}

#[test]
fn a_failed_recovery_yields_nothing_rather_than_a_bogus_link() {
    let pending = PendingLaunchDeepLinks::default();
    let failure: Result<Option<Vec<Url>>, String> = Err("plugin unavailable".to_owned());
    let mut dispatched = Vec::new();
    route_cold_start_links(&pending, failure, |url| dispatched.push(url.to_owned()));
    assert!(
        dispatched.is_empty(),
        "a recovery failure must be logged, not turned into a dispatch"
    );
    let mut held = Vec::new();
    drain_launch_links(&pending, |url| held.push(url.to_owned()));
    assert!(held.is_empty());
}

#[test]
fn every_handled_action_has_a_launch_disposition() {
    // `LAUNCH_DELIVERY` and the `match url.host_str()` in `handle_deep_link_url`
    // are two halves of one decision. An arm added to either alone is how
    // BUG-045 happened: `message` and `nostr-bind` were treated as recoverable
    // because nothing forced anyone to decide whether they were.
    let deep_link = code_of("deep_link.rs");
    let body = block_after(
        &deep_link,
        only_offset(
            &deep_link,
            "fn handle_deep_link_url(",
            "handle_deep_link_url definition",
        ),
    );
    let mut handled: Vec<&str> = Vec::new();
    let mut rest = &deep_link[body];
    while let Some(start) = rest.find("Some(\"") {
        rest = &rest[start + 6..];
        let end = rest.find('"').expect("unterminated action literal");
        handled.push(&rest[..end]);
        rest = &rest[end..];
    }
    assert!(
        handled.len() >= 6,
        "expected to find the deep-link action arms, found {handled:?}"
    );
    let mut classified: Vec<&str> = LAUNCH_DELIVERY.iter().map(|(name, _)| *name).collect();
    handled.sort_unstable();
    classified.sort_unstable();
    assert_eq!(
        handled, classified,
        "LAUNCH_DELIVERY and handle_deep_link_url disagree about which actions exist"
    );
}

#[test]
fn the_only_actions_dispatched_at_setup_are_the_ones_with_a_frontend_queue() {
    // The safety property behind `LaunchDelivery::Immediately`: an arm may only
    // be dispatched before the frontend exists if it parks its payload in
    // `PendingCommunityDeepLinks`, which the frontend drains on mount. Read off
    // the arms themselves, so relabelling an arm `Immediately` without giving
    // it a queue fails here rather than at the next cold start.
    let deep_link = code_of("deep_link.rs");
    let body = block_after(
        &deep_link,
        only_offset(
            &deep_link,
            "fn handle_deep_link_url(",
            "handle_deep_link_url definition",
        ),
    );
    let body = &deep_link[body];
    for (action, delivery) in LAUNCH_DELIVERY {
        if *delivery != LaunchDelivery::Immediately {
            continue;
        }
        let arm = block_after(
            body,
            only_offset(body, &format!("Some(\"{action}\")"), "action arm"),
        );
        assert!(
            body[arm].contains("queue_community_deep_link("),
            "`{action}` is dispatched at setup but never queues for the frontend"
        );
    }
}

#[test]
fn launch_delivery_is_unknown_for_an_unlisted_action() {
    assert_eq!(launch_delivery("connect"), Some(LaunchDelivery::Immediately));
    assert_eq!(
        launch_delivery("restart-agent"),
        Some(LaunchDelivery::AfterWorkspaceApply)
    );
    assert_eq!(launch_delivery("message"), Some(LaunchDelivery::Unsupported));
    assert_eq!(launch_delivery("connect "), None);
    assert_eq!(launch_delivery(""), None);
}

#[test]
fn wiring_setup_recovers_the_launch_link_after_registering_the_warm_path() {
    // Wiring check, not behaviour. The routing above is inert unless `setup`
    // calls it, and the recovery must sit *after* `on_open_url` — a link the
    // plugin re-delivers while recovery is mid-flight would otherwise find no
    // listener — and inside the same `#[cfg(desktop)]` block, since neither
    // `get_current` nor the plugin extension exists elsewhere.
    let lib_rs = code_of("lib.rs");
    let registration = only_offset(&lib_rs, "on_open_url(", "on_open_url registration");
    let recovery = only_offset(
        &lib_rs,
        "recover_cold_start_deep_link(",
        "cold-start recovery call site",
    );
    assert!(
        recovery > registration,
        "recovery must run after the on_open_url registration"
    );
    let cfg = lib_rs[..registration]
        .rfind("#[cfg(desktop)]")
        .expect("the deep-link registration must live under #[cfg(desktop)]");
    let block = block_after(&lib_rs, cfg);
    assert!(
        block.contains(&recovery),
        "the recovery call must sit inside that same #[cfg(desktop)] block"
    );
}

#[test]
fn wiring_apply_workspace_dispatches_held_links_after_the_relay_and_restore() {
    // Wiring check, not behaviour, and the load-bearing half of BUG-044. The
    // dispatch must follow the relay override (or the agent starts on the
    // localhost fallback) and must follow the awaited launch restore (or the
    // deep-link start path can interleave with restore's snapshot and produce
    // two harnesses for one nsec under two runtime keys, which neither the
    // runtime registry check nor the URL dedup catches).
    let workspace = code_of("commands/workspace.rs");
    let dispatch = only_offset(
        &workspace,
        "dispatch_launch_deep_links(",
        "launch deep-link dispatch site",
    );
    for (marker, why) in [
        ("relay_url_override", "the workspace relay override"),
        (
            "managed_agent_restore_pending",
            "the restore-pending latch swap",
        ),
        (
            "restore_managed_agents_on_launch(",
            "the awaited launch-time agent restore",
        ),
    ] {
        let last = workspace
            .rfind(marker)
            .unwrap_or_else(|| panic!("{marker} is gone from apply_workspace"));
        assert!(
            dispatch > last,
            "launch deep links must be dispatched after {why}"
        );
    }
}
