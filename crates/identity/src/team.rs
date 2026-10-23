use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub creator_id: Uuid,
    #[serde(default)]
    pub members: Vec<Uuid>,
    #[serde(default)]
    pub join_requests: Vec<JoinRequest>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub identity_id: Uuid,
    pub requested_at: DateTime<Utc>,
}

impl Team {
    pub fn new(name: impl Into<String>, creator_id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            creator_id,
            members: vec![creator_id],
            join_requests: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn has_member(&self, identity_id: Uuid) -> bool {
        self.members.contains(&identity_id)
    }

    pub fn add_member(&mut self, identity_id: Uuid) -> bool {
        if self.members.contains(&identity_id) {
            return false;
        }
        self.members.push(identity_id);
        self.join_requests.retain(|r| r.identity_id != identity_id);
        self.updated_at = Utc::now();
        true
    }

    pub fn remove_member(&mut self, identity_id: Uuid) -> bool {
        let before = self.members.len();
        self.members.retain(|m| *m != identity_id);
        if self.members.len() < before {
            self.updated_at = Utc::now();
            true
        } else {
            false
        }
    }

    pub fn request_join(&mut self, identity_id: Uuid) -> bool {
        if self.has_member(identity_id)
            || self
                .join_requests
                .iter()
                .any(|r| r.identity_id == identity_id)
        {
            return false;
        }
        self.join_requests.push(JoinRequest {
            identity_id,
            requested_at: Utc::now(),
        });
        self.updated_at = Utc::now();
        true
    }

    pub fn reject_join(&mut self, identity_id: Uuid) -> bool {
        let before = self.join_requests.len();
        self.join_requests.retain(|r| r.identity_id != identity_id);
        if self.join_requests.len() < before {
            self.updated_at = Utc::now();
            true
        } else {
            false
        }
    }
}

pub trait TeamStore: Send + Sync {
    fn create_team(&self, team: &Team) -> eyre::Result<()>;
    fn get_team(&self, id: Uuid) -> eyre::Result<Option<Team>>;
    fn update_team(&self, team: &Team) -> eyre::Result<()>;
    fn delete_team(&self, id: Uuid) -> eyre::Result<()>;
    fn list_teams(&self) -> eyre::Result<Vec<Team>>;
    fn teams_for_member(&self, identity_id: Uuid) -> eyre::Result<Vec<Team>>;
}
