//! Secret-store key layout used by `config` (writer) and `adapters` (reader).
//!
//! Both sides must agree on exactly where each field lives; keeping the path
//! builders here prevents silent layout drift.

pub fn workspace_field(adapter: &str, workspace: &str, field: &str) -> String {
    format!("skills/{adapter}/workspaces/{workspace}/{field}")
}

pub fn legacy_field(adapter: &str, field: &str) -> String {
    format!("skills/{adapter}/{field}")
}

pub fn workspaces_index(adapter: &str) -> String {
    format!("skills/{adapter}/workspaces_index")
}

pub fn default_workspace(adapter: &str) -> String {
    format!("skills/{adapter}/default_workspace")
}

pub fn hub_entry(name: &str) -> String {
    format!("hubs/{name}")
}

pub const HUBS_INDEX: &str = "hubs_index";

pub const ACTIVE_HUB: &str = "hubs/active";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(
            workspace_field("linear", "work", "api_key"),
            "skills/linear/workspaces/work/api_key"
        );
        assert_eq!(legacy_field("linear", "api_key"), "skills/linear/api_key");
        assert_eq!(workspaces_index("jira"), "skills/jira/workspaces_index");
        assert_eq!(
            default_workspace("metabase"),
            "skills/metabase/default_workspace"
        );
        assert_eq!(hub_entry("driftwerk"), "hubs/driftwerk");
    }
}
