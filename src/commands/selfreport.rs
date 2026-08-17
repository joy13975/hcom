//! `hcom epic` / `hcom doing` / `hcom heads-up` — what an agent advertises about itself.
//!
//! Three fields, one mechanism. `epic` is the high-level line of work, `doing`
//! the current local focus within it, and `heads-up` what peers should
//! anticipate so two agents do not thrash the same systems.
//!
//! All three are first-person: they record the CALLER's own state, which other
//! agents then read through `hcom forum` and `hcom list`. Stored as events
//! rather than `instances` columns because the hook lifecycle rewrites
//! `status_context` and `status_detail` on every tool-use tick and would erase
//! agent-authored text.

use crate::db::{DOING_EVENT_TYPE, EPIC_EVENT_TYPE, HEADSUP_EVENT_TYPE, HcomDb};
use crate::identity;
use crate::shared::{CommandContext, SenderKind};

/// Parsed arguments for `hcom epic`.
#[derive(clap::Parser, Debug)]
#[command(name = "epic", about = "Set or show your high-level line of work")]
pub struct EpicArgs {
    /// One-line description of the work. Omit to print it; pass "" to clear it.
    pub text: Vec<String>,
}

/// Parsed arguments for `hcom doing`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "doing",
    about = "Set or show what you are currently working on"
)]
pub struct DoingArgs {
    /// Activity text. Omit to print the current value; pass "" to clear it.
    pub text: Vec<String>,
}

/// Parsed arguments for `hcom heads-up`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "heads-up",
    about = "Set or show what peers should watch out for"
)]
pub struct HeadsUpArgs {
    /// Warning text. Omit to print the current value; pass "" to clear it.
    pub text: Vec<String>,
}

/// Human label and prompt wording for one self-report kind.
///
/// Keyed off the event-type constant so the CLI wording and the stored event
/// type cannot drift apart.
fn kind_labels(kind: &str) -> (&'static str, &'static str) {
    match kind {
        EPIC_EVENT_TYPE => ("epic", "your high-level line of work"),
        DOING_EVENT_TYPE => ("doing", "what you are working on"),
        HEADSUP_EVENT_TYPE => ("heads-up", "what peers should watch out for"),
        other => unreachable!("unknown self-report kind {other}"),
    }
}

/// The CLI verb for one kind. `headsup` is stored unhyphenated but typed with a
/// hyphen, so the "set it with" hint must print the typed form.
fn kind_verb(kind: &str) -> &'static str {
    match kind {
        EPIC_EVENT_TYPE => "epic",
        DOING_EVENT_TYPE => "doing",
        HEADSUP_EVENT_TYPE => "heads-up",
        other => unreachable!("unknown self-report kind {other}"),
    }
}

/// Resolve the calling instance's own name.
///
/// Returns None when the caller has no instance identity. Self-reports are
/// first-person, so an unidentified caller must be told to register rather than
/// have its state silently filed under a fallback name.
///
/// A name reaching here must belong to a registered instance: state recorded
/// against an unknown name would never surface in any listing, which is a silent
/// no-op from the caller's point of view.
fn resolve_self_name(db: &HcomDb, ctx: Option<&CommandContext>) -> Option<String> {
    let candidate = if let Some(c) = ctx
        && let Some(ref id) = c.identity
        && matches!(id.kind, SenderKind::Instance)
    {
        id.name.clone()
    } else if let Some(name) = ctx.and_then(|c| c.explicit_name.as_deref()) {
        name.to_string()
    } else {
        match identity::resolve_identity(db, None, None, None, None, None, None) {
            Ok(id) if matches!(id.kind, SenderKind::Instance) => id.name,
            _ => return None,
        }
    };

    match db.get_instance_full(&candidate) {
        Ok(Some(_)) => Some(candidate),
        _ => None,
    }
}

/// Shared implementation behind `epic`, `doing` and `heads-up`.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_selfreport(
    db: &HcomDb,
    kind: &str,
    words: &[String],
    ctx: Option<&CommandContext>,
) -> i32 {
    let (label, prompt) = kind_labels(kind);

    let Some(name) = resolve_self_name(db, ctx) else {
        eprintln!(
            "Error: no registered hcom agent for this session.\n\
             Run 'hcom start' first, or pass --name <agent> naming one from 'hcom list'."
        );
        return 1;
    };

    if words.is_empty() {
        let current = db.get_selfreport_field(kind, &name);
        if current.is_empty() {
            println!("{name} {label}: nothing set");
            println!("Set it with: hcom {} \"{prompt}\"", kind_verb(kind));
        } else {
            println!("{name} {label}: {current}");
        }
        return 0;
    }

    let text = words.join(" ");
    let text = text.trim();
    // Write-boundary guard: these are agent-authored free text rendered raw on
    // peer terminals (forum digest, roster, `-v`, `list <name>`, TUI) and
    // replicated relay-wide, so they must satisfy the same control-char / size
    // invariant as `hcom send`. They render on a single line, so tabs/newlines
    // are rejected too. Empty stays allowed as the intended clear.
    if let Err(e) = crate::messages::validate_text_field(text, false) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = db.log_selfreport_event(kind, &name, text) {
        eprintln!("Error: could not record {label}: {e}");
        return 1;
    }

    if text.is_empty() {
        println!("{name} {label}: cleared");
    } else {
        println!("{name} {label}: {text}");
    }
    0
}

/// Entry point for `hcom epic`.
pub fn cmd_epic(db: &HcomDb, args: &EpicArgs, ctx: Option<&CommandContext>) -> i32 {
    cmd_selfreport(db, EPIC_EVENT_TYPE, &args.text, ctx)
}

/// Entry point for `hcom doing`.
pub fn cmd_doing(db: &HcomDb, args: &DoingArgs, ctx: Option<&CommandContext>) -> i32 {
    cmd_selfreport(db, DOING_EVENT_TYPE, &args.text, ctx)
}

/// Entry point for `hcom heads-up`.
pub fn cmd_headsup(db: &HcomDb, args: &HeadsUpArgs, ctx: Option<&CommandContext>) -> i32 {
    cmd_selfreport(db, HEADSUP_EVENT_TYPE, &args.text, ctx)
}
