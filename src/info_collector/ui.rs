use std::collections::HashMap;

use serde_json::{Value, json};
use twilight_model::application::interaction::modal::{
    ModalInteractionActionRow, ModalInteractionComponent, ModalInteractionLabel,
};
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, Container, TextDisplay,
};
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use uuid::Uuid;

pub(super) fn info_button_components(request_id: Uuid, message: &str) -> Vec<Component> {
    vec![Component::Container(Container {
        id: None,
        accent_color: Some(Some(0x5865F2)),
        spoiler: None,
        components: vec![
            Component::TextDisplay(TextDisplay {
                id: None,
                content: format!("## Information Needed\n{message}"),
            }),
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![Component::Button(Button {
                    id: None,
                    custom_id: Some(format!("info_collect:{request_id}")),
                    disabled: false,
                    emoji: None,
                    label: Some("Provide Information".to_string()),
                    style: ButtonStyle::Primary,
                    url: None,
                    sku_id: None,
                })],
            }),
        ],
    })]
}

pub(super) fn ephemeral_error(msg: &str) -> Value {
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(msg.to_string()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    serde_json::to_value(&response).unwrap_or_else(|_| json!({ "type": 4 }))
}

pub(super) fn extract_text_inputs(
    components: &[ModalInteractionComponent],
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for c in components {
        match c {
            ModalInteractionComponent::TextInput(t) => {
                map.insert(t.custom_id.clone(), t.value.clone());
            }
            ModalInteractionComponent::ActionRow(ModalInteractionActionRow {
                components, ..
            }) => {
                map.extend(extract_text_inputs(components));
            }
            ModalInteractionComponent::Label(ModalInteractionLabel { component, .. }) => {
                map.extend(extract_text_inputs(std::slice::from_ref(component.as_ref())));
            }
            _ => {}
        }
    }
    map
}
