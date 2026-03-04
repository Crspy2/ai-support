use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use dashmap::DashMap;
use serde_json::{Value, json};
use sqlx::PgPool;
use twilight_http::Client as HttpClient;
use twilight_model::application::interaction::modal::{
    ModalInteractionActionRow, ModalInteractionComponent, ModalInteractionData,
    ModalInteractionLabel,
};
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, Container, Label, Separator, TextDisplay, TextInput,
    TextInputStyle,
};
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, MessageMarker};
use uuid::Uuid;

use crate::extensions::ExtensionRegistry;

pub struct InfoField {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub required: bool,
    pub style: TextInputStyle,
    pub cache: bool,
    pub cache_ttl_hours: Option<u64>,
}

pub struct PendingInfoRequest {
    pub id: Uuid,
    pub target_user_id: String,
    pub channel_id: Id<ChannelMarker>,
    pub reply_to_msg_id: Id<MessageMarker>,
    pub title: String,
    pub message: String,
    pub fields: Vec<InfoField>,
    pub ext_name: String,
    pub method_name: String,
    pub known_args: Value,
}

/// Internal struct stored in the DashMap — extends PendingInfoRequest with the
/// ID of the button message so it can be edited after modal submission.
struct StoredInfoRequest {
    request: PendingInfoRequest,
    button_msg_id: Id<MessageMarker>,
}

pub struct InfoCollector {
    pending: DashMap<Uuid, StoredInfoRequest>,
    pool: Arc<PgPool>,
    http: Arc<HttpClient>,
    registry: Arc<ExtensionRegistry>,
    #[allow(dead_code)]
    application_id: Id<ApplicationMarker>,
}

impl InfoCollector {
    pub fn new(
        pool: Arc<PgPool>,
        http: Arc<HttpClient>,
        registry: Arc<ExtensionRegistry>,
        application_id: Id<ApplicationMarker>,
    ) -> Self {
        Self {
            pending: DashMap::new(),
            pool,
            http,
            registry,
            application_id,
        }
    }

    /// Check cache, then either call action directly (returns false) or post button and store (returns true).
    pub async fn initiate(&self, req: PendingInfoRequest) -> Result<bool> {
        let mut cached_values: HashMap<String, String> = HashMap::new();

        for field in &req.fields {
            if field.cache {
                let result: Option<String> = sqlx::query_scalar(
                    "SELECT value FROM user_info \
                     WHERE user_id = $1 AND key = $2 \
                     AND (expires_at IS NULL OR expires_at > now())",
                )
                .bind(&req.target_user_id)
                .bind(&field.id)
                .fetch_optional(self.pool.as_ref())
                .await?;

                if let Some(v) = result {
                    cached_values.insert(field.id.clone(), v);
                }
            }
        }

        let needs_modal = req
            .fields
            .iter()
            .any(|f| !f.cache || !cached_values.contains_key(&f.id));

        if !needs_modal {
            let mut args = if req.known_args.is_object() {
                req.known_args.clone()
            } else {
                json!({})
            };
            if let Some(obj) = args.as_object_mut() {
                for (key, val) in &cached_values {
                    obj.insert(key.clone(), Value::String(val.clone()));
                }
            }

            let result =
                match self.registry.call_action(&req.ext_name, &req.method_name, args).await {
                    Ok(r) => r,
                    Err(e) => format!("Action failed: {e:#}"),
                };

            self.http
                .create_message(req.channel_id)
                .reply(req.reply_to_msg_id)
                .flags(MessageFlags::IS_COMPONENTS_V2)
                .components(&[Component::TextDisplay(TextDisplay {
                    id: None,
                    content: result,
                })])
                .await?;

            return Ok(false);
        }

        let components = info_button_components(req.id, &req.message);
        let button_msg = self
            .http
            .create_message(req.channel_id)
            .reply(req.reply_to_msg_id)
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(&components)
            .await?
            .model()
            .await?;

        self.pending.insert(req.id, StoredInfoRequest {
            button_msg_id: button_msg.id,
            request: req,
        });
        Ok(true)
    }

    /// Build modal response JSON synchronously. Called from the interaction handler on type 3.
    pub fn build_modal_response(&self, custom_id: &str, interaction_user_id: &str) -> Value {
        let uuid_str = match custom_id.strip_prefix("info_collect:") {
            Some(s) => s,
            None => return ephemeral_error("Invalid button."),
        };

        let uuid = match Uuid::parse_str(uuid_str) {
            Ok(u) => u,
            Err(_) => return ephemeral_error("Invalid button ID."),
        };

        let stored = match self.pending.get(&uuid) {
            Some(s) => s,
            None => return ephemeral_error("This request has expired."),
        };

        if stored.request.target_user_id != interaction_user_id {
            return ephemeral_error("This button is not for you.");
        }

        #[allow(deprecated)]
        let components: Vec<Component> = stored
            .request
            .fields
            .iter()
            .map(|f| {
                Component::Label(Label {
                    id: None,
                    label: f.label.clone(),
                    description: f.description.clone(),
                    component: Box::new(Component::TextInput(TextInput {
                        id: None,
                        custom_id: f.id.clone(),
                        label: None,
                        style: f.style,
                        placeholder: f.placeholder.clone(),
                        required: Some(f.required),
                        min_length: None,
                        max_length: None,
                        value: None,
                    })),
                })
            })
            .collect();

        let response = InteractionResponse {
            kind: InteractionResponseType::Modal,
            data: Some(InteractionResponseData {
                custom_id: Some(format!("info_submit:{uuid}")),
                title: Some(stored.request.title.clone()),
                components: Some(components),
                ..Default::default()
            }),
        };

        serde_json::to_value(&response)
            .unwrap_or_else(|_| ephemeral_error("Failed to build modal."))
    }

