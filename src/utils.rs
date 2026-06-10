use crate::nips_fetcher;
use nostr_sdk::prelude::Kind;
use std::time::{SystemTime, UNIX_EPOCH};

/// Checks if a kind is free (not taken) according to Nostr NIPs.
///
/// # Arguments
///
/// * `kind` - The kind of the event to check.
///
/// # Returns
///
/// Returns `true` if the kind is free; otherwise, returns `false`.
pub fn is_kind_free(kind: Kind) -> bool {
    let kind_num = kind.as_u16() as u32;
    !nips_fetcher::is_kind_documented(kind_num)
}

/// Whether a millisecond Unix timestamp is older than the 30-day retention
/// window. Entries past it are deleted by the periodic cleanup pass and
/// skipped by the stats.json migration.
pub fn is_old(unix_time_millis: i64) -> bool {
    unix_time_millis < (chrono::Utc::now() - chrono::Duration::days(30)).timestamp_millis()
}

pub fn should_log() -> bool {
    let interval_seconds = 10;
    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH).expect("Time went backwards");

    since_the_epoch.as_secs().is_multiple_of(interval_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_is_old_accepts_recent_and_rejects_stale_timestamps() {
        let now = Utc::now().timestamp_millis();
        assert!(!is_old(now));
        assert!(!is_old(now - chrono::Duration::days(29).num_milliseconds()));
        assert!(is_old(now - chrono::Duration::days(31).num_milliseconds()));
    }
}
