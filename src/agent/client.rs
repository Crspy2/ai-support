use anyhow::{Context, Result};
use async_openai::{
    Client as OpenAIClient,
    config::OpenAIConfig,
    types::{
        chat::{
            ChatCompletionMessageToolCalls,
            ChatCompletionRequestAssistantMessageArgs,
            ChatCompletionRequestMessage,
            ChatCompletionRequestToolMessageArgs,
            ChatCompletionTool,
            ChatCompletionToolChoiceOption,
            ChatCompletionTools,
            CreateChatCompletionRequestArgs,
            FunctionObjectArgs,
            ToolChoiceOptions,
        },
        moderations::{CreateModerationRequest, ModerationInput},
    },
};
use serde_json::json;

use crate::extensions::ExtensionRegistry;

const MAX_TOOL_TURNS: usize = 10;

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
    initial_messages: Vec<ChatCompletionRequestMessage>,
    registry: &ExtensionRegistry,
) -> Result<AiResponse> {
    let mut messages = initial_messages;
    let mut reaction: Option<String> = None;

    let tools = build_tools(registry)?;

    for _ in 0..MAX_TOOL_TURNS {
        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages(messages.clone())
            .max_tokens(2000u32)
            .tools(tools.clone())
            .tool_choice(ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::Auto))
            .build()?;

        let response = client.chat().create(request).await?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("OpenAI returned no choices")?;

        let tool_calls = choice.message.tool_calls.clone();

        if let Some(ref calls) = tool_calls {
            for call in calls {
                if let ChatCompletionMessageToolCalls::Function(fn_call) = call {
                    if fn_call.function.name == "react" {
                        if let Ok(args) =
                            serde_json::from_str::<serde_json::Value>(&fn_call.function.arguments)
                        {
                            if let Some(emoji) = args["emoji"].as_str() {
                                reaction = Some(emoji.to_string());
                            }
                        }
                    }
                }
            }
        }

        if tool_calls.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
            return Ok(AiResponse {
                content: choice.message.content,
                reaction,
            });
        }

        messages.push(
            ChatCompletionRequestAssistantMessageArgs::default()
                .tool_calls(tool_calls.clone().unwrap_or_default())
                .build()?
                .into(),
        );

        if let Some(calls) = &tool_calls {
            for call in calls {
                if let ChatCompletionMessageToolCalls::Function(fn_call) = call {
                    let tool_result = if fn_call.function.name == "react" {
                        "ok".to_string()
                    } else {
                        execute_tool(registry, &fn_call.function.name, &fn_call.function.arguments)
                            .await
                    };

                    messages.push(
                        ChatCompletionRequestToolMessageArgs::default()
                            .tool_call_id(fn_call.id.clone())
                            .content(tool_result)
                            .build()?
                            .into(),
                    );
                }
            }
        }
    }

    Ok(AiResponse {
        content: Some("(I reached my tool call limit — please try again.)".to_string()),
        reaction,
    })
}

/// Execute a named tool from the registry.
/// Tool names are encoded as "ExtName::method_name".
async fn execute_tool(registry: &ExtensionRegistry, name: &str, arguments: &str) -> String {
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);

    let (ext_name, method_name) = match name.split_once("::") {
        Some(pair) => pair,
        None => return format!("error: malformed tool name '{name}'"),
    };

    match registry.call_fetcher(ext_name, method_name, args.clone()).await {
        Ok(result) => result,
        Err(_) => match registry.call_action(ext_name, method_name, args).await {
            Ok(result) => result,
            Err(e) => format!("error: {e:#}"),
        },
    }
}

/// Build the full tool list: react + non-embeddable fetchers + actions.
fn build_tools(registry: &ExtensionRegistry) -> Result<Vec<ChatCompletionTools>> {
    let mut tools: Vec<ChatCompletionTools> = Vec::new();

    tools.push(ChatCompletionTools::Function(ChatCompletionTool {
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
    }));

    for (ext_name, method_name, description, schema) in registry.non_embeddable_fetchers() {
        let tool_name = format!("{ext_name}::{method_name}");
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name(tool_name)
                .description(description)
                .parameters(schema)
                .build()?,
        }));
    }

    for (ext_name, method_name, description, schema) in registry.all_actions() {
        let tool_name = format!("{ext_name}::{method_name}");
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name(tool_name)
                .description(description)
                .parameters(schema)
                .build()?,
        }));
    }

    Ok(tools)
}
