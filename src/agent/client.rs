use anyhow::{Context, Result};
use async_openai::{
    Client as OpenAIClient,
    config::OpenAIConfig,
    types::chat::{ChatCompletionRequestMessage, CreateChatCompletionRequestArgs},
};

pub async fn call_openai(
    client: &OpenAIClient<OpenAIConfig>,
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
) -> Result<String> {
    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(messages)
        .max_tokens(2000u32)
        .build()?;

    let response = client.chat().create(request).await?;

    response
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .context("OpenAI returned no content")
}
