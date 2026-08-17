//! `hcom doing` command — advertise what this agent is currently working on.
//!
//! First-person only: it records the CALLER's own activity, which other agents
//! then read through `hcom list`. Stored as a `doing` event rather than an
//! `instances` column because the hook lifecycle rewrites `status_context` and
//! `status_detail` on every tool-use tick and would erase agent-authored text.

use crate::db::HcomDb;
use crate::identity;
use crate::shared::{CommandContext, SenderKind};

/// Parsed arguments for `hcom doing`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "doing",
    about = "Set or show what you are currently working on"
)]
pub struct DoingArgs {
    /// Activity text. Omit to print the current value; pass an empty string to clear it.
    pub text: Vec<String>,
}

/// Resolve the calling instance's own name.
///
/// Returns None when the caller has no instance identity. `doing` is
/// first-person, so an unidentified caller must be told to register rather than
/// have its activity silently filed under a fallback name.
///
/// A name reaching here must belong to a registered instance: activity recorded
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

/// Main entry point for `hcom doing`.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_doing(db: &HcomDb, args: &DoingArgs, ctx: Option<&CommandContext>) -> i32 {
    let Some(name) = resolve_self_name(db, ctx) else {
        eprintln!(
            "Error: no registered hcom agent for this session.\n\
             Run 'hcom start' first, or pass --name <agent> naming one from 'hcom list'."
        );
        return 1;
    };

    if args.text.is_empty() {
        let current = db.get_doing(&name);
        if current.is_empty() {
            println!("{name}: nothing set");
            println!("Set it with: hcom doing \"what you are working on\"");
        } else {
            println!("{name}: {current}");
        }
        return 0;
    }

    let text = args.text.join(" ");
    let text = text.trim();
    if let Err(e) = db.log_doing_event(&name, text) {
        eprintln!("Error: could not record activity: {e}");
        return 1;
    }

    if text.is_empty() {
        println!("{name}: cleared");
    } else {
        println!("{name}: {text}");
    }
    0
}