    /// Handle modal submission (type 5). Extracts values, upserts cache, calls action,
    /// collapses the button message, posts result as a reply with ping, then deletes after 10s.
    pub async fn handle_modal_submit(&self, interaction: Value) -> Result<()> {
        let custom_id = interaction["data"]["custom_id"]
            .as_str()
            .context("missing custom_id")?;

        let uuid_str = custom_id
            .strip_prefix("info_submit:")
            .context("not an info_submit interaction")?;

        let uuid = Uuid::parse_str(uuid_str)?;

        let (_, stored) = self
            .pending
            .remove(&uuid)
            .context("pending request not found or already processed")?;

        let StoredInfoRequest { request: pending, button_msg_id } = stored;

        let modal_data: ModalInteractionData =
            serde_json::from_value(interaction["data"].clone())?;

        let submitted = extract_text_inputs(&modal_data.components);

        let mut args = if pending.known_args.is_object() {
            pending.known_args.clone()
        } else {
            json!({})
        };

        if let Some(obj) = args.as_object_mut() {
            for field in &pending.fields {
                if let Some(value) = submitted.get(&field.id) {
                    obj.insert(field.id.clone(), Value::String(value.clone()));

                    if field.cache {
                        match field.cache_ttl_hours {
                            Some(hours) => {
                                sqlx::query(
                                    "INSERT INTO user_info (user_id, key, value, expires_at) \
                                     VALUES ($1, $2, $3, now() + ($4::float8 * interval '1 hour')) \
                                     ON CONFLICT (user_id, key) DO UPDATE \
                                     SET value = EXCLUDED.value, \
                                         expires_at = EXCLUDED.expires_at, \
                                         updated_at = now()",
                                )
                                .bind(&pending.target_user_id)
                                .bind(&field.id)
                                .bind(value)
                                .bind(hours as f64)
                                .execute(self.pool.as_ref())
                                .await?;
                            }
                            None => {
                                sqlx::query(
                                    "INSERT INTO user_info (user_id, key, value, expires_at) \
                                     VALUES ($1, $2, $3, NULL) \
                                     ON CONFLICT (user_id, key) DO UPDATE \
                                     SET value = EXCLUDED.value, \
                                         expires_at = NULL, \
                                         updated_at = now()",
                                )
                                .bind(&pending.target_user_id)
                                .bind(&field.id)
                                .bind(value)
                                .execute(self.pool.as_ref())
                                .await?;
                            }
                        }
                    }
                }
            }
        }

        let result =
            match self.registry.call_action(&pending.ext_name, &pending.method_name, args).await {
                Ok(r) => r,
                Err(e) => format!("Action failed: {e:#}"),
            };

        // Collapse the button message to a bare separator so it disappears visually.
        let _ = self
            .http
            .update_message(pending.channel_id, button_msg_id)
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(Some(&[Component::Separator(Separator {
                id: None,
                divider: Some(false),
                spacing: None,
            })]))
            .await;

        // Post the result as a reply to the (now-collapsed) button message, pinging the user.
        let result_msg = self
            .http
            .create_message(pending.channel_id)
            .reply(button_msg_id)
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(&[Component::TextDisplay(TextDisplay {
                id: None,
                content: format!("<@{}> {}", pending.target_user_id, result),
            })])
            .await?
            .model()
            .await?;

        // Delete the result message after 10 seconds so the ping doesn't linger.
        let http = Arc::clone(&self.http);
        let channel_id = pending.channel_id;
        let msg_id = result_msg.id;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let _ = http.delete_message(channel_id, msg_id).await;
        });

        Ok(())
    }
}

fn extract_text_inputs(components: &[ModalInteractionComponent]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for c in components {
        match c {
            ModalInteractionComponent::TextInput(t) => {
                map.insert(t.custom_id.clone(), t.value.clone());
            }
            ModalInteractionComponent::ActionRow(ModalInteractionActionRow { components, .. }) => {
                map.extend(extract_text_inputs(components));
            }
            ModalInteractionComponent::Label(ModalInteractionLabel { component, .. }) => {
                map.extend(extract_text_inputs(std::slice::from_ref(component.as_ref())));
            }
            _ => {}
        }
    }
    map
}

fn info_button_components(request_id: Uuid, message: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x5865F2)),
        spoiler: None,
        components: vec![
            Component::TextDisplay(TextDisplay {
                id: None,
                content: format!("## Information Needed\n{message}"),
            }),
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![Component::Button(Button {
                    id: None,
                    custom_id: Some(format!("info_collect:{request_id}")),
                    disabled: false,
                    emoji: None,
                    label: Some("Provide Information".to_string()),
                    style: ButtonStyle::Primary,
                    url: None,
                    sku_id: None,
                })],
            }),
        ],
    })]
}

fn ephemeral_error(msg: &str) -> Value {
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(msg.to_string()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    serde_json::to_value(&response).unwrap_or_else(|_| json!({ "type": 4 }))
}
