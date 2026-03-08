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
use serde_json::{Value, json};
use twilight_model::channel::message::component::TextInputStyle;

use crate::extensions::ExtensionRegistry;
use crate::info_collector::{InfoCollector, InfoField};
use crate::memory::MemoryTracker;

const MAX_TOOL_TURNS: usize = 10;

pub struct PartialInfoRequest {
    pub title: String,
    pub message: String,
    pub fields: Vec<InfoField>,
    pub ext_name: String,
    pub method_name: String,
    pub known_args: Value,
}

pub struct AiResponse {
    pub content: Option<String>,
    pub reaction: Option<String>,
    pub info_request: Option<PartialInfoRequest>,
    /// Tool results collected during this response turn, keyed as "ToolName:args_json".
    /// Callers should persist these into the conversation tool cache.
    pub tool_results: Vec<(String, String)>,
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
    memory_tracker: Option<&MemoryTracker>,
    info_collector: Option<&InfoCollector>,
    existing_tool_cache: &std::collections::HashMap<String, String>,
) -> Result<AiResponse> {
    let mut messages = initial_messages;
    let mut reaction: Option<String> = None;
    let mut info_request: Option<PartialInfoRequest> = None;
    let mut tool_results: Vec<(String, String)> = Vec::new();

    let tools = build_tools(registry, memory_tracker, info_collector)?;
    tracing::debug!(tool_count = tools.len(), "built tool list for AI request");

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
                            serde_json::from_str::<Value>(&fn_call.function.arguments)
                        {
                            if let Some(emoji) = args["emoji"].as_str() {
                                reaction = Some(emoji.to_string());
                            }
                        }
                    } else if fn_call.function.name == "request_info" {
                        if let Ok(args) =
                            serde_json::from_str::<Value>(&fn_call.function.arguments)
                        {
                            let fields = args["fields"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .map(|f| InfoField {
                                            id: f["id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string(),
                                            label: f["label"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string(),
                                            description: f["description"]
                                                .as_str()
                                                .map(String::from),
                                            placeholder: f["placeholder"]
                                                .as_str()
                                                .map(String::from),
                                            required: f["required"]
                                                .as_bool()
                                                .unwrap_or(true),
                                            style: match f["style"].as_str().unwrap_or("short") {
                                                "paragraph" => TextInputStyle::Paragraph,
                                                _ => TextInputStyle::Short,
                                            },
                                            cache: f["cache"].as_bool().unwrap_or(false),
                                            cache_ttl_hours: f["cache_ttl_hours"].as_u64(),
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            let resume = &args["resume_action"];
                            let tool_name = resume["name"].as_str().unwrap_or("");
                            let (ext_name, method_name) = tool_name
                                .split_once("__")
                                .unwrap_or(("", tool_name));

                            info_request = Some(PartialInfoRequest {
                                title: args["title"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                message: args["message"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                fields,
                                ext_name: ext_name.to_string(),
                                method_name: method_name.to_string(),
                                known_args: resume["args"].clone(),
                            });
                        }
                    } else if fn_call.function.name == "request_memory" {
                        // handled in the result loop below
                    }
                }
            }
        }

        if tool_calls.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
            tracing::debug!("AI returned content with no tool calls");
            return Ok(AiResponse {
                content: choice.message.content,
                reaction,
                info_request,
                tool_results,
            });
        }

        if let Some(calls) = &tool_calls {
            for call in calls {
                if let ChatCompletionMessageToolCalls::Function(fn_call) = call {
                    tracing::info!(
                        tool = %fn_call.function.name,
                        args = %fn_call.function.arguments,
                        "AI calling tool"
                    );
                }
            }
        }

        messages.push(
            ChatCompletionRequestAssistantMessageArgs::default()
                .tool_calls(tool_calls.clone().unwrap_or_default())
                .build()?
                .into(),
        );

        // If request_info was called this turn, stop immediately — the conversation is
        // paused waiting for the user and there's nothing more to do.
        if info_request.is_some() {
            return Ok(AiResponse {
                content: None,
                reaction,
                info_request,
                tool_results,
            });
        }

        if let Some(calls) = &tool_calls {
            for call in calls {
                if let ChatCompletionMessageToolCalls::Function(fn_call) = call {
                    let tool_result = if fn_call.function.name == "react" {
                        "ok".to_string()
                    } else if fn_call.function.name == "request_info" {
                        "Information requested from user.".to_string()
                    } else if fn_call.function.name == "request_memory" {
                        match memory_tracker {
                            Some(t) => {
                                let args = serde_json::from_str(&fn_call.function.arguments)
                                    .unwrap_or(Value::Null);
                                t.request(args).await
                            }
                            None => "Memory system unavailable.".to_string(),
                        }
                    } else {
                        let cache_key = format!("{}:{}", fn_call.function.name, fn_call.function.arguments);
                        let result = if let Some(cached) = existing_tool_cache.get(&cache_key) {
                            tracing::debug!(tool = %fn_call.function.name, "tool result from conversation cache");
                            cached.clone()
                        } else {
                            let r = execute_tool(registry, &fn_call.function.name, &fn_call.function.arguments)
                                .await;
                            tracing::info!(
                                tool = %fn_call.function.name,
                                result = %r,
                                "tool result"
                            );
                            tool_results.push((cache_key, r.clone()));
                            r
                        };
                        result
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
        info_request,
        tool_results,
    })
}

/// Execute a named tool from the registry.
/// Tool names are encoded as "ExtName::method_name".
async fn execute_tool(registry: &ExtensionRegistry, name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);

    let (ext_name, method_name) = match name.split_once("__") {
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

/// Build the full tool list: react + request_memory (if available) + request_info (if available) + non-embeddable fetchers + actions.
fn build_tools(
    registry: &ExtensionRegistry,
    memory_tracker: Option<&MemoryTracker>,
    info_collector: Option<&InfoCollector>,
) -> Result<Vec<ChatCompletionTools>> {
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

    if memory_tracker.is_some() {
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name("request_memory")
                .description(
                    "Propose that a NEW piece of information be permanently added to the \
                    knowledge base. ONLY call this when the conversation revealed something \
                    genuinely novel and useful that is NOT already covered by existing KB \
                    content — for example: a newly discovered bug, a solution to an unusual \
                    problem, or a recurring issue pattern that was resolved in a non-obvious \
                    way. DO NOT call this for routine interactions such as: user asked a \
                    common question, user was told to provide their username, standard \
                    subscription lookups, or any interaction that follows normal support \
                    procedure. DO NOT call this multiple times in one conversation. If the \
                    user explicitly asks you to remember something, only do so if their \
                    author_id matches the owner_id in the system prompt. The content MUST be \
                    self-contained: describe the specific problem AND the resolution so it can \
                    be matched to similar problems in the future.",
                )
                .parameters(json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "A self-contained entry that includes: (1) the \
                            situation or problem context where this applies, and (2) the \
                            relevant information or solution. Example format: 'When \
                            [situation/symptom], [information/resolution].' Do NOT write a \
                            raw fact without context."
                        },
                        "summary": {
                            "type": "string",
                            "description": "Brief description of the conversation and why \
                            this is worth preserving for future queries."
                        },
                        "message_link": {
                            "type": "string",
                            "description": "The Discord link to the triggering message \
                            (provided in the system prompt)."
                        }
                    },
                    "required": ["content", "summary", "message_link"]
                }))
                .build()?,
        }));
    }

    if info_collector.is_some() {
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name("request_info")
                .description(
                    "Request sensitive information from the user via a private modal popup. \
                    IMPORTANT: Before calling this tool, check whether the user has already \
                    provided the required value anywhere in the current conversation. If they \
                    have (e.g. 'my username is User123'), extract it from the message and call \
                    the action directly — do NOT open a modal for information that is already \
                    known. Only use this tool for data that has NOT been provided and must not \
                    appear in public chat: account names, emails, order IDs, passwords, etc. \
                    If you already know some fields but not others, pass the known values in \
                    `resume_action.args` and only request the missing fields. The `message` \
                    field is shown publicly — it may reference non-sensitive context but must \
                    not contain sensitive values.",
                )
                .parameters(json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Modal title shown to the user"
                        },
                        "message": {
                            "type": "string",
                            "description": "Public message posted in channel. May reference \
                            non-sensitive context but no sensitive values."
                        },
                        "fields": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string" },
                                    "label": {
                                        "type": "string",
                                        "description": "Field label, max 45 chars"
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Optional description, max 100 chars"
                                    },
                                    "placeholder": { "type": "string" },
                                    "required": { "type": "boolean" },
                                    "style": {
                                        "type": "string",
                                        "enum": ["short", "paragraph"]
                                    },
                                    "cache": {
                                        "type": "boolean",
                                        "description": "Whether to store this value per-user \
                                        and skip asking again. Default false. Use true only for \
                                        stable identifiers (username, email). Use false for \
                                        transient values (current order ID, license key for \
                                        daily product)."
                                    },
                                    "cache_ttl_hours": {
                                        "type": "number",
                                        "description": "Only relevant when cache=true. Hours \
                                        before the cached value expires and is re-requested. \
                                        Omit for indefinite caching."
                                    }
                                },
                                "required": ["id", "label", "required"]
                            }
                        },
                        "resume_action": {
                            "type": "object",
                            "properties": {
                                "name": {
                                    "type": "string",
                                    "description": "Tool name in the format 'ExtName__method_name' (double underscore)"
                                },
                                "args": {
                                    "type": "object",
                                    "description": "Already-known args; collected field values \
                                    will be merged in"
                                }
                            },
                            "required": ["name", "args"]
                        }
                    },
                    "required": ["title", "message", "fields", "resume_action"]
                }))
                .build()?,
        }));
    }

    for (ext_name, method_name, description, schema) in registry.non_embeddable_fetchers() {
        let tool_name = format!("{ext_name}__{method_name}");
        tools.push(ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name(tool_name)
                .description(description)
                .parameters(schema)
                .build()?,
        }));
    }

    for (ext_name, method_name, description, schema) in registry.all_actions() {
        let tool_name = format!("{ext_name}__{method_name}");
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
