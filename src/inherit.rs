//! Selective inheritance of shared MCP servers from each tool's main config.
//!
//! Apps such as BrowserOS Neo register their MCP servers in the tool's *main*
//! config — `~/.claude.json` (`mcpServers`) or `~/.codex/config.toml`
//! (`[mcp_servers.*]`). An rtr profile home is a separate native home, so it
//! never saw those additions and the servers were simply missing inside rtr
//! sessions.
//!
//! rtr therefore syncs the MCP section — and only that section — from the main
//! config into every profile home at launch and at `rtr switch`. The rest of
//! the main config stays out: `~/.codex/config.toml` also carries `hooks`,
//! `plugins`, `notify`, `model`, `personality` and per-project trust, and
//! copying those into a profile changes how that profile behaves. Per-profile
//! identity is untouched for free: Codex auth lives in `auth.json` and Claude's
//! in `oauthAccount` / `userID` plus secure storage, none of which is the MCP
//! section.
//!
//! # Provenance
//!
//! `<profile_home>/.rtr-inherited.json` records, per server name, a fingerprint
//! of the definition rtr injected. That is what separates "rtr put this here"
//! from "the user wrote this", which in turn is what lets the sync update and
//! retract its own entries while never touching the profile's:
//!
//! - absent from the profile → inject and record the fingerprint
//! - present, and the profile still matches the recorded fingerprint → rtr owns
//!   it: refresh it when main changed
//! - present, with no record or a different fingerprint → the profile authored
//!   or customized it: leave it alone and drop the record, so it stays the
//!   profile's own from then on
//! - recorded but gone from main → delete it if the profile still matches the
//!   record, otherwise keep the profile's version and drop the record
//!
//! The fingerprint is FNV-1a/64 over the definition rendered as canonical JSON
//! (`serde_json::Value`, whose maps are ordered, so the rendering is stable
//! across formatting and key order). It detects change, not tampering.
//!
//! The sync is idempotent, writes atomically under a per-home lock, and must
//! never prevent a launch: a missing main config is skipped silently and a
//! malformed one warns to stderr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Tool;
use crate::paths::Paths;
use crate::tool_specs::{McpFormat, ToolSpec};

const PROVENANCE_FILE: &str = ".rtr-inherited.json";
const LOCK_FILE: &str = ".rtr-inherit.lock";
const PROVENANCE_VERSION: u32 = 1;

/// What one sync did, by server name. Empty means the profile was already current.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Servers in main that the profile did not have.
    pub injected: Vec<String>,
    /// rtr-owned servers whose definition changed in main.
    pub updated: Vec<String>,
    /// rtr-owned servers that main no longer defines.
    pub removed: Vec<String>,
    /// Servers the profile authored or customized, left untouched.
    pub kept: Vec<String>,
}

