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

pub fn should_log() -> bool {
    let interval_seconds = 10;
    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH).expect("Time went backwards");

    since_the_epoch.as_secs().is_multiple_of(interval_seconds)
}
