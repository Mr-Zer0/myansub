//! The cost-confirmation gate (`docs/CLI.md` §5, `docs/LLM.md` §7/§9).
//!
//! Commands that spend tokens print a pre-flight estimate and ask for confirmation
//! when the estimate exceeds `confirm_above_usd` (config), unless `--yes`
//! (`docs/CLI.md` bottom of §6). The branch is factored into the **pure**
//! [`confirm_decision`] — mirroring [`decide_reinit`](super::input::decide_reinit) —
//! so it is testable without a terminal, and the interactive read runs on the
//! blocking pool ([`tokio::task::spawn_blocking`]) so it never stalls an async
//! runtime worker (`docs/ARCHITECTURE.md` §6 rule 6, consistent with
//! `pipeline::input`'s destructive-reinit prompt and the review editor).
//!
//! Non-interactive safety (`docs/CLI.md` §5): a confirmation that would be shown
//! when stdin is not a TTY and `--yes` was not given is **never** blocked on; it is
//! refused with [`PipelineError::ConfirmationNeeded`] (exit 3) naming `--yes`.

use std::future::Future;

use crate::llm::pricing::Cost;

use super::error::PipelineError;

/// Settings for the cost-confirmation gate, resolved by the command handler from
/// config (`confirm_above_usd`), the terminal (`interactive`), and the global
/// `--yes` flag (`docs/CLI.md` §1/§5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfirmSettings {
    /// The configured threshold in USD (`[llm].confirm_above_usd`; `0` = always ask).
    pub confirm_above_usd: f64,
    /// Whether stdin is a TTY (`std::io::stdin().is_terminal()`).
    pub interactive: bool,
    /// The global `--yes` flag.
    pub yes: bool,
}

/// The pre-flight token/cost estimate shown at the gate (`docs/LLM.md` §9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cost: Cost,
}

/// The decision the cost-confirmation gate takes (`docs/CLI.md` §5). Pure, so it is
/// testable without a terminal (mirrors [`ReinitAction`](super::input::ReinitAction)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Below the threshold, or `--yes`: run without asking.
    Proceed,
    /// Above the threshold and stdin is a TTY: prompt the user.
    Prompt,
    /// Above the threshold but stdin is not a TTY and `--yes` was not given: refuse,
    /// naming `--yes` (exit 3).
    RefuseNeedsYes,
}

/// Decides whether a token-spending command must confirm its estimated cost
/// (`docs/CLI.md` §5, `docs/LLM.md` §9).
///
/// - `--yes` always proceeds.
/// - An estimate that **exceeds** `confirm_above_usd` needs confirmation; a
///   threshold of `0` (or below) always asks (`docs/CONFIG.md`: "0 = always ask").
/// - An **unknown** (unpriced) estimate is treated as needing confirmation — the
///   safe choice, so an unpriced run is never spent silently.
/// - When confirmation is needed: a TTY prompts, a non-TTY refuses (naming `--yes`),
///   never blocking on input (`docs/CLI.md` §5).
pub fn confirm_decision(
    estimated_cost: Cost,
    confirm_above_usd: f64,
    interactive: bool,
    yes: bool,
) -> ConfirmAction {
    if yes {
        return ConfirmAction::Proceed;
    }
    let needs_confirmation = match estimated_cost {
        // An unpriced estimate is treated as needing confirmation (the safe choice):
        // never spend an unknown amount without an explicit go-ahead.
        Cost::Unknown => true,
        Cost::Known(usd) => confirm_above_usd <= 0.0 || usd > confirm_above_usd,
    };
    if !needs_confirmation {
        return ConfirmAction::Proceed;
    }
    if interactive {
        ConfirmAction::Prompt
    } else {
        ConfirmAction::RefuseNeedsYes
    }
}

/// The outcome of a confirmed run (`docs/CLI.md` §5): the work ran (with its own
/// outcome `O`), or the user declined the interactive prompt so nothing was spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmedRun<O> {
    /// The gate was cleared and the work ran.
    Ran(O),
    /// The user declined the interactive prompt; nothing was spent.
    Declined,
}

