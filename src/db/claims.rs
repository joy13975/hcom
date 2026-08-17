//! Advisory path reservations (`hcom claim`).
//!
//! A claim says "I intend to edit these paths" so a peer about to touch the same
//! files is warned *before* writing rather than notified after the fact (which
//! is what the existing `collision` subscription does). Claims are deliberately
//! **advisory**: they expire, they are released when the holder stops, and by
//! default they warn rather than deny. A hard lock held by a crashed agent would
//! wedge the whole forum, which is a worse failure than two agents overlapping.

use anyhow::Result;
use rusqlite::params;

use super::HcomDb;
use crate::shared::time::now_epoch_i64;

/// Default claim lifetime in seconds when `--ttl` is not given.
pub const DEFAULT_CLAIM_TTL_SECS: i64 = 1800;

/// One live path reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub id: i64,
    pub instance: String,
    pub pattern: String,
    pub created_at: i64,
    pub expires_at: i64,
}

impl Claim {
    /// Seconds until this claim expires, floored at zero.
    pub fn expires_in(&self, now: i64) -> i64 {
        (self.expires_at - now).max(0)
    }
}

/// Does `path` fall under `pattern`?
///
/// A relative pattern (`src/auth/**`) is anchored anywhere in the path, because
/// the claiming agent types it relative to its own working directory while the
/// hook that checks it sees an absolute path. An absolute pattern is matched as
/// given. Over-matching is the safe direction here: the consequence of a false
/// positive is one extra advisory line, while a false negative is the silent
/// double-edit the feature exists to prevent.
pub fn pattern_matches_path(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() || path.is_empty() {
        return false;
    }
    let candidates = if std::path::Path::new(pattern).is_absolute() {
        vec![pattern.to_string()]
    } else {
        // Anchor the relative pattern at any depth, and also try it as-is in
        // case the caller passed a relative path too.
        vec![format!("**/{pattern}"), pattern.to_string()]
    };
    for candidate in candidates {
        let Ok(compiled) = glob::Pattern::new(&candidate) else {
            continue;
        };
        if compiled.matches(path) {
            return true;
        }
        // `src/auth` should cover everything beneath it, not just the directory
        // entry itself, without the agent having to remember the `/**` suffix.
        if !candidate.ends_with("**")
            && let Ok(recursive) =
                glob::Pattern::new(&format!("{}/**", candidate.trim_end_matches('/')))
            && recursive.matches(path)
        {
            return true;
        }
    }
    false
}

