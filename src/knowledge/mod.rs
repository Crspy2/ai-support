pub mod embed;

use std::sync::Arc;

use anyhow::Result;
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::extensions::ExtensionRegistry;
use embed::embed_text;

pub struct KnowledgeBase {
    pool: Arc<PgPool>,
    openai: Arc<OpenAIClient<OpenAIConfig>>,
}

impl KnowledgeBase {
    pub fn new(pool: Arc<PgPool>, openai: Arc<OpenAIClient<OpenAIConfig>>) -> Self {
        Self { pool, openai }
    }

    /// Walk all embeddable fetchers, call them, embed their output, and upsert into DB.
    pub async fn populate_at_startup(&self, registry: &ExtensionRegistry) -> Result<()> {
        for ext in registry.extensions() {
            let fetchers = Arc::clone(ext).fetchers();
            for descriptor in fetchers {
                if !descriptor.embeddable {
                    continue;
                }

                let source = format!("{}::{}", ext.name(), descriptor.name);

                let content = match (descriptor.handler)(serde_json::Value::Null).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("KB populate failed for {source}: {e:#}");
                        continue;
                    }
                };

                let hash = sha256_hex(&content);

                let existing_hash: Option<String> = sqlx::query_scalar(
                    "SELECT content_hash FROM knowledge_chunks WHERE source = $1",
                )
                .bind(&source)
                .fetch_optional(self.pool.as_ref())
                .await?;

                if existing_hash.as_deref() == Some(&hash) {
                    tracing::debug!("KB chunk '{source}' unchanged, skipping embed");
                    continue;
                }

                tracing::info!("Embedding KB chunk '{source}'");
                let embedding = embed_text(&self.openai, &content).await?;
                let vector = Vector::from(embedding);

                sqlx::query(
                    r#"
                    INSERT INTO knowledge_chunks (source, content, content_hash, embedding, updated_at)
                    VALUES ($1, $2, $3, $4, now())
                    ON CONFLICT (source) DO UPDATE
                        SET content = EXCLUDED.content,
                            content_hash = EXCLUDED.content_hash,
                            embedding = EXCLUDED.embedding,
                            updated_at = EXCLUDED.updated_at
                    "#,
                )
                .bind(&source)
                .bind(&content)
                .bind(&hash)
                .bind(vector)
                .execute(self.pool.as_ref())
                .await?;

                tracing::info!("KB chunk '{source}' upserted");
            }
        }

        Ok(())
    }

    /// Embed the query and return the top-k most similar knowledge chunks.
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<String>> {
        let embedding = embed_text(&self.openai, query).await?;
        let vector = Vector::from(embedding);

        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM knowledge_chunks ORDER BY embedding <-> $1 LIMIT $2",
        )
        .bind(vector)
        .bind(limit)
        .fetch_all(self.pool.as_ref())
        .await?;

        Ok(rows)
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
