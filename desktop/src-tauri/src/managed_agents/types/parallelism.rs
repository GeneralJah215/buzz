//! Agent runtime-pool defaults, split from `types.rs` (file-size cap).
//!
//! The desktop hands this number to the harness as `BUZZ_ACP_AGENTS`.

/// Default ceiling on an agent's runtime pool (`BUZZ_ACP_AGENTS`).
///
/// A pool slot serves one channel's turn and then returns to the pool, so the
/// realistic number of *concurrent* consumers is one live conversation plus the
/// heartbeat — two. The harness grows the pool on demand up to this ceiling and
/// reaps idle slots, so this is a ceiling, not a startup cost: an agent that
/// never sees two overlapping turns never pays for the second slot.
///
/// Was 10 before BUG-064. Every record on disk carried that value, the harness
/// eagerly spawned that many agent runtimes, and dozens of managed agents
/// multiplied it into a process explosion. See
/// [`LEGACY_DEFAULT_AGENT_PARALLELISM`] and
/// `managed_agents::storage::migrate_legacy_parallelism` for the load-time
/// clamp that repairs already-written records.
pub const DEFAULT_AGENT_PARALLELISM: u32 = 2;

/// The pre-BUG-064 default. Records written before the fix are pinned at this
/// exact value; the load-time migration rewrites only this value, so any other
/// number is treated as a deliberate operator choice and preserved.
pub const LEGACY_DEFAULT_AGENT_PARALLELISM: u32 = 10;

/// Serde `default` for [`super::ManagedAgentRecord::parallelism`].
pub(crate) fn default_agent_parallelism() -> u32 {
    DEFAULT_AGENT_PARALLELISM
}

/// Clamp a pool size inherited from an agent *definition* (BUG-064).
///
/// Applied only on the definition-inheritance branch of
/// [`super::resolve_mint_behavioral_defaults`], never to a value the operator
/// typed: a definition advertising exactly [`LEGACY_DEFAULT_AGENT_PARALLELISM`]
/// is carrying the pre-fix serde default that nobody chose, whereas an explicit
/// input of 10 is a real choice and is left alone. Same equality-only rule as
/// the load-time migration — the legacy default is the only value we can prove
/// was machine-written.
///
/// Every other value passes through unchanged, including out-of-range junk, so
/// the caller's 1..=32 check still fails loudly instead of being pre-laundered.
pub(crate) fn clamp_legacy_parallelism(advertised: u32) -> u32 {
    if advertised == LEGACY_DEFAULT_AGENT_PARALLELISM {
        DEFAULT_AGENT_PARALLELISM
    } else {
        advertised
    }
}
