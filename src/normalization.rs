use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use regex::Regex;

lazy_static! {
    // Match 64-character hex strings (pubkeys, event IDs)
    static ref HEX_ID_RE: Regex = Regex::new(r"\b[0-9a-f]{64}\b").unwrap();

    // Match NIP-19 encoded entities
    static ref NIP19_RE: Regex = Regex::new(r"\b(npub|note|nevent|nprofile|naddr|nrelay|nsec)1[a-z0-9]{58,}\b").unwrap();

    // Match Unix timestamps (10 digits) and millisecond timestamps (13 digits)
    static ref TIMESTAMP_RE: Regex = Regex::new(r"\b\d{10,13}\b").unwrap();

    // Match URLs
    static ref URL_RE: Regex = Regex::new(r"https?://[^\s]+").unwrap();
}

/// Normalize event content by replacing identifiable values with placeholders
pub fn normalize_content(content: &str) -> String {
    let mut normalized = content.to_string();

    // Replace NIP-19 entities first (more specific)
    normalized = NIP19_RE.replace_all(&normalized, "<NIP19>").to_string();

    // Replace hex IDs
    normalized = HEX_ID_RE.replace_all(&normalized, "<HEX_ID>").to_string();

    // Replace timestamps
    normalized = TIMESTAMP_RE.replace_all(&normalized, "<TIMESTAMP>").to_string();

    // Replace URLs (keep structure but remove specific domains/paths)
    normalized = URL_RE.replace_all(&normalized, "<URL>").to_string();

    // Normalize whitespace
    normalized = normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    normalized
}

/// Extract normalized tag types and structures from an event
/// Returns a set of tag patterns like "e", "p", "d:value", etc.
pub fn normalize_tags(tags: &[Tag]) -> Vec<String> {
    let mut normalized = Vec::new();

    for tag in tags {
        // Get the tag as a string vector
        let tag_vec = tag.clone().to_vec();

        if tag_vec.is_empty() {
            continue;
        }

        let tag_type = &tag_vec[0];

        // For simple tags, just keep the type
        if tag_vec.len() == 1 {
            normalized.push(tag_type.clone());
            continue;
        }

        // For tags with values, normalize the value
        let value = &tag_vec[1];

        // Check if value is a hex ID or NIP-19
        let normalized_value = if HEX_ID_RE.is_match(value) {
            "<HEX_ID>".to_string()
        } else if NIP19_RE.is_match(value) {
            "<NIP19>".to_string()
        } else if TIMESTAMP_RE.is_match(value) {
            "<TIMESTAMP>".to_string()
        } else if URL_RE.is_match(value) {
            "<URL>".to_string()
        } else if value.len() > 20 {
            // Long values -> just note presence
            "<LONG_VALUE>".to_string()
        } else {
            // Short values -> keep them (e.g., "d" tag values, relay hints)
            value.clone()
        };

        // Create pattern: "tag_type:normalized_value"
        normalized.push(format!("{}:{}", tag_type, normalized_value));
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_content_hex_ids() {
        let content = "Check out event a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2 by author";
        let normalized = normalize_content(content);
        assert!(normalized.contains("<HEX_ID>"));
        assert!(!normalized.contains("a1b2c3d4"));
    }

    #[test]
    fn test_normalize_content_nip19() {
        // Use realistic NIP-19 encoded strings (63+ chars, lowercase only like real bech32)
        let content = "Follow npub1234567890abcdef234567890abcdef234567890abcdef234567890abcdef2 and check note1234567890abcdef234567890abcdef234567890abcdef234567890abcdef2";
        let normalized = normalize_content(content);
        assert!(normalized.contains("<NIP19>"), "Normalized content: {}", normalized);
        assert!(!normalized.contains("npub1"));
        assert!(!normalized.contains("note1"));
    }

    #[test]
    fn test_normalize_content_timestamps() {
        let content = "Posted at 1699123456 and later at 1699123456789";
        let normalized = normalize_content(content);
        assert_eq!(normalized, "Posted at <TIMESTAMP> and later at <TIMESTAMP>");
    }

    #[test]
    fn test_normalize_content_urls() {
        let content = "Visit https://example.com/path and http://test.org";
        let normalized = normalize_content(content);
        assert_eq!(normalized, "Visit <URL> and <URL>");
    }

    #[test]
    fn test_normalize_tags() {
        let tags = vec![
            Tag::parse(["e", "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"]).unwrap(),
            Tag::parse(["p", "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3"]).unwrap(),
            Tag::parse(["d", "my-identifier"]).unwrap(),
            Tag::parse(["t", "nostr"]).unwrap(),
        ];

        let normalized = normalize_tags(&tags);

        assert!(normalized.contains(&"e:<HEX_ID>".to_string()));
        assert!(normalized.contains(&"p:<HEX_ID>".to_string()));
        assert!(normalized.contains(&"d:my-identifier".to_string()));
        assert!(normalized.contains(&"t:nostr".to_string()));
    }
}
