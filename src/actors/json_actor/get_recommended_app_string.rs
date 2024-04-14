use anyhow::Result;
use nostr_sdk::prelude::*;

pub fn parse_recommended_app(app_event: &Event) -> Result<(Box<[Kind]>, String)> {
    let alt_tag_data = app_event.tags().iter().find_map(|tag| match tag {
        Tag::Generic(TagKind::Custom(tag_name), data) if tag_name == "alt" && !data.is_empty() => {
            data.first().cloned()
        }
        _ => None,
    });

    let kind_tag_data: Box<[Kind]> = app_event
        .tags()
        .iter()
        .filter_map(|tag| match tag {
            Tag::Generic(TagKind::Custom(tag_name), data)
                if tag_name == "k" && !data.is_empty() =>
            {
                data.first()
                    .and_then(|kind_string| kind_string.parse::<u64>().ok().map(Kind::from))
            }
            _ => None,
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
