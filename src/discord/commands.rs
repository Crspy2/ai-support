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

    client.set_global_commands(&commands).await?;
    tracing::info!(count = commands.len(), "upserted global commands");

    if let Some(gid) = guild_id {
        let guild_id = Id::<GuildMarker>::new(gid.parse()?);
        client.set_guild_commands(guild_id, &commands).await?;
        tracing::info!(%guild_id, count = commands.len(), "upserted guild commands");
    }

    Ok(())
}
