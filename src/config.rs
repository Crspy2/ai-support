use anyhow::{Context, Result};

pub struct Config {
    pub discord_token: String,
    pub discord_public_key: String,
    pub openai_api_key: String,
    pub ai_model: String,
    pub ai_system_prompt: String,
    pub ai_reply_chain_depth: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Self {
            discord_token: std::env::var("DISCORD_TOKEN")
               .context("DISCORD_TOKN is not set")?,
            discord_public_key: std::env::var("DISCORD_PUBLIC_KEY")
                .context("DISCORD_PUBLIC_KEY is not set")?,
            openai_api_key: std::env::var("OPENAI_API_KEY")
                .context("OPENAI_API_KEY is not set")?,
            ai_model:  std::env::var("AI_MODEL")
                .context("AI_MODEL is not set")?,
            ai_system_prompt: std::env::var("AI_SYSTEM_PROMPT")
                .context("AI_SYSTEM_PROMPT is not set")?,
            ai_reply_chain_depth: std::env::var("AI_REPLY_CHAIN_DEPTH")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .context("AI_REPLY_CHAIN_DEPTH must be a number")?,
        })
    }
}