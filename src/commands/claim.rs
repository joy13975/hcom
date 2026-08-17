//! `hcom claim` — advisory path reservations so peers do not thrash the same files.
//!
//! Claiming is first-person and advisory. It records "I intend to edit these
//! paths"; the `PreToolUse` hook then warns a *different* agent before it writes
//! somewhere already claimed. Nothing is locked: claims expire, are released on
//! stop, and warn rather than deny unless `claim_block` is turned on.

use crate::db::{DEFAULT_CLAIM_TTL_SECS, HcomDb};
use crate::identity;
use crate::shared::time::{format_age, now_epoch_i64};
use crate::shared::{CommandContext, SenderKind};

/// Parsed arguments for `hcom claim`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "claim",
    about = "Reserve paths you are about to edit (advisory)"
)]
pub struct ClaimArgs {
    /// Path or glob to claim, e.g. "src/auth/**". Omit with --list to show all.
    pub pattern: Vec<String>,

    /// How long the claim lasts, e.g. 30m, 2h, 900s. Default 30m.
    #[arg(long)]
    pub ttl: Option<String>,

    /// Release the named pattern instead of claiming it.
    #[arg(long)]
    pub release: bool,

    /// With --release, release every claim you hold.
    #[arg(long)]
    pub all: bool,

    /// Show every live claim and who holds it.
    #[arg(long)]
    pub list: bool,
}

/// Parse a TTL like `30m`, `2h`, `900s` or a bare seconds count.
///
/// Rejects zero and negatives: a claim that is already expired the moment it is
/// created would advertise protection the forum is not providing.
fn parse_ttl(raw: &str) -> Result<i64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty --ttl".to_string());
    }
    let (digits, multiplier) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        Some('d') => (&raw[..raw.len() - 1], 86400),
        _ => (raw, 1),
    };
    let value: i64 = digits
        .parse()
        .map_err(|_| format!("invalid --ttl '{raw}' (use 30m, 2h, 900s)"))?;
    if value <= 0 {
        return Err(format!("--ttl must be positive, got '{raw}'"));
    }
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("--ttl '{raw}' is too large"))
}

/// Resolve the calling instance's own name; see `selfreport::resolve_self_name`.
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

/// Main entry point for `hcom claim`.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_claim(db: &HcomDb, args: &ClaimArgs, ctx: Option<&CommandContext>) -> i32 {
    // --list is a read-only query and must work without an identity, so a human
    // at a shell can see who is holding what.
    if args.list {
        let claims = db.live_claims();
        if claims.is_empty() {
            println!("No live claims");
            return 0;
        }
        let now = now_epoch_i64();
        for claim in claims {
            println!(
                "{}  {}  (expires in {})",
                claim.instance,
                claim.pattern,
                format_age(claim.expires_in(now))
            );
        }
        return 0;
    }

    let Some(name) = resolve_self_name(db, ctx) else {
        eprintln!(
            "Error: no registered hcom agent for this session.\n\
             Run 'hcom start' first, or pass --name <agent> naming one from 'hcom list'."
        );
        return 1;
    };

    if args.release {
        if args.all {
            match db.release_all_claims(&name) {
                Ok(count) => {
                    println!("{name}: released {count} claim(s)");
                    return 0;
                }
                Err(e) => {
                    eprintln!("Error: could not release claims: {e}");
                    return 1;
                }
            }
        }
        if args.pattern.is_empty() {
            eprintln!("Error: --release needs a pattern, or --release --all");
            return 1;
        }
        let pattern = args.pattern.join(" ");
        match db.release_claim(&name, pattern.trim()) {
            Ok(0) => {
                // Not an error: releasing something you do not hold is a no-op,
                // but say so rather than implying a release happened.
                println!("{name}: no live claim on {}", pattern.trim());
                0
            }
            Ok(count) => {
                println!("{name}: released {count} claim(s) on {}", pattern.trim());
                0
            }
            Err(e) => {
                eprintln!("Error: could not release claim: {e}");
                1
            }
        }
    } else {
        if args.all {
            eprintln!("Error: --all only applies with --release");
            return 1;
        }
        if args.pattern.is_empty() {
            let held = db.live_claims_for(&name);
            if held.is_empty() {
                println!("{name}: no live claims");
                println!("Claim paths with: hcom claim \"src/auth/**\"");
            } else {
                let now = now_epoch_i64();
                for claim in held {
                    println!(
                        "{name}: {}  (expires in {})",
                        claim.pattern,
                        format_age(claim.expires_in(now))
                    );
                }
            }
            return 0;
        }

        let pattern = args.pattern.join(" ");
        let pattern = pattern.trim();
        // Same write-boundary guard as the self-report fields: a claim pattern is
        // agent-authored text rendered raw on peer terminals and replicated
        // relay-wide.
        if let Err(e) = crate::messages::validate_text_field(pattern, false) {
            eprintln!("Error: {e}");
            return 1;
        }
        if pattern.is_empty() {
            eprintln!("Error: claim pattern required, e.g. hcom claim \"src/auth/**\"");
            return 1;
        }
        // Reject a pattern glob cannot compile rather than storing a claim that
        // will silently never match anything.
        if glob::Pattern::new(pattern).is_err() {
            eprintln!("Error: '{pattern}' is not a valid path glob");
            return 1;
        }

        let ttl = match args.ttl.as_deref() {
            Some(raw) => match parse_ttl(raw) {
                Ok(secs) => secs,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            },
            None => DEFAULT_CLAIM_TTL_SECS,
        };

        // Tell the claimer who is already there. Claims are advisory, so an
        // overlap is allowed - but it must not be silent.
        let existing = db.conflicting_claims(&name, pattern);
        for claim in &existing {
            println!(
                "Note: {} already claims {} (overlaps yours)",
                claim.instance, claim.pattern
            );
        }

        match db.add_claim(&name, pattern, ttl) {
            Ok(_) => {
                println!("{name}: claimed {pattern} for {}", format_age(ttl));
                0
            }
            Err(e) => {
                eprintln!("Error: could not record claim: {e}");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_ttl;

    #[test]
    fn ttl_units_parse() {
        assert_eq!(parse_ttl("900s").unwrap(), 900);
        assert_eq!(parse_ttl("30m").unwrap(), 1800);
        assert_eq!(parse_ttl("2h").unwrap(), 7200);
        assert_eq!(parse_ttl("1d").unwrap(), 86400);
        assert_eq!(parse_ttl("45").unwrap(), 45);
    }

    /// A non-positive TTL would create an already-expired claim, which would
    /// advertise protection that does not exist.
    #[test]
    fn ttl_rejects_nonpositive_and_garbage() {
        assert!(parse_ttl("0").is_err());
        assert!(parse_ttl("0m").is_err());
        assert!(parse_ttl("-5m").is_err());
        assert!(parse_ttl("soon").is_err());
        assert!(parse_ttl("").is_err());
    }
}
