//! The tools of the assistant: web fetch, web search, workspace files and
//! the tools of the MCP servers.
//!
//! `[tools]` turns each one on or off. Web search also needs the Brave
//! Search API key in [`SEARCH_KEY_ENV`]. The key never goes in the config
//! file, and the log never shows it.
//!
//! The workspace lives in [`WORKSPACE_DIR`] beside the database. The engine
//! saves each change of a successful tool call there as a snapshot, so the
//! files survive a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use autumn_harvest::payload_codec::PayloadCodecs;
use autumn_harvest_agent::{AgentHarness, AgentProfile, DirWorkspaceBlobs, Tool, WorkspaceStore};
use littleguy_core::{GoogleConfig, McpServer, ToolsConfig};
use littleguy_tools::google::GoogleApi;
use littleguy_tools::mcp::Vars;
use littleguy_tools::{FetchTool, SearchTool, file_tools};

/// The variable that holds the Brave Search API key.
pub const SEARCH_KEY_ENV: &str = "LG_BRAVE_API_KEY";

/// The folder of the workspace snapshots, beside the database.
pub const WORKSPACE_DIR: &str = "workspace";

/// The workspace folder beside the database `db`.
#[must_use]
pub fn workspace_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(WORKSPACE_DIR)
}

/// The search key of the environment, when it is set and not empty.
#[must_use]
pub fn search_key_from_env() -> Option<String> {
    std::env::var(SEARCH_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// Opens each MCP server of the config and returns its tools. A server
/// that fails gives no tools, and the log says why. The daemon still
/// starts, so one broken server does not take the assistant down.
pub async fn mcp_tools(servers: &[McpServer]) -> Vec<Arc<dyn Tool>> {
    if servers.is_empty() {
        return Vec::new();
    }
    let vars: Vars = Arc::new(|name: &str| std::env::var(name).ok());
    let found = littleguy_tools::mcp::connect(servers, &vars).await;
    for (id, error) in &found.failed {
        tracing::warn!(server = %id, %error, "an MCP server gives no tools");
    }
    for note in &found.skipped {
        tracing::warn!("{note}");
    }
    found.tools
}

/// The Gmail and Calendar tools of `config` on `api`. A config with a
/// `[google]` section but no API, which only a test builds, gets none.
pub fn google_tools(
    config: Option<&GoogleConfig>,
    api: Option<Arc<dyn GoogleApi>>,
) -> Vec<Arc<dyn Tool>> {
    match (config, api) {
        (Some(config), Some(api)) => {
            littleguy_tools::google::google_tools(&api, config.mail, config.calendar)
        }
        (Some(_), None) => {
            tracing::warn!("the Google tools are off: San Mateo did not start");
            Vec::new()
        }
        (None, _) => Vec::new(),
    }
}

/// The tools that `config` turns on, and the workspace store they need.
#[derive(Debug, Default)]
pub struct Tools {
    tools: Vec<Arc<dyn Tool>>,
    workspace: Option<PathBuf>,
}

impl Tools {
    /// The tools of `config` for the database `db`. Web search needs
    /// `search_key`.
    #[must_use]
    pub fn new(config: &ToolsConfig, db: &Path, search_key: Option<String>) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        if config.web_fetch {
            tools.push(Arc::new(FetchTool::new(config.fetch_max_chars)));
        }
        match (config.web_search, search_key) {
            (true, Some(key)) => tools.push(Arc::new(SearchTool::new(key))),
            (true, None) => {
                tracing::info!("web search is off: set {SEARCH_KEY_ENV} to turn it on");
            }
            (false, _) => {}
        }
        if config.files {
            tools.extend(file_tools());
        }
        Self {
            tools,
            workspace: config.files.then(|| workspace_for(db)),
        }
    }

    /// Adds `extra`, such as the tools of the MCP servers.
    #[must_use]
    pub fn with(mut self, extra: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(extra);
        self
    }

    /// The tools, in order.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// The names of the tools, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// Unpacks the tool list and optional workspace snapshot directory.
    #[must_use]
    pub fn into_parts(self) -> (Vec<Arc<dyn Tool>>, Option<PathBuf>) {
        (self.tools, self.workspace)
    }

    /// Gives the harness the tools and the workspace store, and the
    /// profile a workspace.
    #[must_use]
    pub fn apply(
        self,
        harness: AgentHarness,
        profile: AgentProfile,
    ) -> (AgentHarness, AgentProfile) {
        let harness = harness.tools(self.tools);
        match self.workspace {
            Some(root) => {
                let blobs = Arc::new(DirWorkspaceBlobs::new(root));
                (
                    harness.workspace(WorkspaceStore::new(blobs, PayloadCodecs::default())),
                    profile.workspace(),
                )
            }
            None => (harness, profile),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_picks_the_tools() {
        let db = Path::new("/var/lg/lg.db");
        let all = Tools::new(&ToolsConfig::default(), db, Some(String::from("k")));
        assert_eq!(
            all.names(),
            [
                "web_fetch",
                "web_search",
                "read_file",
                "write_file",
                "list_files",
                "delete_file"
            ]
        );
        assert_eq!(
            all.workspace.as_deref(),
            Some(Path::new("/var/lg/workspace"))
        );

        // Without a key there is no search.
        let keyless = Tools::new(&ToolsConfig::default(), db, None);
        assert!(!keyless.names().contains(&"web_search"));

        let none = ToolsConfig {
            web_fetch: false,
            web_search: false,
            files: false,
            ..ToolsConfig::default()
        };
        let empty = Tools::new(&none, db, Some(String::from("k")));
        assert_eq!(empty.names().len(), 0);
        assert_eq!(empty.workspace, None);
    }

    #[test]
    fn a_bare_database_name_keeps_the_workspace_beside_it() {
        assert_eq!(workspace_for(Path::new("lg.db")), Path::new("workspace"));
    }

    #[tokio::test]
    async fn a_broken_mcp_server_gives_no_tools() {
        assert!(mcp_tools(&[]).await.is_empty());
        let broken = McpServer {
            id: String::from("gone"),
            transport: littleguy_core::McpTransport::Stdio {
                command: String::from("/nonexistent/lg-mcp"),
                args: Vec::new(),
                env: Vec::new(),
            },
            labels: Vec::new(),
            effect: autumn_harvest_agent::ToolEffect::External,
            tools: None,
            timeout_secs: 5,
        };
        assert!(mcp_tools(&[broken]).await.is_empty());
    }

    #[tokio::test]
    async fn mcp_tools_join_the_others() {
        let server = littleguy_tools::testing::serve_http(None).await.unwrap();
        let web = McpServer {
            id: String::from("web"),
            transport: littleguy_core::McpTransport::Http {
                url: server.url.clone(),
                bearer_env: None,
            },
            labels: Vec::new(),
            effect: autumn_harvest_agent::ToolEffect::ReadOnly,
            tools: Some(vec![String::from("echo"), String::from("gone")]),
            timeout_secs: 5,
        };
        let mcp = mcp_tools(&[web]).await;
        let config = ToolsConfig {
            web_search: false,
            files: false,
            ..ToolsConfig::default()
        };
        let tools = Tools::new(&config, Path::new("/var/lg/lg.db"), None).with(mcp);
        assert_eq!(tools.names(), ["web_fetch", "web__echo"]);
        assert_eq!(tools.all().len(), 2);
    }
}
