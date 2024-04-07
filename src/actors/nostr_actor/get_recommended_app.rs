use anyhow::Result;
use nostr_sdk::prelude::*;
use ractor::concurrency::Duration;

pub async fn get_recommended_app(client: &Client, kind: Kind) -> Result<String> {
    let recommended_app_event = recommended_app_event(client, kind).await?;

    match recommended_app_event {
        Some(app_event) => {
            let alt_tag_data = app_event.tags().iter().find_map(|tag| match tag {
                Tag::Generic(TagKind::Custom(tag_name), data)
                    if tag_name == "alt" && !data.is_empty() =>
                {
                    data.first().cloned() // Prefer `cloned()` over `map(|s| s.clone())`
                }
                _ => None,
            });

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

            Ok(alt_tag_data
                .or(content_value)
                .unwrap_or_else(|| "None found".to_string()))
        }
        None => Ok("None found".to_string()),
    }
}

async fn recommended_app_event(client: &Client, kind: Kind) -> Result<Option<Event>> {
    let filters = vec![Filter::new()
        .limit(1)
        .kind(Kind::ParameterizedReplaceable(31990))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::K), [kind.to_string()])];
    let recommended_apps = client
        .get_events_of(filters, Some(Duration::from_secs(1)))
        .await?;
    Ok(recommended_apps.first().cloned())
}