impl SyncReport {
    pub fn changed(&self) -> bool {
        !self.injected.is_empty() || !self.updated.is_empty() || !self.removed.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Provenance {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, String>,
}

/// Sync one profile home's MCP servers, reporting failure to stderr only.
///
/// Inheritance is a convenience layered on top of the launch; a broken main
/// config must degrade to "no new servers", never to "cannot start the tool".
pub(crate) fn sync_profile_best_effort(
    paths: &Paths,
    spec: &ToolSpec,
    tool: &Tool,
    profile_home: &Path,
) {
    if !tool.inherit_mcp {
        return;
    }
    let main_config = paths.main_tool_config(spec);
    if let Err(error) = sync(spec, &main_config, profile_home) {
        eprintln!(
            "rtr: could not inherit MCP servers from {}: {error:#}",
            main_config.display()
        );
    }
}

/// Sync `main_config`'s MCP servers into `profile_home` under an exclusive lock.
pub fn sync(spec: &ToolSpec, main_config: &Path, profile_home: &Path) -> Result<SyncReport> {
    // Checked before locking so a tool with no main config leaves no trace at
    // all in the profile home, not even a lock file.
    if !main_config.exists() {
        return Ok(SyncReport::default());
    }
    crate::file_lock::with_exclusive_lock(&profile_home.join(LOCK_FILE), || {
        sync_locked(spec, main_config, profile_home)
    })
}

fn sync_locked(spec: &ToolSpec, main_config: &Path, profile_home: &Path) -> Result<SyncReport> {
    let profile_config = profile_config_path(spec, profile_home);
    // A home whose config *is* the main config has nothing to inherit, and
    // rewriting it would make rtr fight itself.
    if same_file(main_config, &profile_config) {
        return Ok(SyncReport::default());
    }
    let Some(main_text) = read_optional(main_config)? else {
        return Ok(SyncReport::default());
    };

    match spec.mcp_format {
        McpFormat::Json => sync_json(spec, &main_text, &profile_config, profile_home),
        McpFormat::Toml => sync_toml(spec, &main_text, &profile_config, profile_home),
    }
}

// ---------------------------------------------------------------------------
// Format-independent plan
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Plan {
    /// Names to copy from main into the profile (insert or replace).
    write: Vec<String>,
    /// Names to delete from the profile.
    remove: Vec<String>,
    /// Provenance to persist afterwards.
    provenance: BTreeMap<String, String>,
    report: SyncReport,
}

fn plan(
    main: &BTreeMap<String, String>,
    profile: &BTreeMap<String, String>,
    previous: &BTreeMap<String, String>,
) -> Plan {
    let mut plan = Plan::default();

    for (name, main_print) in main {
        match profile.get(name) {
            None => {
                plan.write.push(name.clone());
                plan.provenance.insert(name.clone(), main_print.clone());
                plan.report.injected.push(name.clone());
            }
            // Untouched since rtr injected it, so rtr still owns it.
            Some(profile_print) if previous.get(name) == Some(profile_print) => {
                if profile_print != main_print {
                    plan.write.push(name.clone());
                    plan.report.updated.push(name.clone());
                }
                plan.provenance.insert(name.clone(), main_print.clone());
            }
            // Authored or customized in the profile: hands off, and forget the
            // record so later syncs keep treating it as the profile's own.
            Some(_) => plan.report.kept.push(name.clone()),
        }
    }

    for (name, recorded) in previous {
        if main.contains_key(name) {
            continue;
        }
        if profile.get(name) == Some(recorded) {
            plan.remove.push(name.clone());
            plan.report.removed.push(name.clone());
        }
        // Either way main no longer owns the name, so the record is dropped.
    }

    plan
}

// ---------------------------------------------------------------------------
// JSON (Claude)
// ---------------------------------------------------------------------------

fn sync_json(
    spec: &ToolSpec,
    main_text: &str,
    profile_config: &Path,
    profile_home: &Path,
) -> Result<SyncReport> {
    let main_doc: serde_json::Value =
        serde_json::from_str(main_text).context("parsing main config as JSON")?;
    let main_servers = json_servers(&main_doc, spec.mcp_key)
        .context("reading MCP servers from the main config")?;

    let profile_text = read_optional(profile_config)?;
    // Nothing to inherit and nothing already inherited: leave a pristine home
    // pristine rather than creating a config the tool has not asked for.
    if main_servers.is_empty() && profile_text.is_none() {
        return Ok(SyncReport::default());
    }
    let mut profile_doc = match profile_text.as_deref() {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).context("parsing profile config as JSON")?
        }
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    let profile_servers = json_servers(&profile_doc, spec.mcp_key)
        .context("reading MCP servers from the profile config")?;

    let previous = load_provenance(profile_home);
    let plan = plan(
        &fingerprints(&main_servers)?,
        &fingerprints(&profile_servers)?,
        &previous,
    );

    if plan.report.changed() {
        let root = profile_doc
            .as_object_mut()
            .context("profile config is not a JSON object")?;
        let servers = root
            .entry(spec.mcp_key.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| format!("profile config '{}' is not an object", spec.mcp_key))?;
        for name in &plan.write {
            servers.insert(name.clone(), main_servers[name].clone());
        }
        for name in &plan.remove {
            servers.remove(name);
        }
        let mut rendered =
            serde_json::to_string_pretty(&profile_doc).context("serializing profile config")?;
        rendered.push('\n');
        crate::file_lock::write_private_atomic(profile_config, &rendered)?;
    }

    save_provenance(profile_home, &previous, &plan.provenance)?;
    Ok(plan.report)
}

/// The named object's entries, or empty when the key is absent or null.
fn json_servers(
    document: &serde_json::Value,
    key: &str,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let Some(value) = document.get(key) else {
        return Ok(BTreeMap::new());
    };
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let object = value
        .as_object()
        .with_context(|| format!("'{key}' is not an object"))?;
    Ok(object
        .iter()
        .map(|(name, definition)| (name.clone(), definition.clone()))
        .collect())
}

// ---------------------------------------------------------------------------
// TOML (Codex)
// ---------------------------------------------------------------------------

fn sync_toml(
    spec: &ToolSpec,
    main_text: &str,
    profile_config: &Path,
    profile_home: &Path,
) -> Result<SyncReport> {
    let main_doc: toml_edit::DocumentMut =
        main_text.parse().context("parsing main config as TOML")?;
    let main_servers = toml_servers(&main_doc, spec.mcp_key)?;

    let profile_text = read_optional(profile_config)?;
    if main_servers.is_empty() && profile_text.is_none() {
        return Ok(SyncReport::default());
    }
    let mut profile_doc: toml_edit::DocumentMut = profile_text
        .as_deref()
        .unwrap_or_default()
        .parse()
        .context("parsing profile config as TOML")?;
    let profile_servers = toml_servers(&profile_doc, spec.mcp_key)?;

    let previous = load_provenance(profile_home);
    let plan = plan(
        &fingerprints(&toml_values(&main_servers)?)?,
        &fingerprints(&toml_values(&profile_servers)?)?,
        &previous,
    );

    if plan.report.changed() {
        let entry = profile_doc.entry(spec.mcp_key).or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            // Implicit so servers render as `[mcp_servers.<name>]` without an
            // empty `[mcp_servers]` header above them.
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        });
        let servers = entry
            .as_table_like_mut()
            .with_context(|| format!("profile config '{}' is not a table", spec.mcp_key))?;
        for name in &plan.write {
            let mut item = main_servers[name].clone();
            // Main's comments and blank lines describe main; strip them so the
            // injected table renders the same however main happens to be laid out.
            strip_item_decor(&mut item);
            servers.insert(name, item);
        }
        for name in &plan.remove {
            servers.remove(name);
        }
        crate::file_lock::write_private_atomic(profile_config, &profile_doc.to_string())?;
    }

    save_provenance(profile_home, &previous, &plan.provenance)?;
    Ok(plan.report)
}

