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

const SIGNAL_WINDOW_MINUTES: f64 = 30.0;
const CLUSTER_THRESHOLD: i64 = 3;
const DEDUP_DISTANCE: f64 = 0.15;

pub struct IssueTracker {
    pool: Arc<PgPool>,
    openai: Arc<OpenAIClient<OpenAIConfig>>,
    config: Arc<Config>,
    registry: Arc<ExtensionRegistry>,
    http: Arc<HttpClient>,
}

impl IssueTracker {
    pub fn new(
        pool: Arc<PgPool>,
        openai: Arc<OpenAIClient<OpenAIConfig>>,
        config: Arc<Config>,
        registry: Arc<ExtensionRegistry>,
        http: Arc<HttpClient>,
    ) -> Result<Self> {
        Ok(Self { pool, openai, config, registry, http })
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

        let channel_id = self.open_dm_channel().await?;

        let issue_id = Uuid::new_v4();
        let msg = self
            .discord_post_message(channel_id, &proposal_components(issue_id, &summary, user_count))
            .await?;

        sqlx::query(
            "INSERT INTO issues \
             (id, summary, embedding, status, user_count, dm_channel_id, dm_message_id) \
             VALUES ($1, $2, $3, 'proposed', $4, $5, $6)",
        )
        .bind(issue_id)
        .bind(&summary)
        .bind(&centroid)
        .bind(user_count as i32)
        .bind(channel_id.to_string())
        .bind(msg.id.to_string())
        .execute(self.pool.as_ref())
        .await?;

        tracing::info!("proposed issue {issue_id}: {summary}");
        self.registry
            .fire_hook(
                HookEvent::IssueProposed,
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

        let (id, summary, channel_id_str, message_id_str) = row;
        let channel_id = Id::<ChannelMarker>::new(channel_id_str.parse()?);
        let message_id = Id::<MessageMarker>::new(message_id_str.parse()?);

        let (new_status, patch_components) = match action {
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

        self.discord_patch_message(channel_id, message_id, &patch_components)
            .await?;

        match action {
            "issue_accept" => {
                self.registry
                    .fire_hook(
                        HookEvent::IssueAccepted,
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
                        HookEvent::IssueRejected,
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
                        HookEvent::IssueEnded,
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

fn proposal_components(issue_id: Uuid, summary: &str, user_count: i64) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x5865F2)),
        spoiler: None,
        components: vec![
            Component::TextDisplay(TextDisplay {
                id: None,
                content: format!(
                    "## Issue Detected\n{summary}\n\n{user_count} users affected in the last 30 minutes"
                ),
            }),
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(format!("issue_accept:{issue_id}")),
                        disabled: false,
                        emoji: None,
                        label: Some("Accept".to_string()),
                        style: ButtonStyle::Success,
                        url: None,
                        sku_id: None,
                    }),
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(format!("issue_reject:{issue_id}")),
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

fn accepted_components(issue_id: Uuid, summary: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x57F287)),
        spoiler: None,
        components: vec![
            Component::TextDisplay(TextDisplay {
                id: None,
                content: format!("## Issue Ongoing\n{summary}"),
            }),
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![Component::Button(Button {
                    id: None,
                    custom_id: Some(format!("issue_end:{issue_id}")),
                    disabled: false,
                    emoji: None,
                    label: Some("End Issue".to_string()),
                    style: ButtonStyle::Danger,
                    url: None,
                    sku_id: None,
                })],
            }),
        ],
    })]
}

fn rejected_components(summary: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0xED4245)),
        spoiler: None,
        components: vec![Component::TextDisplay(TextDisplay {
            id: None,
            content: format!("## Issue Dismissed\n{summary}"),
        })],
    })]
}

fn ended_components(summary: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x80848E)),
        spoiler: None,
        components: vec![Component::TextDisplay(TextDisplay {
            id: None,
            content: format!("## Issue Resolved\n{summary}"),
        })],
    })]
}
