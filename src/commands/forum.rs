//! `hcom forum` — one call that answers "what is everyone working on?".
//!
//! `hcom list` answers whether an agent is awake and shows a truncated activity
//! line. The forum digest is the other question: for every agent, its high-level
//! line of work, its current focus with the AGE of that focus, what it warns
//! peers about, and which paths it has reserved. The age matters — a self-report
//! nobody refreshes becomes a lie, so every reader shows how old it is instead of
//! presenting stale text as current.

use crate::db::{HcomDb, SelfReport};
use crate::shared::time::{format_age, now_epoch_i64};

/// Parsed arguments for `hcom forum`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "forum",
    about = "Show what every agent is working on, warning about, and has claimed"
)]
pub struct ForumArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Age of a `doing` report in seconds, from its stored ISO timestamp.
fn doing_age_seconds(report: &SelfReport) -> Option<i64> {
    let raw = report.doing_at.as_deref()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp())?;
    Some((chrono::Utc::now().timestamp() - parsed).max(0))
}

/// Main entry point for `hcom forum`.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_forum(db: &HcomDb, args: &ForumArgs) -> i32 {
    let reports = db.get_selfreport_map();
    let claims = db.live_claims();
    let now = now_epoch_i64();

    // Every agent that advertises anything OR holds a claim. Sorted so repeated
    // calls are diffable rather than reordering on every run.
    let mut names: Vec<String> = reports.keys().cloned().collect();
    for claim in &claims {
        if !names.contains(&claim.instance) {
            names.push(claim.instance.clone());
        }
    }
    names.sort();

    if args.json {
        let payload: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                let report = reports.get(name).cloned().unwrap_or_default();
                serde_json::json!({
                    "name": name,
                    "epic": report.epic,
                    "doing": report.doing,
                    "doing_age_seconds": doing_age_seconds(&report),
                    "headsup": report.headsup,
                    "claims": claims
                        .iter()
                        .filter(|c| &c.instance == name)
                        .map(|c| serde_json::json!({
                            "pattern": c.pattern,
                            "expires_in_seconds": c.expires_in(now),
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        match serde_json::to_string_pretty(&payload) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("Error: could not serialize forum: {e}");
                return 1;
            }
        }
        return 0;
    }

    if names.is_empty() {
        println!("Nobody has posted to the forum yet.");
        println!("Advertise your work with:");
        println!("  hcom epic \"<the line of work you are on>\"");
        println!("  hcom doing \"<what you are doing right now>\"");
        println!("  hcom heads-up \"<what peers should watch out for>\"");
        return 0;
    }

    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let report = reports.get(name).cloned().unwrap_or_default();
        println!("{name}");
        if !report.epic.is_empty() {
            println!("  epic:     {}", report.epic);
        }
        if !report.doing.is_empty() {
            match doing_age_seconds(&report) {
                // `format_age` already renders sub-minute ages as "now", so
                // appending "ago" would read as "(now ago)".
                Some(age) if age < 60 => println!("  doing:    {}  (just now)", report.doing),
                Some(age) => println!("  doing:    {}  ({} ago)", report.doing, format_age(age)),
                None => println!("  doing:    {}", report.doing),
            }
        }
        if !report.headsup.is_empty() {
            println!("  heads-up: {}", report.headsup);
        }
        for claim in claims.iter().filter(|c| &c.instance == name) {
            println!(
                "  claims:   {}  (expires in {})",
                claim.pattern,
                format_age(claim.expires_in(now))
            );
        }
    }

    0
}
