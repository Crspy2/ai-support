use anyhow::Result;
use twilight_http::Client as HttpClient;
use twilight_model::application::command::CommandType;
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, GuildMarker};
use twilight_util::builder::command::{CommandBuilder, StringBuilder};

pub async fn register_commands(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    guild_id: Option<&str>,
) -> Result<()> {
    let client = http.interaction(application_id);
    let commands = vec![
        CommandBuilder::new("ask", "Ask the support bot a question", CommandType::ChatInput)
            .option(StringBuilder::new("question", "Your question").required(true))
            .build(),
        CommandBuilder::new("Ask Support Bot", "", CommandType::Message).build(),
    ];

    // Replace all global commands (prunes any old ones).
    client.set_global_commands(&commands).await?;
    tracing::info!(count = commands.len(), "upserted global commands");

    // Parse the configured guild ID (if any) so we know which guild gets the full list.
    let configured_guild: Option<Id<GuildMarker>> = guild_id
        .and_then(|s| s.parse::<u64>().ok())
        .map(Id::new);

    // Iterate every guild the bot is in and either set or clear commands.
    let guilds = http.current_user_guilds().await?.model().await?;
    for guild in guilds {
        if configured_guild == Some(guild.id) {
            client.set_guild_commands(guild.id, &commands).await?;
            tracing::info!(%guild.id, count = commands.len(), "upserted guild commands");
        } else {
            client.set_guild_commands(guild.id, &[]).await?;
            tracing::info!(%guild.id, "cleared stale guild commands");
        }
    }

    Ok(())
}
