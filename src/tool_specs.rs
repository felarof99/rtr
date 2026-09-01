use anyhow::Result;

/// How one tool stores MCP server definitions in its main config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFormat {
    /// A JSON object keyed by server name, e.g. `~/.claude.json` `mcpServers`.
    Json,
    /// A TOML table keyed by server name, e.g. `~/.codex/config.toml`
    /// `[mcp_servers.*]`.
    Toml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub resume_args: &'static [&'static str],
    pub native_home_env: &'static str,
    pub native_secure_storage_env: Option<&'static str>,
    pub default_skills_source: &'static [&'static str],
    pub rebase_external_skill_symlinks: bool,
    /// The tool's own config file, relative to `$HOME`, that apps register MCP
    /// servers in. rtr inherits only the MCP section of this file.
    pub main_config_rel: &'static [&'static str],
    /// Key holding the server map inside that file.
    pub mcp_key: &'static str,
    pub mcp_format: McpFormat,
    /// Same-named file inside a profile home, which rtr syncs into.
    pub profile_config_rel: &'static [&'static str],
}

pub const CLAUDE: ToolSpec = ToolSpec {
    name: "claude",
    resume_args: &["--resume"],
    native_home_env: "CLAUDE_CONFIG_DIR",
    native_secure_storage_env: Some("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
    default_skills_source: &[".claude", "skills"],
    rebase_external_skill_symlinks: true,
    main_config_rel: &[".claude.json"],
    mcp_key: "mcpServers",
    mcp_format: McpFormat::Json,
    profile_config_rel: &[".claude.json"],
};

pub const CODEX: ToolSpec = ToolSpec {
    name: "codex",
    resume_args: &["resume"],
    native_home_env: "CODEX_HOME",
    native_secure_storage_env: None,
    default_skills_source: &[".codex", "skills"],
    rebase_external_skill_symlinks: true,
    main_config_rel: &[".codex", "config.toml"],
    mcp_key: "mcp_servers",
    mcp_format: McpFormat::Toml,
    profile_config_rel: &["config.toml"],
};

pub const SPECS: &[ToolSpec] = &[CLAUDE, CODEX];

/// Resolve one of rtr's first-class native-profile tools.
pub fn get(name: &str) -> Result<&'static ToolSpec> {
    SPECS.iter().find(|spec| spec.name == name).ok_or_else(|| {
        anyhow::anyhow!("unsupported subscription tool '{name}' (supported: claude, codex)")
    })
}

pub fn all() -> &'static [ToolSpec] {
    SPECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_define_tool_runtime_contracts() {
        assert_eq!(get("claude").unwrap().resume_args, &["--resume"]);
        assert_eq!(get("claude").unwrap().native_home_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(
            get("claude").unwrap().native_secure_storage_env,
            Some("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        );
        assert_eq!(
            get("claude").unwrap().default_skills_source,
            &[".claude", "skills"]
        );
        assert!(get("claude").unwrap().rebase_external_skill_symlinks);
        assert_eq!(get("codex").unwrap().resume_args, &["resume"]);
        assert_eq!(get("codex").unwrap().native_home_env, "CODEX_HOME");
        assert_eq!(get("codex").unwrap().native_secure_storage_env, None);
        assert_eq!(
            get("codex").unwrap().default_skills_source,
            &[".codex", "skills"]
        );
        assert!(get("codex").unwrap().rebase_external_skill_symlinks);
        assert!(get("curl").is_err());
    }

    #[test]
    fn specs_locate_each_tools_shared_mcp_section() {
        let claude = get("claude").unwrap();
        assert_eq!(claude.main_config_rel, &[".claude.json"]);
        assert_eq!(claude.profile_config_rel, &[".claude.json"]);
        assert_eq!(claude.mcp_key, "mcpServers");
        assert_eq!(claude.mcp_format, McpFormat::Json);

        let codex = get("codex").unwrap();
        assert_eq!(codex.main_config_rel, &[".codex", "config.toml"]);
        assert_eq!(codex.profile_config_rel, &["config.toml"]);
        assert_eq!(codex.mcp_key, "mcp_servers");
        assert_eq!(codex.mcp_format, McpFormat::Toml);
    }
}
