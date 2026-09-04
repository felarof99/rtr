//! `rtr switch`: point a shell at one profile's isolated native home.
//!
//! Every other command launches a child, so it can express profile choice in
//! the child's environment directly. `switch` instead has to reach the *parent*
//! shell, which no child process can write to. rtr therefore emits `export`
//! lines and lets the shell apply them, either through the `rtr shell-init zsh`
//! wrapper or an explicit `eval "$(rtr switch <profile>)"`.
//!
//! The persisted selection lives in `state.toml` next to the rotation cursors
//! so a new shell can adopt the last switch without re-running preparation.

use std::ffi::OsString;

use anyhow::{bail, Context, Result};

use crate::config::{Config, Tool};
use crate::paths::Paths;
use crate::runner;
use crate::state::State;
use crate::tool_specs;

/// Environment variable naming the profile a tool is switched to.
///
/// Exported alongside the native home so a prompt or status line can show the
/// current identity without shelling out to rtr.
pub(crate) fn profile_label_env(tool: &str) -> String {
    format!("RTR_PROFILE_{}", tool.to_uppercase())
}

/// Return the per-shell switch, then the persisted switch when no shell label exists.
pub(crate) fn active_profile(paths: &Paths, tool: &str) -> Result<Option<String>> {
    active_profile_with(paths, tool, std::env::var_os(profile_label_env(tool)))
}

fn active_profile_with(
    paths: &Paths,
    tool: &str,
    shell_profile: Option<OsString>,
) -> Result<Option<String>> {
    if let Some(profile) = shell_profile {
        return profile
            .into_string()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", profile_label_env(tool)));
    }

    Ok(State::load(&paths.state_file())?
        .current_profile(tool)
        .map(str::to_string))
}

/// Ensure a fork destination has a usable isolated home.
pub(crate) fn validate_profile(config: &Config, tool: &str, profile: &str) -> Result<()> {
    let spec = tool_specs::get(tool)?;
    let selector = SwitchSelector {
        tool: Some(spec.name.to_string()),
        profile: profile.to_string(),
        command: Vec::new(),
    };
    let Some(configured) = config
        .tools
        .get(spec.name)
        .and_then(|configured_tool| configured_tool.profiles.get(profile))
    else {
        return Err(no_target_error(config, &selector, &[], &[]));
    };
    if !configured.enabled {
        return Err(no_target_error(config, &selector, &[spec.name], &[]));
    }
    if configured.bypass {
        return Err(no_target_error(config, &selector, &[], &[spec.name]));
    }
    Ok(())
}

/// One tool's resolved switch target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchTarget {
    pub tool: &'static str,
    pub profile: String,
    /// `(key, value)` pairs the shell must export, native home first.
    pub env: Vec<(String, String)>,
}

/// A parsed `rtr switch` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchSelector {
    pub tool: Option<String>,
    pub profile: String,
    pub command: Vec<String>,
}

/// Split `switch` positionals into an optional tool, a profile, and a command.
///
/// `rtr switch nit claudexxx` must keep working when `nit` is a profile and
/// `claudexxx` is a shell alias, so a leading `claude` / `codex` is only read as
/// a tool when another positional follows it to serve as the profile.
pub fn parse_selector(args: &[String], tool_flag: Option<&str>) -> Result<SwitchSelector> {
    let (first, rest) = args
        .split_first()
        .context("switch needs a profile: rtr switch <profile> [command...]")?;

    if let Some(tool) = tool_flag {
        let spec = tool_specs::get(tool)?;
        return Ok(SwitchSelector {
            tool: Some(spec.name.to_string()),
            profile: first.clone(),
            command: rest.to_vec(),
        });
    }

    if tool_specs::get(first).is_ok() {
        if let Some((profile, command)) = rest.split_first() {
            return Ok(SwitchSelector {
                tool: Some(first.clone()),
                profile: profile.clone(),
                command: command.to_vec(),
            });
        }
        bail!(
            "'{first}' is a tool, not a profile; use `rtr switch {first} <profile>` or `rtr switch <profile>`"
        );
    }

    Ok(SwitchSelector {
        tool: None,
        profile: first.clone(),
        command: rest.to_vec(),
    })
}

