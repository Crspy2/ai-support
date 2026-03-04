pub mod hooks;
#[allow(unused_imports)]
pub use hooks::{MemoryApprovedHook, MemoryRejectedHook, MemoryRequestedHook};

use std::sync::Arc;

use anyhow::{Context, Result};
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use pgvector::Vector;
use serde_json::Value;
use sqlx::PgPool;
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, Container, TextDisplay,
};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, MessageMarker, UserMarker};
use uuid::Uuid;

use crate::config::Config;
use crate::extensions::{ExtensionRegistry, HookEvent};
use crate::knowledge::embed::embed_text;

const DEDUP_DISTANCE: f64 = 0.25;

pub struct MemoryTracker {
    pool: Arc<PgPool>,
    openai: Arc<OpenAIClient<OpenAIConfig>>,
    config: Arc<Config>,
    registry: Arc<ExtensionRegistry>,
    http: Arc<HttpClient>,
}

impl MemoryTracker {
    pub fn new(
        pool: Arc<PgPool>,
        openai: Arc<OpenAIClient<OpenAIConfig>>,
        config: Arc<Config>,
        registry: Arc<ExtensionRegistry>,
        http: Arc<HttpClient>,
    ) -> Result<Self> {
        Ok(Self { pool, openai, config, registry, http })
    }

    pub async fn request(&self, args: Value) -> String {
        match self.request_inner(args).await {
            Ok(msg) => msg,
            Err(e) => {
                tracing::error!("memory request failed: {e:#}");
                "Memory request failed due to an internal error.".to_string()
            }
        }
    }

