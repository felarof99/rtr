//! Command layer for `rtr switch` and `rtr shell-init`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::cli::SwitchArgs;
use crate::config::Config;
use crate::paths::Paths;
use crate::runner;
use crate::state::State;
use crate::switch;

/// zsh integration: apply `rtr switch` to the running shell.
///
/// Only `switch` is intercepted; every other subcommand goes straight to the
/// binary, so the wrapper cannot drift from the real command surface. The
/// trailing command is evaluated in the caller's shell rather than a child, so
/// aliases and shell functions resolve exactly as when typed by hand.
const ZSH_INIT: &str = r#"rtr() {
  if [ "$1" = switch ]; then
    shift
    local __rtr_script
    __rtr_script="$(command rtr switch --emit-zsh "$@")" || return $?
    local -a __rtr_cmd
    __rtr_cmd=()
    eval "$__rtr_script"
    if (( ${#__rtr_cmd[@]} )); then
      eval "${__rtr_cmd[@]}"
    fi
  else
    command rtr "$@"
  fi
}
alias rtrs='rtr switch'
"#;

pub async fn run_switch(paths: &Paths, args: SwitchArgs) -> Result<i32> {
    let config_path = paths.config_file();
    if !config_path.exists() {
        bail!(
            "no config at {} — run `rtr init` first",
            config_path.display()
        );
    }
    let config = Config::load(&config_path)?;
    let selector = switch::parse_selector(&args.args, args.tool.as_deref())?;
    let targets = switch::resolve_targets(paths, &config, &selector)?;
    switch::persist(paths, &targets)?;

    if args.emit_zsh {
        print!("{}", switch::render_exports(&targets));
        print!("{}", switch::render_command_array(&selector.command));
        eprintln!("{}", switch::render_summary(&targets));
        return Ok(0);
    }

    eprintln!("{}", switch::render_summary(&targets));
    if selector.command.is_empty() {
        print!("{}", switch::render_exports(&targets));
        eprintln!(
            "rtr: this shell is unchanged — apply with `eval \"$(rtr switch {})\"`, or add `eval \"$(rtr shell-init zsh)\"` to ~/.zshrc",
            runner::shell_quote(&selector.profile)
        );
        return Ok(0);
    }
    run_switched_command(&targets, &selector.command).await
}

/// Run a trailing command with the switched environment applied.
///
/// A real executable is spawned directly so it keeps rtr's terminal and signal
/// behavior. Anything else is assumed to be a shell alias or function, which
/// only the user's interactive shell can resolve.
async fn run_switched_command(targets: &[switch::SwitchTarget], command: &[String]) -> Result<i32> {
    let env: Vec<(String, std::ffi::OsString)> = targets
        .iter()
        .flat_map(|target| target.env.iter())
        .map(|(key, value)| (key.clone(), std::ffi::OsString::from(value)))
        .collect();

    let program = &command[0];
    if let Some(path) = find_executable(program) {
        return runner::execute_program(
            path.to_string_lossy().as_ref(),
            &[],
            command[1..].to_vec(),
            env,
            Vec::new(),
            None,
        )
        .await;
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let script = switch::render_shell_command(command);
    runner::execute_program(
        &shell,
        &["-i".to_string(), "-c".to_string()],
        vec![script],
        env,
        Vec::new(),
        None,
    )
    .await
    .with_context(|| format!("running '{program}' through {shell}"))
}

/// Locate an executable the way a shell would, without resolving aliases.
fn find_executable(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn print_shell_init(paths: &Paths, shell: &str) -> Result<()> {
    match shell {
        "zsh" => {
            let state = State::load(&paths.state_file())?;
            print!("{ZSH_INIT}");
            print!("{}", switch::render_restore(paths, &state));
            Ok(())
        }
        other => bail!("unsupported shell '{other}' (supported: zsh)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_init_defines_the_wrapper_and_shortcut() {
        assert!(ZSH_INIT.contains("rtr() {"), "{ZSH_INIT}");
        assert!(
            ZSH_INIT.contains("command rtr switch --emit-zsh"),
            "{ZSH_INIT}"
        );
        assert!(ZSH_INIT.contains("alias rtrs='rtr switch'"), "{ZSH_INIT}");
        // Every non-switch command must reach the real binary unchanged.
        assert!(ZSH_INIT.contains("command rtr \"$@\""), "{ZSH_INIT}");
    }

    #[test]
    fn shell_init_rejects_unsupported_shells() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            state_dir: dir.path().join("state"),
            home_dir: dir.path().join("home"),
        };
        let error = print_shell_init(&paths, "fish").unwrap_err().to_string();
        assert!(error.contains("unsupported shell 'fish'"), "{error}");
    }

    #[test]
    fn shell_init_emits_the_persisted_switch() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            state_dir: dir.path().join("state"),
            home_dir: dir.path().join("home"),
        };
        State::update_locked(&paths.state_file(), |state| {
            state.set_current_profile("codex", "eng");
            Ok(())
        })
        .unwrap();
        paths.ensure_profile_home_dir("codex", "eng").unwrap();

        let state = State::load(&paths.state_file()).unwrap();
        let restore = switch::render_restore(&paths, &state);
        assert!(restore.contains("export CODEX_HOME="), "{restore}");
        assert!(
            restore.contains("export RTR_PROFILE_CODEX=eng"),
            "{restore}"
        );
    }

    #[test]
    fn absolute_and_path_lookups_agree_with_the_shell() {
        assert_eq!(
            find_executable("/bin/sh"),
            Some(PathBuf::from("/bin/sh")),
            "absolute executable"
        );
        assert_eq!(find_executable("/bin/definitely-not-here"), None);
        assert!(
            find_executable("sh").is_some(),
            "sh should resolve through PATH"
        );
        // Aliases are shell-only constructs and must fall through to $SHELL.
        assert_eq!(find_executable("claudexxx-not-a-real-binary"), None);
    }
}
