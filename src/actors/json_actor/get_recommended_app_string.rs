use nostr_sdk::prelude::*;
use serde_json::Value;

/// Reject blank or whitespace-only name candidates.
fn usable_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parse recommended app information from NIP-89 event.
///
/// Extracts app name and supported kinds from a NIP-89 handler event.
/// Name precedence is content display_name/name, then the alt tag, then
/// the website URL: in a 348-event sample of live kind-31990 events the
/// alt was always a NIP-31 description ("Nostr App: nostrudel") wherever
/// a content name existed, and no event carried an alt without one.
/// The name is `None` when no source carries a usable value — callers
/// must persist nothing rather than a sentinel.
///
/// # Returns
///
/// Returns tuple of (supported_kinds, app_name).
pub fn parse_recommended_app(app_event: &Event) -> (Box<[Kind]>, Option<String>) {
    let alt_tag_data = app_event.tags.iter().find_map(|tag| {
        // Look for alt tags using tag name
        if tag.as_slice().first() == Some(&"alt".to_string()) {
            return tag.as_slice().get(1).and_then(|v| usable_name(v));
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

    // Unparseable content (plain text in the wild) is treated as absent
    // rather than an error so the alt tag can still name the app.
    let content_json = serde_json::from_str::<Value>(&app_event.content).ok();
    let content_name = content_json.as_ref().and_then(|json| {
        [&json["display_name"], &json["name"]]
            .iter()
            .find_map(|value| value.as_str().and_then(usable_name))
    });
    let website = content_json
        .as_ref()
        .and_then(|json| json["website"].as_str().and_then(usable_name));

    (kind_tag_data, content_name.or(alt_tag_data).or(website))
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
        let (kinds, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("MyApp"));
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
        let (kinds, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("ContentApp"));
        assert_eq!(kinds.as_ref(), &[Kind::from(1063u16)]);
    }

    #[test]
    fn test_skips_unparseable_kind_tags() {
        let event = handler_event(vec![vec!["alt", "App"], vec!["k", "not-a-kind"]], "");
        let (kinds, _) = parse_recommended_app(&event);
        assert!(kinds.is_empty());
    }

    #[test]
    fn test_plain_text_content_falls_through_to_alt_tag() {
        // NIP-89 handlers in the wild carry plain-text content; that must
        // not discard the event when the alt tag still names the app.
        let event = handler_event(
            vec![vec!["alt", "AltApp"], vec!["k", "1063"]],
            "Check out my cool nostr app!",
        );
        let (kinds, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("AltApp"));
        assert_eq!(kinds.as_ref(), &[Kind::from(1063u16)]);
    }

    #[test]
    fn test_prefers_human_readable_name_over_website() {
        let event = handler_event(
            vec![vec!["k", "1063"]],
            r#"{"website":"https://app.example","display_name":"Display Name","name":"plainname"}"#,
        );
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("Display Name"));

        let event = handler_event(
            vec![vec!["k", "1063"]],
            r#"{"website":"https://app.example","name":"plainname"}"#,
        );
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("plainname"));

        // Website alone is still better than nothing
        let event = handler_event(
            vec![vec!["k", "1063"]],
            r#"{"website":"https://app.example"}"#,
        );
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("https://app.example"));
    }

    #[test]
    fn test_prefers_content_name_over_alt_description() {
        // Sampled real 31990 events: alt is a NIP-31 description ("Nostr
        // App: nostrudel") while content metadata carries the proper name.
        let event = handler_event(
            vec![vec!["alt", "Nostr App: nostrudel"], vec!["k", "1063"]],
            r#"{"name":"noStrudel"}"#,
        );
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("noStrudel"));
    }

    #[test]
    fn test_alt_tag_beats_website_url() {
        // A descriptive alt still labels better than a bare URL.
        let event = handler_event(
            vec![vec!["alt", "ZapClock"], vec!["k", "1063"]],
            r#"{"website":"https://app.example"}"#,
        );
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app.as_deref(), Some("ZapClock"));
    }

    #[test]
    fn test_returns_none_without_any_usable_name() {
        // No sentinel string: absence is None so nothing gets persisted.
        let event = handler_event(vec![vec!["k", "1063"]], "not json at all");
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app, None);

        let event = handler_event(vec![vec!["k", "1063"]], r#"{"name":"  "}"#);
        let (_, app) = parse_recommended_app(&event);
        assert_eq!(app, None, "blank names are as useless as missing ones");
    }
}
