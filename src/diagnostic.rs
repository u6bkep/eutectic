//! Structured diagnostics: the project-wide error + warning vocabulary.
//!
//! The stability goal (see docs/architecture.md, "Error handling") is two layers.
//! The command algebra guarantees *no invalid states*: a transaction either commits
//! a valid document or aborts whole, never half-applies. This module is the second
//! layer — *how the reasons surface*: every fallible operation yields structured
//! [`Diagnostic`]s instead of panicking or returning flat strings.
//!
//! **One vocabulary, two channels.**
//!   - **Hard faults** — a transaction that cannot build a valid model returns
//!     `Err(Vec<Diagnostic>)` and aborts atomically. *Collect-all*: elaboration
//!     reports as many independent faults as it can find in one pass (rustc-style),
//!     suppressing only the cascade from a poisoned entity.
//!   - **Findings on a valid model** — reconciliation, ERC, DRC, and the
//!     floating-pad check attach diagnostics to a document that *did* elaborate.
//!
//! **Severity is seriousness; the channel decides blocking.** A pin-vs-constraint
//! conflict is [`Severity::Error`] (it is genuinely wrong) yet lives in the findings
//! channel on a valid doc — it is surfaced loudly and kept until resolved, not used
//! to abort the commit that produced it. "Can this tape out?" = "are there any
//! `Error`-severity diagnostics across either channel?".
//!
//! **Structured internally, rendered at the edge.** A `Diagnostic` carries *facts*
//! (which entity/net/pin, candidate names) plus a stable `code`, never pre-formatted
//! prose. [`render`] turns a slice into rustc-style text for the CLI / agent surface;
//! a GUI instead reads [`Location`] to highlight the offending entity. Domain types
//! callers consume as data (e.g. [`crate::route::Violation`]) stay typed and
//! implement [`Diagnose`] for *rendering* — `Diagnostic` is the presentation lingua
//! franca, not a replacement for them.

use crate::doc::PinRef;
use crate::id::{EntityId, NetId, TraceId, ViaId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Genuinely wrong. In the hard-fault channel it aborts the transaction; in the
    /// findings channel it is a must-fix issue on an otherwise-valid document.
    /// Ordered first so errors sort before warnings.
    Error,
    /// Advisory: the document is valid and this is worth surfacing (e.g. a decayed
    /// hint, a redundant pin).
    Warning,
}

/// Where a diagnostic points. **Semantic-first**: most issues locate by *model
/// identity* (an entity / net / pin / trace / via), which is exactly what a GUI
/// highlights and what an agent editing the model acts on. `Span` is the textual
/// location only the text front-end can supply.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Location {
    Entity(EntityId),
    Net(NetId),
    Pin(PinRef),
    Trace(TraceId),
    Via(ViaId),
    /// Line/column in a parsed text document (1-based). Populated by the text
    /// front-end; column is best-effort (`1` when only the line is known).
    Span { line: u32, col: u32 },
    /// No specific location (a whole-document or configuration-level issue).
    None,
}

/// A single structured diagnostic: severity + a stable machine-greppable `code` +
/// a human `message` + a [`Location`] + optional `help`. Construct via [`error`] /
/// [`warning`] and the [`with_help`] builder.
///
/// [`error`]: Diagnostic::error
/// [`warning`]: Diagnostic::warning
/// [`with_help`]: Diagnostic::with_help
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable, closed-set code (e.g. `"E_UNKNOWN_PIN"`, `"W_HINT_DECAYED"`). The
    /// agent/CLI parse anchor: a reader greps the code rather than the prose, so
    /// messages can be reworded without breaking tooling.
    pub code: &'static str,
    pub message: String,
    pub location: Location,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, location: Location) -> Diagnostic {
        Diagnostic { severity: Severity::Error, code, message: message.into(), location, help: None }
    }
    pub fn warning(code: &'static str, message: impl Into<String>, location: Location) -> Diagnostic {
        Diagnostic { severity: Severity::Warning, code, message: message.into(), location, help: None }
    }
    /// Attach a help line (e.g. `"available pins: VDD, GND"`).
    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Render a value's issues into the shared diagnostic vocabulary. Implemented by
/// the typed domain results (e.g. [`crate::route::Violation`],
/// [`crate::doc::ReconReport`]) so they stay usable as data while sharing one text
/// rendering. The returned diagnostics are *not* pre-sorted; pass them to [`render`].
pub trait Diagnose {
    fn diagnostics(&self) -> Vec<Diagnostic>;
}

/// Does this set contain any `Error`-severity diagnostic? (The "is it clean?" test.)
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(Diagnostic::is_error)
}

/// Render diagnostics as deterministic, rustc-style text: errors before warnings,
/// then by code/message/location (the derived `Ord`), so output is byte-stable and
/// diffable — a human reads errors first, an agent can rely on the ordering and the
/// codes. Returns an empty string for no diagnostics.
pub fn render(diags: &[Diagnostic]) -> String {
    let mut sorted: Vec<&Diagnostic> = diags.iter().collect();
    sorted.sort();
    let mut out = String::new();
    for d in sorted {
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        out.push_str(&format!("{sev}[{}]: {}", d.code, d.message));
        if !matches!(d.location, Location::None) {
            out.push_str(&format!("\n  --> {}", render_location(&d.location)));
        }
        if let Some(h) = &d.help {
            out.push_str(&format!("\n  help: {h}"));
        }
        out.push('\n');
    }
    out
}

fn render_location(loc: &Location) -> String {
    match loc {
        Location::Entity(e) => format!("{e}"),
        Location::Net(n) => format!("net {n}"),
        Location::Pin(p) => format!("{}.{}", p.comp, p.pin),
        Location::Trace(t) => format!("trace {t}"),
        Location::Via(v) => format!("via {v}"),
        Location::Span { line, col } => format!("{line}:{col}"),
        Location::None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_sorted_errors_first_and_stable() {
        let diags = vec![
            Diagnostic::warning("W_B", "second warning", Location::None),
            Diagnostic::error("E_A", "an error", Location::Net(NetId::new("GND")))
                .with_help("try connecting it"),
            Diagnostic::warning("W_A", "first warning", Location::None),
        ];
        let out = render(&diags);
        let lines: Vec<&str> = out.lines().filter(|l| !l.starts_with("  ")).collect();
        // Error sorts before both warnings; warnings sort by code (W_A < W_B).
        assert_eq!(lines[0], "error[E_A]: an error");
        assert_eq!(lines[1], "warning[W_A]: first warning");
        assert_eq!(lines[2], "warning[W_B]: second warning");
        // Location + help render as indented follow-on lines.
        assert!(out.contains("\n  --> net GND"));
        assert!(out.contains("\n  help: try connecting it"));
        // Deterministic: same input, same output.
        assert_eq!(render(&diags), out);
    }

    #[test]
    fn has_errors_detects_severity() {
        assert!(has_errors(&[Diagnostic::error("E", "x", Location::None)]));
        assert!(!has_errors(&[Diagnostic::warning("W", "x", Location::None)]));
        assert!(!has_errors(&[]));
    }
}
