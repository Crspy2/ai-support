use anyhow::{Context, Result};
use async_openai::{
    Client as OpenAIClient,
    config::OpenAIConfig,
    types::{
        chat::{
            ChatCompletionMessageToolCalls, ChatCompletionRequestMessage, ChatCompletionTool,
            CreateChatCompletionRequestArgs, FunctionObjectArgs,
        },
        moderations::{CreateModerationRequest, ModerationInput},
    },
};
use serde_json::json;

pub struct AiResponse {
    pub content: Option<String>,
    pub reaction: Option<String>,
}

pub async fn call_moderation(
    client: &OpenAIClient<OpenAIConfig>,
    content: &str,
) -> Result<bool> {
    let request = CreateModerationRequest {
        input: ModerationInput::String(content.to_string()),
        model: None,
    };

    let response = client.moderations().create(request).await?;
    Ok(response.results.first().map(|r| r.flagged).unwrap_or(false))
}

pub async fn call_openai(
    client: &OpenAIClient<OpenAIConfig>,
    model: &str,
    messages: Vec<ChatCompletionRequestMessage>,
) -> Result<AiResponse> {
    let react_tool = ChatCompletionTool {
        function: FunctionObjectArgs::default()
            .name("react")
            .description(
                "Add an emoji reaction to the user's message. \
                Use this when the user says something true, interesting, or positive. \
                You can call this alongside your text response, or instead of one \
                if no reply is needed.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "emoji": {
                        "type": "string",
                        "description": "A single emoji character to react with"
                    }
                },
                "required": ["emoji"]
            }))
            .build()?,
    };

    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(messages)
        .max_tokens(2000u32)
        .tools(react_tool)
        .build()?;

    let response = client.chat().create(request).await?;
    let choice = response.choices.into_iter().next().context("OpenAI returned no choices")?;

    let reaction = choice.message.tool_calls.as_ref().and_then(|calls| {
        calls.iter().find_map(|call| {
            if let ChatCompletionMessageToolCalls::Function(fn_call) = call {
                if fn_call.function.name == "react" {
                    return serde_json::from_str::<serde_json::Value>(&fn_call.function.arguments)
                        .ok()
                        .and_then(|args| args["emoji"].as_str().map(|s| s.to_string()));
                }
            }
            None
        })
    });

    Ok(AiResponse {
        content: choice.message.content,
        reaction,
    })
}
