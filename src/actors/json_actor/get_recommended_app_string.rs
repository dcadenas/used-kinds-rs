use anyhow::Result;
use nostr_sdk::prelude::*;
use serde_json::Value;

/// Parse recommended app information from NIP-89 event.
///
/// Extracts app name and supported kinds from a NIP-89 handler event.
///
/// # Returns
///
/// Returns tuple of (supported_kinds, app_name).
///
/// # Note
///
/// Currently unused - recommended app functionality not yet fully implemented.
/// Kept for future NIP-89 integration.
#[allow(dead_code)]
pub fn parse_recommended_app(app_event: &Event) -> Result<(Box<[Kind]>, String)> {
    let alt_tag_data = app_event.tags.iter().find_map(|tag| {
        // Look for alt tags using tag name
        if tag.as_slice().first() == Some(&"alt".to_string()) {
            return tag.as_slice().get(1).cloned();
        }
        None
    });

    let kind_tag_data: Box<[Kind]> = app_event
        .tags
        .iter()
        .filter_map(|tag| {
            // Look for k tags (single letter)
            if tag.as_slice().first() == Some(&"k".to_string()) {
                if let Some(kind_str) = tag.as_slice().get(1) {
                    return kind_str.parse::<u16>().ok().map(Kind::from);
                }
            }
            None
        })
        .collect();

    let content_value = if app_event.content.trim().is_empty() {
        None
    } else {
        let content_json: Value = serde_json::from_str(&app_event.content)?;

        let candidates = [
            &content_json["website"],
            &content_json["display_name"],
            &content_json["name"],
        ];

        candidates
            .iter()
            .find_map(|value| value.as_str().map(String::from))
    };

    Ok((
        kind_tag_data,
        alt_tag_data
            .or(content_value)
            .unwrap_or_else(|| "None found".to_string()),
    ))
}
