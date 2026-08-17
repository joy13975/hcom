//! Forum cadence and claim advisories — the harness-agnostic half.
//!
//! Both features are enforced through hooks that already fire on every turn, so
//! neither needs a daemon or a timer. This module holds the decisions ("is this
//! agent's `doing` stale?", "is this write into someone else's claim?") and
//! returns plain strings; each harness's hook module wraps them in whatever JSON
//! shape that harness expects. Keeping the logic here is what stops the feature
//! from becoming per-harness code.

use crate::db::HcomDb;
use crate::shared::time::{format_age, now_epoch_i64};

/// KV key holding the last time we nudged one instance.
fn nudge_key(instance: &str) -> String {
    format!("forum:last_nudge:{instance}")
}

/// Should this instance be reminded to refresh its `doing`, and if so, what text?
///
/// Returns None when the feature is off, the report is fresh, or we already
/// nudged inside the current window. **The rate limit is not optional**: these
/// hooks fire on every tool call, so an unlimited nudge would append a reminder
/// to hundreds of turns and get tuned out (or drown real messages).
///
/// Records the nudge time as a side effect when it returns Some, so a caller must
/// only call this when it will actually deliver the text.
pub fn staleness_nudge(db: &HcomDb, instance: &str) -> Option<String> {
    let config = crate::config::HcomConfig::load(None).ok()?;
    if !config.doing_nudge {
        return None;
    }
    let max_age = config.doing_max_age;
    let now = now_epoch_i64();

    // Rate limit first: it is the cheap check and short-circuits the common case.
    let last_nudge: i64 = db
        .kv_get(&nudge_key(instance))
        .ok()
        .flatten()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    if last_nudge > 0 && now - last_nudge < max_age {
        return None;
    }

    let report = db.get_selfreport(instance);
    let age = match report.doing_at.as_deref() {
        Some(raw) => {
            let stamped = chrono::DateTime::parse_from_rfc3339(raw)
                .ok()
                .map(|t| t.timestamp())?;
            now - stamped
        }
        // Never set. Nudge once so a silent agent starts participating, but only
        // after it has been alive long enough for the primer to have landed.
        None => {
            let created = db
                .get_instance_full(instance)
                .ok()
                .flatten()
                .map(|row| row.created_at as i64)
                .unwrap_or(0);
            if created == 0 || now - created < max_age {
                return None;
            }
            now - created
        }
    };

    if age < max_age {
        return None;
    }

    let _ = db.kv_set(&nudge_key(instance), Some(&now.to_string()));

    Some(if report.doing.is_empty() {
        "[hcom] you have not told the forum what you are working on. \
         Run: hcom epic \"<your line of work>\" and hcom doing \"<current focus>\" \
         (peers read these with 'hcom forum')"
            .to_string()
    } else {
        format!(
            "[hcom] your \"doing\" is {} old and peers are reading it as current. \
             Refresh it: hcom doing \"<what you are on now>\"",
            format_age(age)
        )
    })
}

/// Warn text when `path` is covered by another agent's live claim.
///
/// Returns None when nothing conflicts. The caller decides whether this warns or
/// denies, based on `claim_block`.
pub fn claim_advisory(db: &HcomDb, instance: &str, path: &str) -> Option<String> {
    let conflicts = db.conflicting_claims(instance, path);
    if conflicts.is_empty() {
        return None;
    }
    let now = now_epoch_i64();
    let held: Vec<String> = conflicts
        .iter()
        .map(|c| {
            format!(
                "{} claims {} (expires in {})",
                c.instance,
                c.pattern,
                format_age(c.expires_in(now))
            )
        })
        .collect();
    Some(format!(
        "[hcom] {path} is claimed by another agent: {}. \
         Coordinate before editing (hcom send) or wait for the claim to lapse.",
        held.join("; ")
    ))
}

/// Is claim enforcement set to deny rather than warn?
///
/// An unreadable config falls back to the advisory default, and says so: silently
/// choosing "warn" when the operator asked for "deny" would misreport the
/// protection in force.
pub fn claim_blocks() -> bool {
    match crate::config::HcomConfig::load(None) {
        Ok(config) => config.claim_block,
        Err(e) => {
            crate::log::log_error(
                "forum",
                "claim_blocks.config_unreadable",
                &format!("falling back to advisory (warn, not deny): {e}"),
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DOING_EVENT_TYPE;
    use crate::db::tests::{cleanup_test_db, setup_full_test_db};

    /// A fresh report must not nudge, and a nudge must not repeat inside the
    /// window - the rate limit is the part most likely to be wrong, and a
    /// reminder on every tool call would be worse than none.
    #[test]
    fn fresh_doing_never_nudges() {
        let (db, db_path) = setup_full_test_db();
        db.log_selfreport_event(DOING_EVENT_TYPE, "luna", "on it")
            .unwrap();
        assert_eq!(staleness_nudge(&db, "luna"), None);
        cleanup_test_db(db_path);
    }

    #[test]
    fn stale_doing_nudges_once_then_stays_quiet() {
        let (db, db_path) = setup_full_test_db();

        // Stamp a `doing` event well outside any sane window.
        let old = chrono::Utc::now() - chrono::Duration::seconds(4000);
        db.log_event_with_ts(
            DOING_EVENT_TYPE,
            "luna",
            &serde_json::json!({"text": "stale work"}),
            Some(&old.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()),
        )
        .unwrap();

        let first = staleness_nudge(&db, "luna");
        assert!(first.is_some(), "a stale doing must nudge");
        assert!(first.unwrap().contains("Refresh it"));

        assert_eq!(
            staleness_nudge(&db, "luna"),
            None,
            "must not nudge again inside the window"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn claim_advisory_names_the_holder_and_ignores_own_claims() {
        let (db, db_path) = setup_full_test_db();
        db.add_claim("luna", "src/auth/**", 1800).unwrap();

        assert_eq!(claim_advisory(&db, "luna", "/repo/src/auth/a.rs"), None);
        let warning = claim_advisory(&db, "nova", "/repo/src/auth/a.rs").unwrap();
        assert!(warning.contains("luna"));
        assert!(warning.contains("src/auth/**"));

        cleanup_test_db(db_path);
    }
}
