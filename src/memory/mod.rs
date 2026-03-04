pub mod hooks;
#[allow(unused_imports)]
pub use hooks::{MemoryApprovedHook, MemoryRejectedHook, MemoryRequestedHook};

use std::sync::Arc;

use anyhow::{Context, Result};
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use pgvector::Vector;
use serde_json::{Value, json};
use sqlx::PgPool;
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
    reqwest: reqwest::Client,
}

impl MemoryTracker {
    pub fn new(
        pool: Arc<PgPool>,
        openai: Arc<OpenAIClient<OpenAIConfig>>,
        config: Arc<Config>,
        registry: Arc<ExtensionRegistry>,
    ) -> Result<Self> {
        Ok(Self {
            pool,
            openai,
            config,
            registry,
            reqwest: reqwest::Client::new(),
        })
    }

    /// Called by call_openai when the AI invokes the request_memory tool.
    /// Args JSON: { content, summary, message_link }
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

        // Dedup: check if near-identical content already exists in knowledge_chunks
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

        let body = proposal_components(memory_id, &content, &summary, &message_link);
        let msg = self.discord_post_message(&channel_id, body).await?;
        let message_id = msg["id"]
            .as_str()
            .context("missing message id in Discord response")?
            .to_string();

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
        .bind(&channel_id)
        .bind(&message_id)
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

    /// Called from interactions.rs for memory_approve:uuid / memory_reject:uuid buttons.
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

        let (id, content, channel_id, message_id, embedding) = row;

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

                self.discord_patch_message(&channel_id, &message_id, approved_components(&content))
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

                self.discord_patch_message(&channel_id, &message_id, rejected_components(&content))
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

    async fn open_dm_channel(&self) -> Result<String> {
        let response = self
            .reqwest
            .post("https://discord.com/api/v10/users/@me/channels")
            .header(
                "Authorization",
                format!("Bot {}", self.config.discord_token),
            )
            .json(&json!({ "recipient_id": self.config.owner_id }))
            .send()
            .await?
            .json::<Value>()
            .await?;

        response["id"]
            .as_str()
            .context("missing channel id in DM open response")
            .map(|s| s.to_string())
    }

    async fn discord_post_message(&self, channel_id: &str, body: Value) -> Result<Value> {
        let response = self
            .reqwest
            .post(format!(
                "https://discord.com/api/v10/channels/{channel_id}/messages"
            ))
            .header(
                "Authorization",
                format!("Bot {}", self.config.discord_token),
            )
            .json(&body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        Ok(response)
    }

    async fn discord_patch_message(
        &self,
        channel_id: &str,
        message_id: &str,
        body: Value,
    ) -> Result<()> {
        self.reqwest
            .patch(format!(
                "https://discord.com/api/v10/channels/{channel_id}/messages/{message_id}"
            ))
            .header(
                "Authorization",
                format!("Bot {}", self.config.discord_token),
            )
            .json(&body)
            .send()
            .await?;

        Ok(())
    }
}

fn proposal_components(memory_id: Uuid, content: &str, summary: &str, message_link: &str) -> Value {
    json!({
        "flags": 32768,
        "components": [{ "type": 17, "components": [
            { "type": 10, "content": format!(
                "**Memory request**\n\n**Summary:** {summary}\n\n**Content:** {content}\n\n[Jump to message]({message_link})"
            )},
            { "type": 1, "components": [
                { "type": 2, "style": 3, "label": "Approve", "custom_id": format!("memory_approve:{memory_id}") },
                { "type": 2, "style": 4, "label": "Reject",  "custom_id": format!("memory_reject:{memory_id}") }
            ]}
        ]}]
    })
}

fn approved_components(content: &str) -> Value {
    json!({
        "flags": 32768,
        "components": [{ "type": 17, "components": [
            { "type": 10, "content": format!("✅ Memory approved — added to knowledge base.\n\n**Content:** {content}") }
        ]}]
    })
}

fn rejected_components(content: &str) -> Value {
    json!({
        "flags": 32768,
        "components": [{ "type": 17, "components": [
            { "type": 10, "content": format!("❌ Memory rejected.\n\n**Content:** {content}") }
        ]}]
    })
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
