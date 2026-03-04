#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IssueProposedHook {
    pub issue_id: uuid::Uuid,
    pub summary: String,
    pub user_count: i32,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IssueAcceptedHook {
    pub issue_id: uuid::Uuid,
    pub summary: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IssueRejectedHook {
    pub issue_id: uuid::Uuid,
    pub summary: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IssueEndedHook {
    pub issue_id: uuid::Uuid,
    pub summary: String,
}
