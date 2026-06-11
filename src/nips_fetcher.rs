use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::RwLock;
use tracing::{error, info, warn};

const NIPS_README_URL: &str =
    "https://raw.githubusercontent.com/nostr-protocol/nips/master/README.md";
const CACHE_DURATION_HOURS: i64 = 24;

/// Hard deadline for the README fetch. main awaits this on the boot path
/// before any actor (or the health endpoint) starts, so without a deadline
/// a hung connection stalls startup forever; an error instead falls back
/// to the cached/static kind list.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

lazy_static! {
    static ref DOCUMENTED_KINDS: RwLock<DocumentedKinds> = RwLock::new(DocumentedKinds::default());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentedKinds {
    pub single_kinds: HashSet<u32>,
    pub range_kinds: Vec<(u32, u32)>, // (start, end) inclusive
    pub last_updated: DateTime<Utc>,
}

impl Default for DocumentedKinds {
    fn default() -> Self {
        // Fallback to the static list
        let mut single_kinds = HashSet::new();
        let static_kinds = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 40, 41, 42, 43, 44, 1021, 1022, 1040,
            1059, 1063, 1311, 1971, 1984, 1985, 4550, 7000, 9041, 9734, 9735, 9802, 10000, 10001,
            10002, 10003, 10004, 10005, 10006, 10007, 10009, 10015, 10030, 10096, 13194, 21000,
            22242, 23194, 23195, 24133, 27235, 30000, 30001, 30002, 30003, 30004, 30008, 30009,
            30015, 30017, 30018, 30019, 30020, 30023, 30024, 30030, 30063, 30078, 30311, 30315,
            30402, 30403, 31922, 31923, 31924, 31925, 31989, 31990, 34550,
        ];

        for kind in static_kinds {
            single_kinds.insert(kind);
        }

        // Add known ranges
        let range_kinds = vec![
            (5000, 5999), // Job Requests
            (6000, 6999), // Job Results
            (9000, 9030), // Group Control Events
        ];

        Self {
            single_kinds,
            range_kinds,
            last_updated: Utc::now() - Duration::days(30), // Force update on first use
        }
    }
}

impl DocumentedKinds {
    pub fn contains(&self, kind: u32) -> bool {
        if self.single_kinds.contains(&kind) {
            return true;
        }

        for (start, end) in &self.range_kinds {
            if kind >= *start && kind <= *end {
                return true;
            }
        }

        false
    }
}

/// Fetch the README body, failing instead of hanging past `timeout`.
async fn fetch_readme(url: &str, timeout: std::time::Duration) -> Result<String> {
    let response = reqwest::Client::builder()
        .timeout(timeout)
        .build()?
        .get(url)
        .send()
        .await?;
    Ok(response.text().await?)
}

pub async fn fetch_and_update_kinds() -> Result<()> {
    info!("Fetching latest NIPs documentation from GitHub");

    let content = fetch_readme(NIPS_README_URL, FETCH_TIMEOUT).await?;

    let kinds = parse_kinds_from_readme(&content)?;

    // Update the global state
    {
        let mut documented_kinds = DOCUMENTED_KINDS
            .write()
            .map_err(|_| anyhow!("Lock poisoned"))?;
        *documented_kinds = kinds;
    }

    info!("Successfully updated documented kinds from NIPs");
    Ok(())
}

fn parse_kinds_from_readme(content: &str) -> Result<DocumentedKinds> {
    let mut single_kinds = HashSet::new();
    let mut range_kinds = Vec::new();

    // Find the Event Kinds section
    let start_marker = "## Event Kinds";
    let start_idx = content
        .find(start_marker)
        .ok_or_else(|| anyhow!("Could not find Event Kinds section"))?;

    // Find the next section (to know where to stop)
    let content_after_start = &content[start_idx..];
    let end_idx = content_after_start[start_marker.len()..]
        .find("\n## ")
        .map(|idx| idx + start_marker.len())
        .unwrap_or(content_after_start.len());

    let kinds_section = &content_after_start[..end_idx];

    // Parse each line that looks like a table row
    for line in kinds_section.lines() {
        if !line.starts_with('|') || line.contains("---") || line.contains("kind") {
            continue; // Skip non-data rows
        }

        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }

        let kind_str = parts[1].trim();

        // Check for range format: `5000`-`5999` or `39000-9` (shorthand for 39000-39009)
        if kind_str.contains('-') {
            if let Some((start_str, end_str)) = kind_str.split_once('-') {
                let start = start_str
                    .trim_matches(|c| c == '`' || c == ' ')
                    .parse::<u32>()
                    .ok();
                let end_raw = end_str.trim_matches(|c| c == '`' || c == ' ');
                let end = end_raw.parse::<u32>().ok();

                if let (Some(start), Some(end)) = (start, end) {
                    // Handle shorthand notation like "39000-9" meaning 39000-39009
                    let actual_end = if end < 10 && start >= 10 {
                        // Extract the prefix from start and append the end digit
                        let start_str = start.to_string();
                        let prefix = &start_str[..start_str.len() - 1];
                        format!("{}{}", prefix, end).parse::<u32>().unwrap_or(end)
                    } else {
                        end
                    };

                    range_kinds.push((start, actual_end));
                }
            }
        } else {
            // Single kind format: `42`
            let cleaned = kind_str.trim_matches(|c| c == '`' || c == ' ');
            if let Ok(kind) = cleaned.parse::<u32>() {
                single_kinds.insert(kind);
            }
        }
    }

    // Validate we found some kinds
    if single_kinds.is_empty() && range_kinds.is_empty() {
        return Err(anyhow!("No kinds found in README"));
    }

    Ok(DocumentedKinds {
        single_kinds,
        range_kinds,
        last_updated: Utc::now(),
    })
}

