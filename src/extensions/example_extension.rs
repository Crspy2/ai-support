use extensions_macros::{action, extension, fetch, ExtensionSchema};

pub struct ExampleExtension;

impl ExampleExtension {
    pub fn new() -> Self {
        Self
    }
}

#[derive(serde::Deserialize, ExtensionSchema)]
struct GetAccountArgs {
    #[description("The user's Discord snowflake ID")]
    discord_id: String,
}

#[derive(serde::Deserialize, ExtensionSchema)]
struct ResetPasswordArgs {
    #[description("The user's Discord snowflake ID")]
    discord_id: String,
}

#[extension]
impl ExampleExtension {
    /// Embeddable: loaded at startup and stored in the knowledge base.
    /// The AI never calls this directly; its content is injected as KB context.
    #[fetch(cache = "startup", embeddable = true)]
    async fn load_faq(&self, _args: ()) -> anyhow::Result<String> {
        Ok("Q: How do I reset my password? A: Click 'Forgot password' on the login page and follow the instructions sent to your email.\n\
            Q: How do I contact support? A: Use the /ask command in Discord or open a ticket in #support.".into())
    }

    /// Non-embeddable fetcher: exposed to the AI as a tool it can call.
    #[fetch(cache = "5m", description = "Look up a user account by their Discord snowflake ID")]
    async fn get_account(&self, args: GetAccountArgs) -> anyhow::Result<String> {
        Ok(format!(
            "Account for Discord user {}: status=active, plan=free",
            args.discord_id
        ))
    }

    /// Action: exposed to the AI as a tool it can call to take a real-world action.
    #[action(description = "Send a password-reset email to the specified Discord user")]
    async fn reset_password(&self, args: ResetPasswordArgs) -> anyhow::Result<String> {
        tracing::info!("Password reset triggered for user {}", args.discord_id);
        Ok(format!(
            "Password reset email sent to Discord user {}.",
            args.discord_id
        ))
    }
}

inventory::submit!(crate::extensions::ExtensionFactory {
    build: |_ctx| std::sync::Arc::new(ExampleExtension::new()),
});