impl HcomDb {
    /// Register a claim. Returns the new claim's id.
    pub fn add_claim(&self, instance: &str, pattern: &str, ttl_secs: i64) -> Result<i64> {
        let now = now_epoch_i64();
        self.conn.execute(
            "INSERT INTO claims (instance, pattern, created_at, expires_at)
             VALUES (?, ?, ?, ?)",
            params![instance, pattern, now, now + ttl_secs],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every claim that is neither released nor expired.
    pub fn live_claims(&self) -> Vec<Claim> {
        let now = now_epoch_i64();
        let mut stmt = match self.conn.prepare(
            "SELECT id, instance, pattern, created_at, expires_at FROM claims
             WHERE released_at IS NULL AND expires_at > ? ORDER BY id",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let Ok(rows) = stmt.query_map(params![now], |row| {
            Ok(Claim {
                id: row.get(0)?,
                instance: row.get(1)?,
                pattern: row.get(2)?,
                created_at: row.get(3)?,
                expires_at: row.get(4)?,
            })
        }) else {
            return Vec::new();
        };
        rows.filter_map(|row| row.ok()).collect()
    }

    /// Live claims held by one instance.
    pub fn live_claims_for(&self, instance: &str) -> Vec<Claim> {
        self.live_claims()
            .into_iter()
            .filter(|c| c.instance == instance)
            .collect()
    }

    /// Live claims on `path` held by someone other than `caller`.
    ///
    /// This is the question the PreToolUse hook asks. A caller's own claim never
    /// warns it about its own edit.
    pub fn conflicting_claims(&self, caller: &str, path: &str) -> Vec<Claim> {
        self.live_claims()
            .into_iter()
            .filter(|c| c.instance != caller && pattern_matches_path(&c.pattern, path))
            .collect()
    }

    /// Release one instance's claims matching `pattern` exactly. Returns the count.
    pub fn release_claim(&self, instance: &str, pattern: &str) -> Result<usize> {
        let now = now_epoch_i64();
        Ok(self.conn.execute(
            "UPDATE claims SET released_at = ?
             WHERE instance = ? AND pattern = ? AND released_at IS NULL",
            params![now, instance, pattern],
        )?)
    }

    /// Release every claim held by one instance. Returns the count.
    ///
    /// Called on stop as well as by `hcom claim --release --all`, so a finished
    /// agent does not leave its reservations standing for the rest of the TTL.
    pub fn release_all_claims(&self, instance: &str) -> Result<usize> {
        let now = now_epoch_i64();
        Ok(self.conn.execute(
            "UPDATE claims SET released_at = ? WHERE instance = ? AND released_at IS NULL",
            params![now, instance],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{cleanup_test_db, setup_full_test_db};

    #[test]
    fn relative_pattern_matches_at_any_depth() {
        assert!(pattern_matches_path(
            "src/auth/**",
            "/Users/joy/code/app/src/auth/token.rs"
        ));
        assert!(pattern_matches_path("src/auth/**", "src/auth/token.rs"));
        assert!(!pattern_matches_path(
            "src/auth/**",
            "/Users/joy/code/app/src/db/token.rs"
        ));
    }

    /// A bare directory should cover its contents; forgetting `/**` is the
    /// obvious agent mistake and must not silently reserve nothing.
    #[test]
    fn bare_directory_covers_its_contents() {
        assert!(pattern_matches_path("src/auth", "/repo/src/auth/token.rs"));
    }

    #[test]
    fn absolute_pattern_is_not_anchored_elsewhere() {
        assert!(pattern_matches_path("/repo/src/**", "/repo/src/a.rs"));
        assert!(!pattern_matches_path("/repo/src/**", "/other/src/a.rs"));
    }

    #[test]
    fn empty_pattern_or_path_never_matches() {
        assert!(!pattern_matches_path("", "/repo/a.rs"));
        assert!(!pattern_matches_path("src/**", ""));
    }

    #[test]
    fn own_claim_does_not_conflict_but_a_peers_does() {
        let (db, db_path) = setup_full_test_db();

        db.add_claim("luna", "src/auth/**", DEFAULT_CLAIM_TTL_SECS)
            .unwrap();

        assert!(
            db.conflicting_claims("luna", "/repo/src/auth/token.rs")
                .is_empty(),
            "an agent must not be warned about its own claim"
        );
        let conflicts = db.conflicting_claims("nova", "/repo/src/auth/token.rs");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].instance, "luna");

        cleanup_test_db(db_path);
    }

    #[test]
    fn expired_claim_never_conflicts() {
        let (db, db_path) = setup_full_test_db();

        // TTL already in the past.
        db.add_claim("luna", "src/auth/**", -10).unwrap();

        assert!(
            db.live_claims().is_empty(),
            "expired claim must not be live"
        );
        assert!(
            db.conflicting_claims("nova", "/repo/src/auth/token.rs")
                .is_empty()
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn released_claim_never_conflicts() {
        let (db, db_path) = setup_full_test_db();

        db.add_claim("luna", "src/auth/**", DEFAULT_CLAIM_TTL_SECS)
            .unwrap();
        assert_eq!(db.release_claim("luna", "src/auth/**").unwrap(), 1);

        assert!(db.live_claims().is_empty());
        assert!(
            db.conflicting_claims("nova", "/repo/src/auth/token.rs")
                .is_empty()
        );

        cleanup_test_db(db_path);
    }

    /// A crashed or stopped agent must not hold its reservations for the rest of
    /// the TTL — that is what makes advisory claims safe.
    #[test]
    fn release_all_clears_only_the_named_instance() {
        let (db, db_path) = setup_full_test_db();

        db.add_claim("luna", "a/**", DEFAULT_CLAIM_TTL_SECS)
            .unwrap();
        db.add_claim("luna", "b/**", DEFAULT_CLAIM_TTL_SECS)
            .unwrap();
        db.add_claim("nova", "c/**", DEFAULT_CLAIM_TTL_SECS)
            .unwrap();

        assert_eq!(db.release_all_claims("luna").unwrap(), 2);
        let live = db.live_claims();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].instance, "nova");

        cleanup_test_db(db_path);
    }
}