pub fn is_kind_documented(kind: u32) -> bool {
    if let Ok(documented_kinds) = DOCUMENTED_KINDS.read() {
        documented_kinds.contains(kind)
    } else {
        // Fallback to conservative approach if lock fails
        warn!("Could not acquire lock for documented kinds, using fallback");
        let default_kinds = DocumentedKinds::default();
        default_kinds.contains(kind)
    }
}

pub async fn ensure_kinds_updated() -> Result<()> {
    let needs_update = {
        let documented_kinds = DOCUMENTED_KINDS
            .read()
            .map_err(|_| anyhow!("Lock poisoned"))?;
        let age = Utc::now() - documented_kinds.last_updated;
        age > Duration::hours(CACHE_DURATION_HOURS)
    };

    if needs_update {
        if let Err(e) = fetch_and_update_kinds().await {
            error!("Failed to update kinds from NIPs: {}", e);
            // Continue with cached/default data
        }
    }

    Ok(())
}

pub async fn load_or_fetch_kinds(cache_path: Option<&Path>) -> Result<()> {
    // Try to load from cache first
    if let Some(path) = cache_path {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    if let Ok(kinds) = serde_json::from_str::<DocumentedKinds>(&content) {
                        let mut documented_kinds = DOCUMENTED_KINDS
                            .write()
                            .map_err(|_| anyhow!("Lock poisoned"))?;
                        *documented_kinds = kinds;
                        info!("Loaded documented kinds from cache");
                    }
                }
                Err(e) => warn!("Failed to read cache file: {}", e),
            }
        }
    }

    // Check if update is needed
    ensure_kinds_updated().await?;

    // Save to cache if path provided
    if let Some(path) = cache_path {
        let documented_kinds = DOCUMENTED_KINDS
            .read()
            .map_err(|_| anyhow!("Lock poisoned"))?;
        if let Ok(json) = serde_json::to_string_pretty(&*documented_kinds) {
            if let Err(e) = std::fs::write(path, json) {
                warn!("Failed to save kinds cache: {}", e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kinds_table() {
        let sample_readme = r#"
## Event Kinds

| kind          | description                     | NIP         |
| ------------- | ------------------------------- | ----------- |
| `0`           | User Metadata                   | [01](01.md) |
| `1`           | Short Text Note                 | [10](10.md) |
| `5000`-`5999` | Job Request                     | [90](90.md) |
| `6000`-`6999` | Job Result                      | [90](90.md) |

## Message Types
"#;

        let kinds = parse_kinds_from_readme(sample_readme).unwrap();

        assert!(kinds.single_kinds.contains(&0));
        assert!(kinds.single_kinds.contains(&1));
        assert_eq!(kinds.range_kinds.len(), 2);
        assert!(kinds.range_kinds.contains(&(5000, 5999)));
        assert!(kinds.range_kinds.contains(&(6000, 6999)));
    }

    #[test]
    fn test_contains() {
        let kinds = DocumentedKinds {
            single_kinds: [1, 2, 3].iter().cloned().collect(),
            range_kinds: vec![(100, 200), (5000, 5999)],
            last_updated: Utc::now(),
        };

        assert!(kinds.contains(1));
        assert!(kinds.contains(150));
        assert!(kinds.contains(5500));
        assert!(!kinds.contains(4));
        assert!(!kinds.contains(250));
    }

    #[tokio::test]
    async fn test_fetch_readme_times_out_instead_of_hanging() {
        // A bound listener nobody accepts on: the OS backlog completes the
        // TCP handshake but no HTTP response ever arrives. Without a client
        // deadline this await hangs forever — the boot-path failure mode.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetch_readme(&url, std::time::Duration::from_millis(250)),
        )
        .await
        .expect("fetch must hit its own deadline, not hang");

        assert!(result.is_err(), "silent server must yield a timeout error");
    }

    #[test]
    fn test_parse_shorthand_range() {
        let sample_readme = r#"
## Event Kinds

| kind          | description                     | NIP         |
| ------------- | ------------------------------- | ----------- |
| `1`           | Short Text Note                 | [10](10.md) |
| `39000-9`     | Group metadata events           | [29](29.md) |

## Message Types
"#;

        let kinds = parse_kinds_from_readme(sample_readme).unwrap();

        assert_eq!(kinds.range_kinds.len(), 1);
        assert_eq!(kinds.range_kinds[0], (39000, 39009));

        // Verify all kinds in range are documented
        assert!(kinds.contains(39000));
        assert!(kinds.contains(39005));
        assert!(kinds.contains(39009));
        assert!(!kinds.contains(39010));
    }
}
