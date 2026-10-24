//! The preference center: `GET` and `POST /u/{token}/preferences`.
//!
//! The page lists the workspace's email topics and the values of its dimensions with what the
//! person has now, and saves the choices through the same consent write the unsubscribe uses
//! (source `unsubscribe_link`). A topic is a choice about that topic; a dimension value is a choice
//! about the message's topic for that value alone, so leaving one vertical blocks only that
//! vertical. The page shows the names the marketer chose and nothing about the person.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use hashbrown::HashSet;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_core::consent::{Channel, Dimensions};
use zirv_flow_store::{
    PreferenceChange, ReadOutcome, StoreError, TrackedMessage, begin, consent_read,
    list_dimensions, list_topics, save_preferences,
};

use super::admit_message;
use super::pages::{self, Choice, Group};
use crate::AppState;
use crate::trace::request_trace;

/// The scope one choice writes: the topic and dimension values.
type Scope = (Option<Uuid>, Dimensions);

/// What the page shows, and for each choice the scope it writes, in the same order.
struct Loaded {
    groups: Vec<Group>,
    scopes: Vec<Vec<Scope>>,
}

async fn allowed(
    state: &AppState,
    message: &TrackedMessage,
    topic: Option<Uuid>,
    dimensions: &Dimensions,
) -> Result<bool, StoreError> {
    let read = consent_read(
        &state.pool,
        message.workspace(),
        message.person_id,
        Channel::Email,
        topic,
        dimensions,
    )
    .await?;
    Ok(matches!(read, ReadOutcome::Read { verdict, .. } if verdict.allowed))
}

async fn load(state: &AppState, message: &TrackedMessage) -> Result<Loaded, StoreError> {
    let workspace = message.workspace();
    let mut groups = Vec::new();
    let mut scopes = Vec::new();

    let mut choices = Vec::new();
    let mut topic_scopes = Vec::new();
    for topic in list_topics(&state.pool, workspace, false).await? {
        if topic.channel != Channel::Email {
            continue;
        }
        let scope = (Some(topic.id), Dimensions::new());
        choices.push(Choice {
            field: "topic",
            value: topic.key.clone(),
            label: topic.name.clone(),
            checked: allowed(state, message, scope.0, &scope.1).await?,
        });
        topic_scopes.push(scope);
    }
    groups.push(Group {
        title: "Topics".to_owned(),
        choices,
    });
    scopes.push(topic_scopes);

    let mut connection = begin(&state.pool).await?;
    let dimensions = list_dimensions(&mut connection, workspace, false).await?;
    drop(connection);
    for dimension in dimensions
        .into_iter()
        .filter(|dimension| !dimension.builtin)
    {
        let mut choices = Vec::new();
        let mut dimension_scopes = Vec::new();
        for value in &dimension.values {
            let mut selected = Dimensions::new();
            selected.insert(dimension.key.clone(), value.key.clone());
            let scope = (message.topic_id, selected);
            choices.push(Choice {
                field: "dim",
                value: format!("{}/{}", dimension.key, value.key),
                label: value.name.clone(),
                checked: allowed(state, message, scope.0, &scope.1).await?,
            });
            dimension_scopes.push(scope);
        }
        groups.push(Group {
            title: dimension.name.clone(),
            choices,
        });
        scopes.push(dimension_scopes);
    }
    Ok(Loaded { groups, scopes })
}

/// `GET /u/{token}/preferences`.
pub(crate) async fn preferences_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    let message = match admit_message(&state, &token).await {
        Ok(message) => message,
        Err(response) => return *response,
    };
    match load(&state, &message).await {
        Ok(loaded) => pages::preferences(&token, &loaded.groups, false),
        Err(error) => {
            warn!(%error, "the preference page could not be read");
            pages::unavailable()
        }
    }
}

/// The values of the form's checked boxes, as `field=value` strings.
fn checked_boxes(body: &[u8]) -> HashSet<String> {
    url::form_urlencoded::parse(body)
        .filter(|(name, _)| name == "topic" || name == "dim")
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

/// `POST /u/{token}/preferences`: records every choice that differs from what is held, together.
pub(crate) async fn preferences_save(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let message = match admit_message(&state, &token).await {
        Ok(message) => message,
        Err(response) => return *response,
    };
    let result = async {
        let loaded = load(&state, &message).await?;
        let checked = checked_boxes(&body);
        let mut changes = Vec::new();
        for (group, scopes) in loaded.groups.iter().zip(&loaded.scopes) {
            for (choice, (topic, dimensions)) in group.choices.iter().zip(scopes) {
                let wanted = checked.contains(&format!("{}={}", choice.field, choice.value));
                if wanted != choice.checked {
                    changes.push(PreferenceChange {
                        topic: *topic,
                        dimensions: dimensions.clone(),
                        subscribed: wanted,
                    });
                }
            }
        }
        let trace = request_trace();
        let saved = save_preferences(
            &state.pool,
            &message,
            &changes,
            state.clock.now_in_process(),
            &trace,
        )
        .await?;
        let reloaded = load(&state, &message).await?;
        Ok::<_, StoreError>((reloaded, !saved.unavailable))
    }
    .await;
    match result {
        Ok((loaded, saved)) => pages::preferences(&token, &loaded.groups, saved),
        Err(error) => {
            warn!(%error, "preferences could not be saved");
            pages::unavailable()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_topic_and_dim_fields_count_as_checked_boxes() {
        let checked = checked_boxes(b"topic=news&dim=vertical%2Fb2b&other=1&topic=offers");

        assert_eq!(checked.len(), 3);
        assert!(checked.contains("topic=news"));
        assert!(checked.contains("topic=offers"));
        assert!(checked.contains("dim=vertical/b2b"));
    }
}
