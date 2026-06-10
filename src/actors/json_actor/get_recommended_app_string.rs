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

#[cfg(test)]
mod tests {
    use super::*;

    fn handler_event(tags: Vec<Vec<&str>>, content: &str) -> Event {
        let mut builder = EventBuilder::new(Kind::from(31990u16), content);
        for tag in tags {
            builder = builder.tag(Tag::parse(tag).unwrap());
        }
        let keys = Keys::generate();
        let unsigned = builder.build(keys.public_key());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { keys.sign_event(unsigned).await.unwrap() })
    }

    #[test]
    fn test_parses_kinds_and_alt_tag_name() {
        let event = handler_event(
            vec![vec!["alt", "MyApp"], vec!["k", "30023"], vec!["k", "31234"]],
            "",
        );
        let (kinds, app) = parse_recommended_app(&event).unwrap();
        assert_eq!(app, "MyApp");
        assert_eq!(
            kinds.as_ref(),
            &[Kind::from(30023u16), Kind::from(31234u16)]
        );
    }

    #[test]
    fn test_falls_back_to_content_name_fields() {
        let event = handler_event(
            vec![vec!["k", "1063"]],
            r#"{"name":"ContentApp","about":"x"}"#,
        );
        let (kinds, app) = parse_recommended_app(&event).unwrap();
        assert_eq!(app, "ContentApp");
        assert_eq!(kinds.as_ref(), &[Kind::from(1063u16)]);
    }

    #[test]
    fn test_skips_unparseable_kind_tags() {
        let event = handler_event(vec![vec!["alt", "App"], vec!["k", "not-a-kind"]], "");
        let (kinds, _) = parse_recommended_app(&event).unwrap();
        assert!(kinds.is_empty());
    }
}