/// Resolve which tools a switch applies to and prepare each native home.
///
/// A bare profile switches every tool that configures it, because profile names
/// identify an account rather than a tool. Preparation reuses the launch path so
/// a switched home receives the same startup copy a launch would perform.
pub fn resolve_targets(
    paths: &Paths,
    config: &Config,
    selector: &SwitchSelector,
) -> Result<Vec<SwitchTarget>> {
    let requested: Vec<&'static str> = match &selector.tool {
        Some(tool) => vec![tool_specs::get(tool)?.name],
        None => tool_specs::all().iter().map(|spec| spec.name).collect(),
    };

    let mut targets = Vec::new();
    let mut disabled = Vec::new();
    let mut bypassed = Vec::new();

    for tool_name in requested {
        let Ok(tool) = config.tool(tool_name) else {
            continue;
        };
        let Some(profile) = tool.profiles.get(&selector.profile) else {
            continue;
        };
        if !profile.enabled {
            disabled.push(tool_name);
            continue;
        }
        if profile.bypass {
            bypassed.push(tool_name);
            continue;
        }
        targets.push(prepare_target(paths, tool_name, tool, &selector.profile)?);
    }

    if targets.is_empty() {
        return Err(no_target_error(config, selector, &disabled, &bypassed));
    }
    for tool_name in bypassed {
        eprintln!(
            "rtr: {tool_name}/{} is bypassed — not switched (undo: rtr unbypass {tool_name} --profile {})",
            selector.profile,
            runner::shell_quote(&selector.profile)
        );
    }
    Ok(targets)
}

fn prepare_target(
    paths: &Paths,
    tool_name: &'static str,
    tool: &Tool,
    profile: &str,
) -> Result<SwitchTarget> {
    let spec = tool_specs::get(tool_name)?;
    let env = runner::prepare_native_profile_env(paths, spec, tool, profile)?
        .into_iter()
        .map(|(key, value)| (key, value.to_string_lossy().into_owned()))
        .collect::<Vec<_>>();
    let mut env = env;
    env.push((profile_label_env(tool_name), profile.to_string()));
    Ok(SwitchTarget {
        tool: spec.name,
        profile: profile.to_string(),
        env,
    })
}