fn toml_servers(
    document: &toml_edit::DocumentMut,
    key: &str,
) -> Result<BTreeMap<String, toml_edit::Item>> {
    let Some(item) = document.get(key) else {
        return Ok(BTreeMap::new());
    };
    if item.is_none() {
        return Ok(BTreeMap::new());
    }
    let table = item
        .as_table_like()
        .with_context(|| format!("'{key}' is not a table"))?;
    Ok(table
        .iter()
        .map(|(name, definition)| (name.to_string(), definition.clone()))
        .collect())
}

fn toml_values(
    servers: &BTreeMap<String, toml_edit::Item>,
) -> Result<BTreeMap<String, serde_json::Value>> {
    servers
        .iter()
        .map(|(name, item)| Ok((name.clone(), item_to_json(item)?)))
        .collect()
}

/// Render a TOML item as JSON purely so both formats fingerprint the same way.
///
/// Datetimes become their RFC 3339 text; that only affects change detection,
/// never the TOML actually written into the profile, which is a clone of main's
/// own item.
fn item_to_json(item: &toml_edit::Item) -> Result<serde_json::Value> {
    Ok(match item {
        toml_edit::Item::None => serde_json::Value::Null,
        toml_edit::Item::Value(value) => toml_value_to_json(value)?,
        toml_edit::Item::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, child)| Ok((key.to_string(), item_to_json(child)?)))
                .collect::<Result<serde_json::Map<_, _>>>()?,
        ),
        toml_edit::Item::ArrayOfTables(tables) => serde_json::Value::Array(
            tables
                .iter()
                .map(|table| {
                    Ok(serde_json::Value::Object(
                        table
                            .iter()
                            .map(|(key, child)| Ok((key.to_string(), item_to_json(child)?)))
                            .collect::<Result<serde_json::Map<_, _>>>()?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
    })
}

fn toml_value_to_json(value: &toml_edit::Value) -> Result<serde_json::Value> {
    Ok(match value {
        toml_edit::Value::String(text) => serde_json::Value::String(text.value().clone()),
        toml_edit::Value::Integer(number) => serde_json::Value::from(*number.value()),
        toml_edit::Value::Float(number) => serde_json::Number::from_f64(*number.value())
            .map(serde_json::Value::Number)
            .context("TOML float is not finite")?,
        toml_edit::Value::Boolean(flag) => serde_json::Value::Bool(*flag.value()),
        toml_edit::Value::Datetime(stamp) => serde_json::Value::String(stamp.value().to_string()),
        toml_edit::Value::Array(array) => serde_json::Value::Array(
            array
                .iter()
                .map(toml_value_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        toml_edit::Value::InlineTable(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, child)| Ok((key.to_string(), toml_value_to_json(child)?)))
                .collect::<Result<serde_json::Map<_, _>>>()?,
        ),
    })
}

fn strip_item_decor(item: &mut toml_edit::Item) {
    match item {
        toml_edit::Item::None => {}
        toml_edit::Item::Value(value) => strip_value_decor(value),
        toml_edit::Item::Table(table) => strip_table_decor(table),
        toml_edit::Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                strip_table_decor(table);
            }
        }
    }
}