    async fn request_inner(&self, args: Value) -> Result<String> {
        let content = args["content"]
            .as_str()
            .context("missing content")?
            .to_string();
        let summary = args["summary"]
            .as_str()
            .context("missing summary")?
            .to_string();
        let message_link = args["message_link"]
            .as_str()
            .context("missing message_link")?
            .to_string();

        let embedding = embed_text(&self.openai, &content).await?;
        let vector = Vector::from(embedding);

        let already_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM knowledge_chunks WHERE embedding <-> $1 < $2)",
        )
        .bind(&vector)
        .bind(DEDUP_DISTANCE)
        .fetch_one(self.pool.as_ref())
        .await?;

        if already_exists {
            return Ok("This information is already in the knowledge base.".to_string());
        }

        let memory_id = Uuid::new_v4();
        let channel_id = self.open_dm_channel().await?;

        let msg = self
            .discord_post_message(
                channel_id,
                &proposal_components(memory_id, &content, &summary, &message_link),
            )
            .await?;

        sqlx::query(
            "INSERT INTO pending_memories \
             (id, content, summary, message_link, embedding, dm_channel_id, dm_message_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(memory_id)
        .bind(&content)
        .bind(&summary)
        .bind(&message_link)
        .bind(&vector)
        .bind(channel_id.to_string())
        .bind(msg.id.to_string())
        .execute(self.pool.as_ref())
        .await?;

        tracing::info!("memory request {memory_id}: {summary}");
        self.registry
            .fire_hook(
                HookEvent::MemoryRequested,
                serde_json::to_value(hooks::MemoryRequestedHook {
                    memory_id,
                    content,
                    summary,
                    message_link,
                })?,
            )
            .await;

        Ok("Memory request sent to the owner for review.".to_string())
    }

    pub async fn handle_button(&self, custom_id: &str) -> Result<()> {
        let (action, uuid_str) = custom_id
            .split_once(':')
            .context("invalid custom_id format")?;
        let memory_id = Uuid::parse_str(uuid_str)?;

        let row: (Uuid, String, String, String, Vector) = sqlx::query_as(
            "SELECT id, content, dm_channel_id, dm_message_id, embedding \
             FROM pending_memories WHERE id = $1",
        )
        .bind(memory_id)
        .fetch_one(self.pool.as_ref())
        .await?;

        let (id, content, channel_id_str, message_id_str, embedding) = row;
        let channel_id = Id::<ChannelMarker>::new(channel_id_str.parse()?);
        let message_id = Id::<MessageMarker>::new(message_id_str.parse()?);

        match action {
            "memory_approve" => {
                let content_hash = sha256_hex(&content);
                let source = format!("memory::{id}");
                sqlx::query(
                    "INSERT INTO knowledge_chunks (source, content, content_hash, embedding) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (source) DO UPDATE \
                     SET content = EXCLUDED.content, \
                         content_hash = EXCLUDED.content_hash, \
                         embedding = EXCLUDED.embedding",
                )
                .bind(&source)
                .bind(&content)
                .bind(&content_hash)
                .bind(&embedding)
                .execute(self.pool.as_ref())
                .await?;

                sqlx::query(
                    "UPDATE pending_memories SET status = 'approved', updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .execute(self.pool.as_ref())
                .await?;

                self.discord_patch_message(channel_id, message_id, &approved_components(&content))
                    .await?;

                self.registry
                    .fire_hook(
                        HookEvent::MemoryApproved,
                        serde_json::to_value(hooks::MemoryApprovedHook {
                            memory_id: id,
                            content,
                        })?,
                    )
                    .await;
            }
            "memory_reject" => {
                sqlx::query(
                    "UPDATE pending_memories SET status = 'rejected', updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .execute(self.pool.as_ref())
                .await?;

                self.discord_patch_message(channel_id, message_id, &rejected_components(&content))
                    .await?;

                self.registry
                    .fire_hook(
                        HookEvent::MemoryRejected,
                        serde_json::to_value(hooks::MemoryRejectedHook {
                            memory_id: id,
                            content,
                        })?,
                    )
                    .await;
            }
            _ => {}
        }

        Ok(())
    }

    async fn open_dm_channel(&self) -> Result<Id<ChannelMarker>> {
        let user_id = Id::<UserMarker>::new(self.config.owner_id.parse()?);
        let channel = self.http.create_private_channel(user_id).await?.model().await?;
        Ok(channel.id)
    }

    async fn discord_post_message(
        &self,
        channel_id: Id<ChannelMarker>,
        components: &[Component],
    ) -> Result<twilight_model::channel::Message> {
        let msg = self
            .http
            .create_message(channel_id)
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(components)
            .await?
            .model()
            .await?;
        Ok(msg)
    }

    async fn discord_patch_message(
        &self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        components: &[Component],
    ) -> Result<()> {
        self.http
            .update_message(channel_id, message_id)
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(Some(components))
            .await?;
        Ok(())
    }
}

fn proposal_components(
    memory_id: Uuid,
    content: &str,
    summary: &str,
    message_link: &str,
) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x5865F2)),
        spoiler: None,
        components: vec![
            Component::TextDisplay(TextDisplay {
                id: None,
                content: format!(
                    "## Memory Proposal\n{summary}\n\n**Content**\n{content}\n\n[Jump to message]({message_link})"
                ),
            }),
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(format!("memory_approve:{memory_id}")),
                        disabled: false,
                        emoji: None,
                        label: Some("Approve".to_string()),
                        style: ButtonStyle::Success,
                        url: None,
                        sku_id: None,
                    }),
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(format!("memory_reject:{memory_id}")),
                        disabled: false,
                        emoji: None,
                        label: Some("Reject".to_string()),
                        style: ButtonStyle::Danger,
                        url: None,
                        sku_id: None,
                    }),
                ],
            }),
        ],
    })]
}

fn approved_components(content: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x57F287)),
        spoiler: None,
        components: vec![Component::TextDisplay(TextDisplay {
            id: None,
            content: format!("## Memory Added\n{content}"),
        })],
    })]
}

fn rejected_components(content: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0xED4245)),
        spoiler: None,
        components: vec![Component::TextDisplay(TextDisplay {
            id: None,
            content: format!("## Memory Dismissed\n{content}"),
        })],
    })]
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
