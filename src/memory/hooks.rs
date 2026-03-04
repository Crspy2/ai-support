#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct MemoryRequestedHook {
    pub memory_id: uuid::Uuid,
    pub content: String,
    pub summary: String,
    pub message_link: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct MemoryApprovedHook {
    pub memory_id: uuid::Uuid,
    pub content: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct MemoryRejectedHook {
    pub memory_id: uuid::Uuid,
    pub content: String,
}