fn no_target_error(
    config: &Config,
    selector: &SwitchSelector,
    disabled: &[&'static str],
    bypassed: &[&'static str],
) -> anyhow::Error {
    let profile = &selector.profile;
    if !disabled.is_empty() {
        let list = disabled.join(", ");
        return anyhow::anyhow!(
            "profile '{profile}' is disabled for {list}; enable it with `rtr enable <tool> --profile {}`",
            runner::shell_quote(profile)
        );
    }
    if !bypassed.is_empty() {
        let list = bypassed.join(", ");
        return anyhow::anyhow!(
            "profile '{profile}' is bypassed for {list}, so it has no isolated home to switch to; undo with `rtr unbypass <tool> --profile {}`",
            runner::shell_quote(profile)
        );
    }
    let mut known: Vec<String> = Vec::new();
    for spec in tool_specs::all() {
        if let Ok(tool) = config.tool(spec.name) {
            for name in tool.profiles.keys() {
                known.push(format!("{}/{name}", spec.name));
            }
        }
    }
    if known.is_empty() {
        return anyhow::anyhow!(
            "no profile '{profile}' is configured; create one with `rtr add <tool> --profile {}`",
            runner::shell_quote(profile)
        );
    }
    anyhow::anyhow!(
        "no profile '{profile}' is configured (known: {}); create one with `rtr add <tool> --profile {}`",
        known.join(", "),
        runner::shell_quote(profile)
    )
}

/// Persist the switched profile for each tool so later shells adopt it.
pub fn persist(paths: &Paths, targets: &[SwitchTarget]) -> Result<()> {
    State::update_locked(&paths.state_file(), |state| {
        for target in targets {
            state.set_current_profile(target.tool, &target.profile);
        }
        Ok(())
    })
}

/// Render the `export` lines a shell must evaluate for these targets.
pub fn render_exports(targets: &[SwitchTarget]) -> String {
    let mut out = String::new();
    for target in targets {
        for (key, value) in &target.env {
            out.push_str(&format!("export {key}={}\n", runner::shell_quote(value)));
        }
    }
    out
}

/// Render a `__rtr_cmd` array assignment for the shell wrapper.
///
/// The first word is emitted unquoted so zsh still performs alias expansion on
/// it; that is the whole point of `rtr switch <profile> <alias>`. Later words
/// are quoted because they are data, not command names.
pub fn render_command_array(command: &[String]) -> String {
    if command.is_empty() {
        return String::new();
    }
    let mut parts = Vec::with_capacity(command.len());
    for (index, word) in command.iter().enumerate() {
        if index == 0 {
            parts.push(word.clone());
        } else {
            parts.push(runner::shell_quote(word));
        }
    }
    format!("__rtr_cmd=({})\n", parts.join(" "))
}

/// Join a command for `$SHELL -i -c`, keeping the first word alias-expandable.
pub fn render_shell_command(command: &[String]) -> String {
    let mut parts = Vec::with_capacity(command.len());
    for (index, word) in command.iter().enumerate() {
        if index == 0 {
            parts.push(word.clone());
        } else {
            parts.push(runner::shell_quote(word));
        }
    }
    parts.join(" ")
}

/// Human-readable confirmation, written to stderr so stdout stays evaluable.
pub fn render_summary(targets: &[SwitchTarget]) -> String {
    let list = targets
        .iter()
        .map(|target| format!("{} → {}", target.tool, target.profile))
        .collect::<Vec<_>>()
        .join(", ");
    format!("rtr: switched {list}")
}

/// Exports for the persisted switch, used to seed a newly started shell.
///
/// Preparation is deliberately skipped here: this runs on every shell startup,
/// and a missing home is dropped rather than silently recreated.
pub fn render_restore(paths: &Paths, state: &State) -> String {
    let mut out = String::new();
    for spec in tool_specs::all() {
        let Some(profile) = state.current_profile(spec.name) else {
            continue;
        };
        let home = paths.profile_home_dir(spec.name, profile);
        if !home.is_dir() {
            continue;
        }
        let quoted = runner::shell_quote(&home.to_string_lossy());
        out.push_str(&format!("export {}={quoted}\n", spec.native_home_env));
        if let Some(key) = spec.native_secure_storage_env {
            out.push_str(&format!("export {key}={quoted}\n"));
        }
        out.push_str(&format!(
            "export {}={}\n",
            profile_label_env(spec.name),
            runner::shell_quote(profile)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_profile_keeps_trailing_command() {
        let selector = parse_selector(&v(&["nit", "claudexxx"]), None).unwrap();
        assert_eq!(selector.tool, None);
        assert_eq!(selector.profile, "nit");
        assert_eq!(selector.command, v(&["claudexxx"]));
    }

    #[test]
    fn leading_tool_name_selects_one_tool() {
        let selector =
            parse_selector(&v(&["claude", "nit", "claudexxx", "--resume"]), None).unwrap();
        assert_eq!(selector.tool.as_deref(), Some("claude"));
        assert_eq!(selector.profile, "nit");
        assert_eq!(selector.command, v(&["claudexxx", "--resume"]));
    }

    #[test]
    fn lone_tool_name_is_rejected_with_guidance() {
        let error = parse_selector(&v(&["codex"]), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("is a tool, not a profile"), "{error}");
        assert!(error.contains("rtr switch codex <profile>"), "{error}");
    }

    #[test]
    fn tool_flag_keeps_a_tool_named_profile_addressable() {
        let selector = parse_selector(&v(&["claude", "codexxx"]), Some("codex")).unwrap();
        assert_eq!(selector.tool.as_deref(), Some("codex"));
        assert_eq!(selector.profile, "claude");
        assert_eq!(selector.command, v(&["codexxx"]));
    }

    #[test]
    fn missing_profile_is_rejected() {
        assert!(parse_selector(&[], None).is_err());
    }

    #[test]
    fn command_array_leaves_the_alias_unquoted() {
        let rendered = render_command_array(&v(&["claudexxx", "--resume", "my thread"]));
        assert_eq!(rendered, "__rtr_cmd=(claudexxx --resume 'my thread')\n");
    }

    #[test]
    fn empty_command_renders_no_array() {
        assert_eq!(render_command_array(&[]), "");
    }

    #[test]
    fn shell_command_leaves_the_alias_unquoted() {
        let rendered = render_shell_command(&v(&["claudexxx", "--model", "fable", "two words"]));
        assert_eq!(rendered, "claudexxx --model fable 'two words'");
    }

    #[test]
    fn exports_quote_only_values_that_need_it() {
        let targets = vec![SwitchTarget {
            tool: "claude",
            profile: "nit".into(),
            env: vec![
                ("CLAUDE_CONFIG_DIR".into(), "/homes/claude/nit".into()),
                ("RTR_PROFILE_CLAUDE".into(), "nit".into()),
            ],
        }];
        assert_eq!(
            render_exports(&targets),
            "export CLAUDE_CONFIG_DIR=/homes/claude/nit\nexport RTR_PROFILE_CLAUDE=nit\n"
        );
    }

    #[test]
    fn exports_quote_a_home_containing_spaces() {
        let targets = vec![SwitchTarget {
            tool: "codex",
            profile: "my profile".into(),
            env: vec![("CODEX_HOME".into(), "/homes/codex/my profile".into())],
        }];
        assert_eq!(
            render_exports(&targets),
            "export CODEX_HOME='/homes/codex/my profile'\n"
        );
    }

    #[test]
    fn summary_lists_every_switched_tool() {
        let targets = vec![
            SwitchTarget {
                tool: "claude",
                profile: "eng".into(),
                env: Vec::new(),
            },
            SwitchTarget {
                tool: "codex",
                profile: "eng".into(),
                env: Vec::new(),
            },
        ];
        assert_eq!(
            render_summary(&targets),
            "rtr: switched claude → eng, codex → eng"
        );
    }

    #[test]
    fn restore_skips_tools_without_a_created_home() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            state_dir: dir.path().join("state"),
            home_dir: dir.path().join("home"),
        };
        let mut state = State::default();
        state.set_current_profile("claude", "nit");
        assert_eq!(render_restore(&paths, &state), "");

        let home = paths.ensure_profile_home_dir("claude", "nit").unwrap();
        let rendered = render_restore(&paths, &state);
        assert!(
            rendered.contains(&format!(
                "export CLAUDE_CONFIG_DIR={}",
                runner::shell_quote(&home.to_string_lossy())
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains("export CLAUDE_SECURESTORAGE_CONFIG_DIR="),
            "{rendered}"
        );
        assert!(
            rendered.contains("export RTR_PROFILE_CLAUDE=nit"),
            "{rendered}"
        );
        // Codex was never switched, so it must not leak into the shell.
        assert!(!rendered.contains("CODEX_HOME"), "{rendered}");
    }

    #[test]
    fn tests_that_shell_profile_precedes_persisted_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            state_dir: dir.path().join("state"),
            home_dir: dir.path().join("home"),
        };
        State::update_locked(&paths.state_file(), |state| {
            state.set_current_profile("claude", "persisted");
            Ok(())
        })
        .unwrap();

        assert_eq!(
            active_profile_with(&paths, "claude", Some(OsString::from("shell"))).unwrap(),
            Some("shell".to_string())
        );
    }

    #[test]
    fn tests_that_persisted_profile_is_used_without_a_shell_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            state_dir: dir.path().join("state"),
            home_dir: dir.path().join("home"),
        };
        State::update_locked(&paths.state_file(), |state| {
            state.set_current_profile("codex", "persisted");
            Ok(())
        })
        .unwrap();

        assert_eq!(
            active_profile_with(&paths, "codex", None).unwrap(),
            Some("persisted".to_string())
        );
        assert_eq!(active_profile_with(&paths, "claude", None).unwrap(), None);
    }

    #[test]
    fn tests_that_unusable_fork_targets_are_rejected() {
        let config = Config::parse(
            r#"
[tools.claude]
command = ["claude"]
[tools.claude.profiles.ready]
[tools.claude.profiles.disabled]
enabled = false
[tools.claude.profiles.bypassed]
bypass = true
"#,
        )
        .unwrap();

        validate_profile(&config, "claude", "ready").unwrap();
        for (profile, expected) in [
            ("disabled", "profile 'disabled' is disabled for claude"),
            ("bypassed", "profile 'bypassed' is bypassed for claude"),
            ("missing", "no profile 'missing' is configured"),
        ] {
            let error = validate_profile(&config, "claude", profile)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