/// The result of applying the gate when the caller already holds the transport
/// (`docs/CLI.md` §5). Unlike [`gate`], this does not build a transport — it is used
/// at a mid-pipeline boundary (the `run` translation gate) where the transport is
/// already in hand. A non-TTY refusal is surfaced as [`PipelineError::ConfirmationNeeded`]
/// (exit 3); an interactive decline returns [`GateDecision::Declined`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Clear to run.
    Proceed,
    /// The user declined the interactive prompt; the caller must not spend.
    Declined,
}

/// Applies the cost-confirmation gate without building a transport (the caller holds
/// it). Proceeds below the threshold or with `--yes`; refuses (exit 3) on a non-TTY
/// above the threshold; prompts on a TTY (the read runs on the blocking pool). Shared
/// by [`gate`] and the `run` translation gate so both honour `docs/CLI.md` §5.
pub async fn confirm_or_refuse(
    estimate: CostEstimate,
    settings: ConfirmSettings,
) -> Result<GateDecision, PipelineError> {
    match confirm_decision(
        estimate.cost,
        settings.confirm_above_usd,
        settings.interactive,
        settings.yes,
    ) {
        ConfirmAction::Proceed => Ok(GateDecision::Proceed),
        ConfirmAction::RefuseNeedsYes => {
            eprintln!("{}", estimate_line(&estimate));
            Err(PipelineError::ConfirmationNeeded {
                flag: "--yes".to_string(),
            })
        }
        ConfirmAction::Prompt => {
            // Blocking stdin read on the blocking pool (`docs/ARCHITECTURE.md` §6
            // rule 6); a join failure (panic/shutdown) is treated as a decline.
            let confirmed = tokio::task::spawn_blocking(move || prompt_confirmation(&estimate))
                .await
                .unwrap_or(false);
            if confirmed {
                Ok(GateDecision::Proceed)
            } else {
                Ok(GateDecision::Declined)
            }
        }
    }
}

/// Runs `work` behind the cost-confirmation gate (`docs/CLI.md` §5, `docs/LLM.md`
/// §9).
///
/// `make_transport` builds the LLM transport and is invoked **only** once the gate
/// is cleared, so a refused or declined run never requires an API key. On a refusal
/// (non-TTY, no `--yes`) the estimate is printed and
/// [`PipelineError::ConfirmationNeeded`] (exit 3) is returned. On an interactive
/// prompt the y/N read runs on the blocking pool; a decline returns
/// [`ConfirmedRun::Declined`] (nothing spent).
pub async fn gate<T, MK, RUN, FUT, O>(
    estimate: CostEstimate,
    settings: ConfirmSettings,
    make_transport: MK,
    work: RUN,
) -> Result<ConfirmedRun<O>, PipelineError>
where
    MK: FnOnce() -> Result<T, PipelineError>,
    RUN: FnOnce(T) -> FUT,
    FUT: Future<Output = Result<O, PipelineError>>,
{
    match confirm_or_refuse(estimate, settings).await? {
        GateDecision::Proceed => {
            let transport = make_transport()?;
            Ok(ConfirmedRun::Ran(work(transport).await?))
        }
        GateDecision::Declined => Ok(ConfirmedRun::Declined),
    }
}

/// A one-line rendering of the estimate for the prompt and the refusal message.
fn estimate_line(estimate: &CostEstimate) -> String {
    let cost = match estimate.cost {
        Cost::Known(usd) => format!("${usd:.4}"),
        Cost::Unknown => "unknown (no price entry for the model)".to_string(),
    };
    format!(
        "Estimated cost: {cost} (input {} / output {} tokens).",
        estimate.prompt_tokens, estimate.output_tokens
    )
}

/// Prompts on an interactive terminal before spending tokens. Accepts `y`/`yes`
/// (case-insensitive); anything else (including EOF) declines. Mirrors
/// [`confirm_destructive_reinit`](super::input) — prints to stderr, reads stdin.
fn prompt_confirmation(estimate: &CostEstimate) -> bool {
    use std::io::Write;
    eprintln!("{}", estimate_line(estimate));
    eprint!("Proceed? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}
