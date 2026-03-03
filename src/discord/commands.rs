use anyhow::Result;
use twilight_http::Client as HttpClient;
use twilight_model::application::command::{CommandOption, CommandOptionType};
use twilight_model::id::marker::ApplicationMarker;
use twilight_model::id::Id;

pub async fn register_commands(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
) -> Result<()> {
    let client = http.interaction(application_id);

    client
        .create_global_command()
        .chat_input("ask", "Ask the support bot a question")
        .command_options(&[CommandOption {
            name: "question".to_string(),
            description: "Your question".to_string(),
            kind: CommandOptionType::String,
            required: Some(true),
            autocomplete: None,
            channel_types: None,
            choices: None,
            description_localizations: None,
            max_length: None,
            max_value: None,
            min_length: None,
            min_value: None,
            name_localizations: None,
            options: None,
        }])
        .await?;

    client
        .create_global_command()
        .message("Ask Support Bot")
        .await?;

    tracing::info!("registered global commands");
    Ok(())
}
