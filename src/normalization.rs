use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use regex::Regex;
use serde_json::Value;

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

/// JSON structure metrics for adaptive feature extraction
#[derive(Debug, Clone, Default)]
pub struct JsonStructure {
    pub is_valid_json: bool,
    pub is_object: bool,
    pub is_array: bool,
    pub top_level_key_count: usize,
    pub nested_key_count: usize,  // Keys 2 levels deep
    pub max_nesting_depth: usize,
    pub has_arrays: bool,
    pub string_value_ratio: f32,
    pub number_value_ratio: f32,
    pub bool_value_ratio: f32,
    pub null_value_ratio: f32,
}

/// Analyze JSON structure from content string
pub fn analyze_json_structure(content: &str) -> JsonStructure {
    let trimmed = content.trim();

    // Quick check if it looks like JSON
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return JsonStructure::default();
    }

    // Try to parse as JSON
    let parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return JsonStructure::default(),
    };

    let mut structure = JsonStructure {
        is_valid_json: true,
        ..Default::default()
    };

    match &parsed {
        Value::Object(obj) => {
            structure.is_object = true;
            structure.top_level_key_count = obj.len();

            // Count nested keys (2 levels deep)
            for value in obj.values() {
                if let Value::Object(nested) = value {
                    structure.nested_key_count += nested.len();
                }
            }

            // Analyze structure
            analyze_value_recursive(&parsed, 0, &mut structure);
        }
        Value::Array(arr) => {
            structure.is_array = true;
            structure.top_level_key_count = arr.len();
            analyze_value_recursive(&parsed, 0, &mut structure);
        }
        _ => {}
    }

    structure
}

fn analyze_value_recursive(value: &Value, depth: usize, structure: &mut JsonStructure) {
    // Track max depth
    if depth > structure.max_nesting_depth {
        structure.max_nesting_depth = depth;
    }

    // Check for arrays
    check_for_arrays(value, structure);

    // Track value type distribution
    let mut total_values = 0;
    let mut type_counts = [0usize; 4]; // [string, number, bool, null]

    count_value_types(value, &mut total_values, &mut type_counts, depth);

    if total_values > 0 {
        structure.string_value_ratio = type_counts[0] as f32 / total_values as f32;
        structure.number_value_ratio = type_counts[1] as f32 / total_values as f32;
        structure.bool_value_ratio = type_counts[2] as f32 / total_values as f32;
        structure.null_value_ratio = type_counts[3] as f32 / total_values as f32;
    }
}

fn check_for_arrays(value: &Value, structure: &mut JsonStructure) {
    match value {
        Value::Array(_) => {
            structure.has_arrays = true;
        }
        Value::Object(obj) => {
            for val in obj.values() {
                if structure.has_arrays {
                    return; // Already found, no need to continue
                }
                check_for_arrays(val, structure);
            }
        }
        _ => {}
    }
}

fn count_value_types(value: &Value, total: &mut usize, counts: &mut [usize; 4], depth: usize) {
    match value {
        Value::String(_) => {
            *total += 1;
            counts[0] += 1;
        }
        Value::Number(_) => {
            *total += 1;
            counts[1] += 1;
        }
        Value::Bool(_) => {
            *total += 1;
            counts[2] += 1;
        }
        Value::Null => {
            *total += 1;
            counts[3] += 1;
        }
        Value::Object(obj) => {
            for val in obj.values() {
                count_value_types(val, total, counts, depth + 1);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                count_value_types(val, total, counts, depth + 1);
            }
        }
    }
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