fn strip_table_decor(table: &mut toml_edit::Table) {
    table.decor_mut().clear();
    for (mut key, child) in table.iter_mut() {
        key.leaf_decor_mut().clear();
        key.dotted_decor_mut().clear();
        strip_item_decor(child);
    }
}

fn strip_value_decor(value: &mut toml_edit::Value) {
    value.decor_mut().clear();
    match value {
        toml_edit::Value::Array(array) => {
            for element in array.iter_mut() {
                strip_value_decor(element);
            }
        }
        toml_edit::Value::InlineTable(table) => {
            table.decor_mut().clear();
            for (mut key, child) in table.iter_mut() {
                key.leaf_decor_mut().clear();
                key.dotted_decor_mut().clear();
                strip_value_decor(child);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Fingerprints and provenance
// ---------------------------------------------------------------------------

fn fingerprints(servers: &BTreeMap<String, serde_json::Value>) -> Result<BTreeMap<String, String>> {
    servers
        .iter()
        .map(|(name, definition)| Ok((name.clone(), fingerprint(definition)?)))
        .collect()
}

/// FNV-1a/64 over the canonical JSON rendering of one server definition.
fn fingerprint(definition: &serde_json::Value) -> Result<String> {
    // `serde_json::Map` is a `BTreeMap` here, so this rendering is key-ordered
    // and therefore stable regardless of how the source file was written.
    let canonical =
        serde_json::to_string(definition).context("rendering a server definition for hashing")?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

/// Read recorded provenance, treating anything unreadable as "nothing is ours".
///
/// That is the safe direction: without a record every server counts as the
/// profile's own, so a damaged file can cost an update but never an overwrite.
fn load_provenance(profile_home: &Path) -> BTreeMap<String, String> {
    let path = profile_home.join(PROVENANCE_FILE);
    let Ok(Some(text)) = read_optional(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_str::<Provenance>(&text)
        .map(|provenance| provenance.servers)
        .unwrap_or_default()
}

fn save_provenance(
    profile_home: &Path,
    previous: &BTreeMap<String, String>,
    next: &BTreeMap<String, String>,
) -> Result<()> {
    let path = profile_home.join(PROVENANCE_FILE);
    if previous == next && path.exists() {
        return Ok(());
    }
    if next.is_empty() && !path.exists() {
        return Ok(());
    }
    let provenance = Provenance {
        version: PROVENANCE_VERSION,
        servers: next.clone(),
    };
    let mut rendered =
        serde_json::to_string_pretty(&provenance).context("serializing inherit provenance")?;
    rendered.push('\n');
    crate::file_lock::write_private_atomic(&path, &rendered)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn profile_config_path(spec: &ToolSpec, profile_home: &Path) -> PathBuf {
    let mut path = profile_home.to_path_buf();
    for segment in spec.profile_config_rel {
        path.push(segment);
    }
    path
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_specs::{CLAUDE, CODEX};

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        profile_home: PathBuf,
        paths: Paths,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let paths = Paths {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
            home_dir: home.clone(),
        };
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let profile_home = paths.ensure_profile_home_dir("codex", "work").unwrap();
        Fixture {
            _dir: dir,
            home,
            profile_home,
            paths,
        }
    }

    impl Fixture {
        fn main_config(&self, spec: &ToolSpec) -> PathBuf {
            self.paths.main_tool_config(spec)
        }

        fn write_main(&self, spec: &ToolSpec, text: &str) {
            std::fs::write(self.main_config(spec), text).unwrap();
        }

        fn profile_config(&self, spec: &ToolSpec) -> PathBuf {
            profile_config_path(spec, &self.profile_home)
        }

        fn write_profile(&self, spec: &ToolSpec, text: &str) {
            std::fs::write(self.profile_config(spec), text).unwrap();
        }

        fn read_profile(&self, spec: &ToolSpec) -> String {
            std::fs::read_to_string(self.profile_config(spec)).unwrap()
        }

        fn sync(&self, spec: &ToolSpec) -> Result<SyncReport> {
            sync(spec, &self.main_config(spec), &self.profile_home)
        }

        fn recorded(&self) -> BTreeMap<String, String> {
            load_provenance(&self.profile_home)
        }
    }

    fn tool(inherit_mcp: bool) -> Tool {
        Tool {
            command: vec!["codex".to_string()],
            args: Vec::new(),
            skills_source: None,
            copy: None,
            inherit_mcp,
            profiles: BTreeMap::new(),
        }
    }

    // -- injection ---------------------------------------------------------

    #[test]
    fn a_server_only_main_knows_about_is_injected_into_the_profile() {
        let f = fixture();
        f.write_main(
            &CODEX,
            "model = \"gpt-5\"\n\n[mcp_servers.browseros-neo]\nurl = \"http://127.0.0.1:9010/mcp\"\n",
        );

        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.injected, ["browseros-neo"]);
        let profile = f.read_profile(&CODEX);
        assert!(profile.contains("[mcp_servers.browseros-neo]"), "{profile}");
        assert!(
            profile.contains("url = \"http://127.0.0.1:9010/mcp\""),
            "{profile}"
        );
        // Only the MCP section crosses over.
        assert!(!profile.contains("model"), "{profile}");
        assert_eq!(f.recorded().keys().collect::<Vec<_>>(), ["browseros-neo"]);
    }

    #[test]
    fn injection_carries_nested_env_and_tool_subtables() {
        let f = fixture();
        f.write_main(
            &CODEX,
            r#"
[mcp_servers.node_repl]
command = "node"
args = ["--experimental-repl"]
startup_timeout_sec = 30

[mcp_servers.node_repl.env]
NODE_ENV = "production"

[mcp_servers.paper]
url = "https://paper.example/mcp"

[mcp_servers.paper.tools.write_html]
enabled = true
"#,
        );

        f.sync(&CODEX).unwrap();

        let profile = f.read_profile(&CODEX);
        for expected in [
            "[mcp_servers.node_repl]",
            "[mcp_servers.node_repl.env]",
            "NODE_ENV = \"production\"",
            "[mcp_servers.paper.tools.write_html]",
            "enabled = true",
        ] {
            assert!(profile.contains(expected), "missing {expected}:\n{profile}");
        }
        // The written TOML must itself round-trip as a config.
        profile.parse::<toml_edit::DocumentMut>().unwrap();
    }

    #[test]
    fn injection_preserves_unrelated_profile_sections_and_comments() {
        let f = fixture();
        f.write_profile(
            &CODEX,
            "# profile-only header\nmodel = \"gpt-profile\"\n\n[projects.\"/tmp/x\"]\ntrust_level = \"trusted\"\n",
        );
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");

        f.sync(&CODEX).unwrap();

        let profile = f.read_profile(&CODEX);
        for preserved in [
            "# profile-only header",
            "model = \"gpt-profile\"",
            "[projects.\"/tmp/x\"]",
            "trust_level = \"trusted\"",
        ] {
            assert!(profile.contains(preserved), "lost {preserved}:\n{profile}");
        }
        assert!(profile.contains("[mcp_servers.context7]"), "{profile}");
    }

    #[test]
    fn claude_injection_only_touches_the_server_object() {
        let f = fixture();
        let profile_home = f.paths.ensure_profile_home_dir("claude", "work").unwrap();
        f.write_main(
            &CLAUDE,
            r#"{"userID":"main-user","mcpServers":{"granola":{"type":"http","url":"https://mcp.granola.ai/mcp"}}}"#,
        );
        std::fs::write(
            profile_config_path(&CLAUDE, &profile_home),
            r#"{"userID":"profile-user","oauthAccount":{"emailAddress":"profile@example.com"}}"#,
        )
        .unwrap();

        let report = sync(&CLAUDE, &f.main_config(&CLAUDE), &profile_home).unwrap();

        assert_eq!(report.injected, ["granola"]);
        let profile: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(profile_config_path(&CLAUDE, &profile_home)).unwrap(),
        )
        .unwrap();
        // Identity stays the profile's own; only mcpServers arrived.
        assert_eq!(profile["userID"], "profile-user");
        assert_eq!(
            profile["oauthAccount"]["emailAddress"],
            "profile@example.com"
        );
        assert_eq!(
            profile["mcpServers"]["granola"]["url"],
            "https://mcp.granola.ai/mcp"
        );
    }

    // -- updates and user edits -------------------------------------------

    #[test]
    fn an_rtr_managed_server_follows_main_when_main_changes() {
        let f = fixture();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");
        f.sync(&CODEX).unwrap();

        f.write_main(
            &CODEX,
            "[mcp_servers.context7]\ncommand = \"bunx\"\nargs = [\"-y\"]\n",
        );
        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.updated, ["context7"]);
        assert!(report.injected.is_empty());
        let profile = f.read_profile(&CODEX);
        assert!(profile.contains("command = \"bunx\""), "{profile}");
        assert!(profile.contains("args = [\"-y\"]"), "{profile}");
    }

    #[test]
    fn a_profile_customized_server_survives_a_main_change() {
        let f = fixture();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");
        f.sync(&CODEX).unwrap();

        // The profile customizes rtr's copy...
        f.write_profile(
            &CODEX,
            "[mcp_servers.context7]\ncommand = \"npx\"\nargs = [\"--profile-only\"]\n",
        );
        // ...and main moves on independently.
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"deno\"\n");
        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.kept, ["context7"]);
        assert!(report.updated.is_empty());
        assert!(!report.changed());
        let profile = f.read_profile(&CODEX);
        assert!(profile.contains("args = [\"--profile-only\"]"), "{profile}");
        assert!(!profile.contains("deno"), "{profile}");
        // rtr has disowned it, so later main changes leave it alone too.
        assert!(f.recorded().is_empty());
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"bun\"\n");
        f.sync(&CODEX).unwrap();
        assert!(!f.read_profile(&CODEX).contains("bun"), "{profile}");
    }

    #[test]
    fn a_hand_merged_server_without_provenance_is_treated_as_the_profiles_own() {
        let f = fixture();
        // Exactly the state left by today's manual merge: content in the
        // profile, no `.rtr-inherited.json` beside it.
        f.write_profile(
            &CODEX,
            "[mcp_servers.context7]\ncommand = \"hand-merged\"\n",
        );
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");

        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.kept, ["context7"]);
        assert!(f.read_profile(&CODEX).contains("command = \"hand-merged\""));
    }

    #[test]
    fn a_server_the_profile_alone_defines_is_never_removed() {
        let f = fixture();
        f.write_profile(&CODEX, "[mcp_servers.profile-only]\ncommand = \"mine\"\n");
        f.write_main(&CODEX, "[mcp_servers.shared]\ncommand = \"theirs\"\n");

        f.sync(&CODEX).unwrap();
        f.write_main(&CODEX, "");
        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.removed, ["shared"]);
        let profile = f.read_profile(&CODEX);
        assert!(profile.contains("profile-only"), "{profile}");
        assert!(!profile.contains("shared"), "{profile}");
    }

    // -- retraction --------------------------------------------------------

    #[test]
    fn removing_a_server_from_main_retracts_only_rtrs_own_copy() {
        let f = fixture();
        f.write_main(
            &CODEX,
            "[mcp_servers.keep]\ncommand = \"a\"\n\n[mcp_servers.drop]\ncommand = \"b\"\n",
        );
        f.sync(&CODEX).unwrap();

        f.write_main(&CODEX, "[mcp_servers.keep]\ncommand = \"a\"\n");
        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.removed, ["drop"]);
        let profile = f.read_profile(&CODEX);
        assert!(profile.contains("[mcp_servers.keep]"), "{profile}");
        assert!(!profile.contains("drop"), "{profile}");
        assert_eq!(f.recorded().keys().collect::<Vec<_>>(), ["keep"]);
    }

    #[test]
    fn a_customized_server_stays_when_main_drops_it() {
        let f = fixture();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");
        f.sync(&CODEX).unwrap();
        f.write_profile(&CODEX, "[mcp_servers.context7]\ncommand = \"mine-now\"\n");
        f.write_main(&CODEX, "");

        let report = f.sync(&CODEX).unwrap();

        assert!(report.removed.is_empty());
        assert!(f.read_profile(&CODEX).contains("mine-now"));
        assert!(f.recorded().is_empty());
    }

    // -- idempotency and robustness ---------------------------------------

    #[test]
    fn repeated_syncs_change_nothing_after_the_first() {
        let f = fixture();
        f.write_main(
            &CODEX,
            "[mcp_servers.a]\ncommand = \"a\"\n\n[mcp_servers.b]\ncommand = \"b\"\n\n[mcp_servers.b.env]\nK = \"v\"\n",
        );

        assert!(f.sync(&CODEX).unwrap().changed());
        let after_first = f.read_profile(&CODEX);
        let provenance = f.recorded();

        for _ in 0..3 {
            let report = f.sync(&CODEX).unwrap();
            assert!(!report.changed(), "{report:?}");
        }
        assert_eq!(f.read_profile(&CODEX), after_first);
        assert_eq!(f.recorded(), provenance);
    }

    #[test]
    fn a_missing_main_config_is_skipped_without_creating_a_profile_config() {
        let f = fixture();
        std::fs::remove_dir_all(f.home.join(".codex")).unwrap();

        let report = f.sync(&CODEX).unwrap();

        assert!(!report.changed());
        assert!(!f.profile_config(&CODEX).exists());
        assert!(!f.profile_home.join(PROVENANCE_FILE).exists());
        // A tool the user does not run leaves the home completely untouched.
        assert!(!f.profile_home.join(LOCK_FILE).exists());
    }

    #[test]
    fn a_malformed_main_config_fails_without_touching_the_profile() {
        let f = fixture();
        f.write_profile(&CODEX, "[mcp_servers.mine]\ncommand = \"mine\"\n");
        f.write_main(&CODEX, "this is not = = toml [[[");

        let error = f.sync(&CODEX).unwrap_err().to_string();

        assert!(error.contains("parsing main config as TOML"), "{error}");
        assert_eq!(
            f.read_profile(&CODEX),
            "[mcp_servers.mine]\ncommand = \"mine\"\n"
        );
    }

    #[test]
    fn a_malformed_claude_main_config_fails_without_touching_the_profile() {
        let f = fixture();
        let profile_home = f.paths.ensure_profile_home_dir("claude", "work").unwrap();
        std::fs::write(
            profile_config_path(&CLAUDE, &profile_home),
            "{\"userID\":\"x\"}",
        )
        .unwrap();
        f.write_main(&CLAUDE, "{ not json");

        let error = sync(&CLAUDE, &f.main_config(&CLAUDE), &profile_home)
            .unwrap_err()
            .to_string();

        assert!(error.contains("parsing main config as JSON"), "{error}");
        assert_eq!(
            std::fs::read_to_string(profile_config_path(&CLAUDE, &profile_home)).unwrap(),
            "{\"userID\":\"x\"}"
        );
    }

    #[test]
    fn a_main_config_without_any_mcp_section_leaves_a_fresh_home_untouched() {
        let f = fixture();
        f.write_main(&CODEX, "model = \"gpt-5\"\n[projects.\"/tmp\"]\n");

        assert!(!f.sync(&CODEX).unwrap().changed());
        assert!(!f.profile_config(&CODEX).exists());
    }

    #[test]
    fn damaged_provenance_downgrades_to_leaving_everything_alone() {
        let f = fixture();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");
        f.sync(&CODEX).unwrap();
        std::fs::write(f.profile_home.join(PROVENANCE_FILE), "{ truncated").unwrap();

        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"changed\"\n");
        let report = f.sync(&CODEX).unwrap();

        assert_eq!(report.kept, ["context7"]);
        assert!(f.read_profile(&CODEX).contains("command = \"npx\""));
    }

    // -- fresh profile and opt-out ----------------------------------------

    #[test]
    fn a_brand_new_profile_home_is_seeded_from_main() {
        let f = fixture();
        let fresh = f.paths.ensure_profile_home_dir("codex", "fresh").unwrap();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");
        assert!(!profile_config_path(&CODEX, &fresh).exists());

        let report = sync(&CODEX, &f.main_config(&CODEX), &fresh).unwrap();

        assert_eq!(report.injected, ["context7"]);
        assert!(std::fs::read_to_string(profile_config_path(&CODEX, &fresh))
            .unwrap()
            .contains("[mcp_servers.context7]"));
    }

    #[test]
    fn inherit_mcp_false_freezes_the_profile() {
        let f = fixture();
        f.write_main(&CODEX, "[mcp_servers.context7]\ncommand = \"npx\"\n");

        sync_profile_best_effort(&f.paths, &CODEX, &tool(false), &f.profile_home);
        assert!(!f.profile_config(&CODEX).exists());

        sync_profile_best_effort(&f.paths, &CODEX, &tool(true), &f.profile_home);
        assert!(f.read_profile(&CODEX).contains("[mcp_servers.context7]"));
    }

    #[test]
    fn a_broken_main_config_is_reported_but_never_propagated() {
        let f = fixture();
        f.write_main(&CODEX, "not [[ toml");

        // Best-effort syncing swallows the error so a launch still proceeds.
        sync_profile_best_effort(&f.paths, &CODEX, &tool(true), &f.profile_home);

        assert!(!f.profile_config(&CODEX).exists());
    }

    // -- fingerprints ------------------------------------------------------

    #[test]
    fn fingerprints_ignore_formatting_but_track_content() {
        let compact: toml_edit::DocumentMut = "[s]\nb = 2\na = 1\n".parse().unwrap();
        let spaced: toml_edit::DocumentMut = "# note\n[s]  # trailing\n\na   =   1\nb = 2\n"
            .parse()
            .unwrap();
        let different: toml_edit::DocumentMut = "[s]\na = 1\nb = 3\n".parse().unwrap();

        let print = |doc: &toml_edit::DocumentMut| {
            fingerprint(&item_to_json(doc.get("s").unwrap()).unwrap()).unwrap()
        };
        assert_eq!(print(&compact), print(&spaced));
        assert_ne!(print(&compact), print(&different));
    }

    #[test]
    fn equivalent_json_and_toml_definitions_fingerprint_alike() {
        let toml_doc: toml_edit::DocumentMut =
            "[s]\ncommand = \"npx\"\nargs = [\"-y\"]\n".parse().unwrap();
        let json: serde_json::Value =
            serde_json::from_str(r#"{"args":["-y"],"command":"npx"}"#).unwrap();

        assert_eq!(
            fingerprint(&item_to_json(toml_doc.get("s").unwrap()).unwrap()).unwrap(),
            fingerprint(&json).unwrap()
        );
    }
}
