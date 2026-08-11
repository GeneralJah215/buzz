//! Load-time clamp of the legacy agent `parallelism` default (BUG-064).
//! Split from `storage.rs` (file-size cap).

use crate::managed_agents::{
    ManagedAgentRecord, DEFAULT_AGENT_PARALLELISM, LEGACY_DEFAULT_AGENT_PARALLELISM,
};

/// Clamp legacy `parallelism: 10` records down to the new default (BUG-064).
///
/// Every record written before the fix carries exactly
/// [`LEGACY_DEFAULT_AGENT_PARALLELISM`], because that was the serde default
/// nobody ever changed. That value — and only that value — is rewritten to
/// [`DEFAULT_AGENT_PARALLELISM`]. Any other number is a deliberate operator
/// choice and is left exactly as written, including numbers outside the
/// harness's 1..=32 range: range validation belongs at the mint/edit boundary
/// (`resolve_mint_behavioral_defaults`), and silently "fixing" junk here would
/// destroy the evidence of a bad hand edit. The cost of the ambiguity is a
/// deliberate `10` being clamped to `2`, which is recoverable in the UI; the
/// cost of not migrating is the process explosion this bug is about.
///
/// This runs in memory at every load, from the single
/// [`super::load_agent_store`] chokepoint, so both keyed instances and key-less
/// definitions are covered. Nothing is written here: the next ordinary save
/// persists the clamped value, matching the opportunistic-rewrite pattern
/// [`super::hydrate_keys`] already uses. Re-running on an already-migrated
/// store is a no-op.
///
/// Two fields carry a pool size and both are clamped by the same rule.
/// `parallelism` is what this instance spawns with; `definition_parallelism`
/// is what a *definition* advertises to future mints, so leaving it at 10
/// would re-seed the bug on the next agent minted from that definition.
/// `None` there means "advertises nothing" and stays `None`.
///
/// Returns the number of RECORDS migrated, not fields: a record whose two
/// values were both legacy counts once, so the log line ("clamped N agent
/// record(s)") stays literally true and the number matches what an operator
/// would count in the store file.
pub(crate) fn migrate_legacy_parallelism(records: &mut [ManagedAgentRecord]) -> usize {
    let mut migrated = 0;
    for record in records.iter_mut() {
        let mut touched = false;
        if record.parallelism == LEGACY_DEFAULT_AGENT_PARALLELISM {
            record.parallelism = DEFAULT_AGENT_PARALLELISM;
            touched = true;
        }
        if record.definition_parallelism == Some(LEGACY_DEFAULT_AGENT_PARALLELISM) {
            record.definition_parallelism = Some(DEFAULT_AGENT_PARALLELISM);
            touched = true;
        }
        if touched {
            migrated += 1;
        }
    }

    if migrated > 0 {
        eprintln!(
            "buzz-desktop: BUG-064 — clamped {migrated} agent record(s) from the legacy \
             parallelism default {LEGACY_DEFAULT_AGENT_PARALLELISM} to \
             {DEFAULT_AGENT_PARALLELISM}; the next save persists it"
        );
    }

    migrated
}
