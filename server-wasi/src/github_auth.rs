use anyhow::Result;
use bincode::{Decode, Encode};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

const USER_DB_TREE: &str = "user_data";
const MAX_PROJECTS_PER_USER: usize = 10;

pub struct GitHubAuth {
    user_projects: DashMap<String, UserData>,
    db: sled::Db,
}
#[derive(Serialize, Deserialize, Clone, Debug, Encode, Decode)]
pub struct UserData {
    pub github_username: String,
    pub projects: Vec<String>,
}

impl GitHubAuth {
    pub async fn new(db: sled::Db) -> Result<Self> {
        // Load existing user data
        let user_projects = DashMap::new();

        // Create or get the user data tree
        let user_tree = db.open_tree(USER_DB_TREE)?;

        // Iterate through all items in the tree
        for item in user_tree.iter().flatten() {
            if let Ok(username) = std::str::from_utf8(&item.0) {
                // Try to decode using bincode
                if let Ok((user_data, _)) =
                    bincode::decode_from_slice::<UserData, _>(&item.1, bincode::config::standard())
                {
                    user_projects.insert(username.to_string(), user_data);
                }
            }
        }

        Ok(Self { user_projects, db })
    }

    /// Authenticate and extract username from GitHub token in a single API call
    /// Returns (username, is_valid) tuple
    pub async fn authenticate_github(&self, token: &str) -> Result<(String, bool)> {
        // Check if the token is in the format "username:token"
        let (provided_username, token_value) =
            if let Some((username, token_part)) = token.split_once(':') {
                (
                    Some(username.to_string()),
                    token_part
                        .strip_prefix("Bearer ")
                        .unwrap_or(token_part)
                        .trim(),
                )
            } else {
                (None, token.strip_prefix("Bearer ").unwrap_or(token).trim())
            };

        // Create client with timeout to verify with GitHub API
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;

        // Make a single API call to GitHub
        let response = match client
            .get("https://api.github.com/user")
            .header("User-Agent", "faasta-server")
            .header("Authorization", format!("Bearer {token_value}"))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!("GitHub API request failed: {}", e);
                return Ok(("".to_string(), false));
            }
        };

        if !response.status().is_success() {
            tracing::warn!("GitHub API returned error status: {}", response.status());
            return Ok(("".to_string(), false));
        }

        // Parse response and extract username
        let github_user: Value = match response.json().await {
            Ok(json) => json,
            Err(e) => {
                tracing::error!("Failed to parse GitHub response: {}", e);
                return Ok(("".to_string(), false));
            }
        };

        let api_username = github_user["login"].as_str().unwrap_or("");

        // If username was provided in token, verify it matches
        if let Some(provided) = provided_username {
            if provided != api_username {
                tracing::warn!(
                    "Username mismatch: provided '{}', GitHub returned '{}'",
                    provided,
                    api_username
                );
                return Ok((api_username.to_string(), false));
            }
        }

        Ok((api_username.to_string(), true))
    }

    /// Check if a user can upload more projects (limit is MAX_PROJECTS_PER_USER)
    pub fn can_upload_project(&self, username: &str, project_name: &str) -> bool {
        if let Some(user_data) = self.user_projects.get(username) {
            // Check if they're already at the limit
            if user_data.projects.len() >= MAX_PROJECTS_PER_USER
                && !user_data.projects.contains(&project_name.to_string())
            {
                return false;
            }
        }
        true
    }

    /// Add a project to a user's list
    pub async fn add_project(&self, username: &str, project_name: &str) -> Result<()> {
        // Get or create user data
        let mut user_data = if let Some(data) = self.user_projects.get(username) {
            data.clone()
        } else {
            UserData {
                github_username: username.to_string(),
                projects: Vec::new(),
            }
        };

        // Add or update the project
        if !user_data.projects.contains(&project_name.to_string()) {
            user_data.projects.push(project_name.to_string());
        }

        // Update the map
        self.user_projects
            .insert(username.to_string(), user_data.clone());

        // Save to database
        let user_tree = self.db.open_tree(USER_DB_TREE)?;
        let encoded = bincode::encode_to_vec(&user_data, bincode::config::standard())?;
        user_tree.insert(username.as_bytes(), encoded)?;

        Ok(())
    }

    /// Remove a project from a user's list
    pub async fn remove_project(&self, username: &str, project_name: &str) -> Result<()> {
        // Get user data
        if let Some(mut user_data) = self.user_projects.get_mut(username) {
            // Remove the project
            user_data.projects.retain(|p| p != project_name);

            // Save to database
            let user_tree = self.db.open_tree(USER_DB_TREE)?;
            let user_data_clone = user_data.clone();
            let encoded = bincode::encode_to_vec(&user_data_clone, bincode::config::standard())?;
            user_tree.insert(username.as_bytes(), encoded)?;
        }

        Ok(())
    }

    /// Get the list of projects owned by a user
    pub fn get_user_projects(&self, username: &str) -> Option<Vec<String>> {
        self.user_projects
            .get(username)
            .map(|user_data| user_data.projects.clone())
    }
}
