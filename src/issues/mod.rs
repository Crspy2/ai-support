pub mod hooks;
#[allow(unused_imports)]
pub use hooks::{IssueAcceptedHook, IssueEndedHook, IssueProposedHook, IssueRejectedHook};

use std::sync::Arc;

use anyhow::{Context, Result};
use async_openai::{
    Client as OpenAIClient,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs,
    },
};
use pgvector::Vector;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::Config;
use crate::extensions::ExtensionRegistry;
use crate::knowledge::embed::embed_text;

const SIGNAL_WINDOW_MINUTES: f64 = 30.0;
const CLUSTER_THRESHOLD: i64 = 3;
const DEDUP_DISTANCE: f64 = 0.15;

pub struct IssueTracker {
    pool: Arc<PgPool>,
    openai: Arc<OpenAIClient<OpenAIConfig>>,
    config: Arc<Config>,
    registry: Arc<ExtensionRegistry>,
    reqwest: reqwest::Client,
}

impl IssueTracker {
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

    pub async fn record_signal(&self, user_id: &str, content: &str) -> Result<()> {
        let embedding = embed_text(&self.openai, content).await?;
        let vector = Vector::from(embedding);

        sqlx::query(
            "INSERT INTO issue_signals (user_id, content, embedding) VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind(content)
        .bind(vector)
        .execute(self.pool.as_ref())
        .await?;

        self.check_and_propose().await
    }

    async fn check_and_propose(&self) -> Result<()> {
        // Step 1: count distinct users in window
        let user_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT user_id)::BIGINT FROM issue_signals \
             WHERE created_at >= now() - ($1 * INTERVAL '1 minute')",
        )
        .bind(SIGNAL_WINDOW_MINUTES)
        .fetch_one(self.pool.as_ref())
        .await?;

        if user_count < CLUSTER_THRESHOLD {
            return Ok(());
        }

        // Step 2: compute centroid of all embeddings in window
        let embeddings: Vec<Vector> = sqlx::query_scalar(
            "SELECT embedding FROM issue_signals \
             WHERE created_at >= now() - ($1 * INTERVAL '1 minute')",
        )
        .bind(SIGNAL_WINDOW_MINUTES)
        .fetch_all(self.pool.as_ref())
        .await?;

        let n = embeddings.len();
        if n == 0 {
            return Ok(());
        }

        let dim = embeddings[0].as_slice().len();
        let mut sum = vec![0.0f32; dim];
        for v in &embeddings {
            for (i, &x) in v.as_slice().iter().enumerate() {
                sum[i] += x;
            }
        }
        let mean_vec: Vec<f32> = sum.iter().map(|&x| x / n as f32).collect();
        let centroid = Vector::from(mean_vec);

