use anyhow::{Context, Result};
use async_openai::{
    Client as OpenAIClient,
    config::OpenAIConfig,
    types::embeddings::CreateEmbeddingRequestArgs,
};

pub async fn embed_text(client: &OpenAIClient<OpenAIConfig>, text: &str) -> Result<Vec<f32>> {
    let request = CreateEmbeddingRequestArgs::default()
        .model("text-embedding-3-small")
        .input(text)
        .build()?;

    let response = client.embeddings().create(request).await?;

    let embedding = response
        .data
        .into_iter()
        .next()
        .context("OpenAI returned no embedding")?
        .embedding
        .into_iter()
        .map(|x| x as f32)
        .collect();

    Ok(embedding)
}