        // Step 3: dedup check — skip if active issue already covers this topic
        let dedup_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM issues \
             WHERE status IN ('proposed', 'accepted') AND embedding <-> $1 < $2",
        )
        .bind(&centroid)
        .bind(DEDUP_DISTANCE)
        .fetch_one(self.pool.as_ref())
        .await?;

        if dedup_count > 0 {
            return Ok(());
        }

        // Step 4: summarize using 10 most recent messages
        let recent_contents: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM issue_signals \
             WHERE created_at >= now() - ($1 * INTERVAL '1 minute') \
             ORDER BY created_at DESC LIMIT 10",
        )
        .bind(SIGNAL_WINDOW_MINUTES)
        .fetch_all(self.pool.as_ref())
        .await?;

        let samples = recent_contents.join("\n");
        let summary = self.summarize_issue(&samples).await?;

        // Step 5: open DM channel with owner
        let channel_id = self.open_dm_channel().await?;

        // Step 6: send proposal DM
        let issue_id = Uuid::new_v4();
        let body = proposal_components(issue_id, &summary, user_count);
        let msg = self.discord_post_message(&channel_id, body).await?;
        let message_id = msg["id"]
            .as_str()
            .context("missing message id in Discord response")?
            .to_string();

        // Step 7: persist the proposed issue
        sqlx::query(
            "INSERT INTO issues \
             (id, summary, embedding, status, user_count, dm_channel_id, dm_message_id) \
             VALUES ($1, $2, $3, 'proposed', $4, $5, $6)",
        )
        .bind(issue_id)
        .bind(&summary)
        .bind(&centroid)
        .bind(user_count as i32)
        .bind(&channel_id)
        .bind(&message_id)
        .execute(self.pool.as_ref())
        .await?;

        tracing::info!("proposed issue {issue_id}: {summary}");
        self.registry
            .fire_hook(
                "issue::proposed",
                serde_json::to_value(hooks::IssueProposedHook {
                    issue_id,
                    summary,
                    user_count: user_count as i32,
                })?,
            )
            .await;
        Ok(())
    }

    async fn summarize_issue(&self, samples: &str) -> Result<String> {
        let request = CreateChatCompletionRequestArgs::default()
            .model(self.config.ai_model.as_str())
            .max_tokens(60u32)
            .messages(vec![
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(
                        "Summarize the common technical issue in one sentence (max 15 words).",
                    )
                    .build()?
                    .into(),
                ChatCompletionRequestUserMessageArgs::default()
                    .content(samples)
                    .build()?
                    .into(),
            ])
            .build()?;

        let response = self.openai.chat().create(request).await?;
        let summary = response
            .choices
            .into_iter()
            .next()
            .context("OpenAI returned no choices")?
            .message
            .content
            .unwrap_or_else(|| "Unknown issue".to_string());

        Ok(summary)
    }

    pub async fn handle_button(&self, custom_id: &str) -> Result<()> {
        let (action, uuid_str) = custom_id
            .split_once(':')
            .context("invalid custom_id format")?;
        let issue_id = Uuid::parse_str(uuid_str)?;

        let row: (Uuid, String, String, String) = sqlx::query_as(
            "SELECT id, summary, dm_channel_id, dm_message_id FROM issues WHERE id = $1",
        )
        .bind(issue_id)
        .fetch_one(self.pool.as_ref())
        .await?;

        let (id, summary, channel_id, message_id) = row;

        let (new_status, patch_body) = match action {
            "issue_accept" => ("accepted", accepted_components(id, &summary)),
            "issue_reject" => ("rejected", rejected_components(&summary)),
            "issue_end" => ("ended", ended_components(&summary)),
            _ => return Ok(()),
        };

        sqlx::query("UPDATE issues SET status = $1, updated_at = now() WHERE id = $2")
            .bind(new_status)
            .bind(id)
            .execute(self.pool.as_ref())
            .await?;

        self.discord_patch_message(&channel_id, &message_id, patch_body)
            .await?;

        match action {
            "issue_accept" => {
                self.registry
                    .fire_hook(
                        "issue::accepted",
                        serde_json::to_value(hooks::IssueAcceptedHook {
                            issue_id: id,
                            summary: summary.clone(),
                        })?,
                    )
                    .await;
            }
            "issue_reject" => {
                self.registry
                    .fire_hook(
                        "issue::rejected",
                        serde_json::to_value(hooks::IssueRejectedHook {
                            issue_id: id,
                            summary: summary.clone(),
                        })?,
                    )
                    .await;
            }
            "issue_end" => {
                self.registry
                    .fire_hook(
                        "issue::ended",
                        serde_json::to_value(hooks::IssueEndedHook {
                            issue_id: id,
                            summary,
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

fn proposal_components(issue_id: Uuid, summary: &str, user_count: i64) -> Value {
    json!({
        "flags": 32768,
        "components": [{ "type": 17, "components": [
            { "type": 10, "content": format!("**Potential active issue:** {summary}\n{user_count} users in the last 30 minutes") },
            { "type": 1, "components": [
                { "type": 2, "style": 3, "label": "Accept", "custom_id": format!("issue_accept:{issue_id}") },
                { "type": 2, "style": 4, "label": "Reject",  "custom_id": format!("issue_reject:{issue_id}") }
            ]}
        ]}]
    })
}

fn accepted_components(issue_id: Uuid, summary: &str) -> Value {
    json!({
        "flags": 32768,
        "components": [{ "type": 17, "components": [
            { "type": 10, "content": format!("**Issue is ongoing:** {summary}") },
            { "type": 1, "components": [
                { "type": 2, "style": 4, "label": "End Issue", "custom_id": format!("issue_end:{issue_id}") }
            ]}
        ]}]
    })
}

fn rejected_components(summary: &str) -> Value {
    json!({ "flags": 32768, "components": [{ "type": 17, "components": [
        { "type": 10, "content": format!("**Issue rejected:** {summary}") }
    ]}]})
}

fn ended_components(summary: &str) -> Value {
    json!({ "flags": 32768, "components": [{ "type": 17, "components": [
        { "type": 10, "content": format!("**Issue resolved:** {summary}") }
    ]}]})
}
