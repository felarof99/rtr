//! Exercise allocation edits and scheduling through separate RTR processes.
//! Disposable config/state and a trivial child keep real subscriptions untouched.
use std::process::{Command, Output};

use rtr::{config::Config, paths::Paths, usage};

struct Fixture {
    _root: tempfile::TempDir,
    paths: Paths,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("config"),
            state_dir: root.path().join("state"),
            home_dir: root.path().join("home"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.config_file(),
            concat!(
                "# preserve this file\n[tools.codex]\ncommand=['true']\ncopy=[]\n",
                "[tools.codex.profiles.eng]\n# eng settings\n",
                "[tools.codex.profiles.nik]\n",
                "[tools.codex.profiles.\"work team\"]\nbypass=false # keep\n",
                "[tools.claude]\ncommand=['true']\ncopy=[]\n[tools.claude.profiles.other]\n"
            ),
        )
        .unwrap();
        Self { _root: root, paths }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rtr"));
        command
            .args(args)
            .env("RTR_CONFIG_DIR", &self.paths.config_dir)
            .env("RTR_STATE_DIR", &self.paths.state_dir)
            .env("NO_COLOR", "1");
        command
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn failed(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(!output.status.success(), "unexpected success for {args:?}");
        output
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.paths.config_file()).unwrap()
    }
}

#[test]
fn weight_edits_show_fixed_percentages_and_the_equal_remainder_then_reset() {
    let f = Fixture::new();
    let default = f.run(&["weight"]);
    assert!(default.contains("claude"), "{default}");
    assert!(default.contains("33.33%"), "{default}");
    let changed = f.run(&["weight", "codex", "--profile", "nik", "25%"]);
    assert!(changed.contains("25%"), "{changed}");
    assert_eq!(changed.matches("37.5%").count(), 2, "{changed}");
    assert!(!changed.contains("claude"), "{changed}");
    f.run(&["weight", "codex", "--profile", "eng", "50"]);
    let config = Config::load(&f.paths.config_file())
        .unwrap()
        .to_toml()
        .unwrap();
    assert!(config.contains("share_percent = 25"), "{config}");
    assert!(config.contains("share_percent = 50"), "{config}");
    assert!(f.config().contains("# preserve this file"));
    assert!(f.config().contains("# eng settings"));
    assert!(f.config().contains("bypass=false # keep"));
    let reset = f.run(&["weight", "codex", "--reset"]);
    assert_eq!(reset.matches("33.33%").count(), 3, "{reset}");
    assert!(!f.config().contains("share_percent"));
    assert!(
        !f.paths.state_file().exists(),
        "editing must not own scheduling state"
    );
}

#[test]
fn invalid_weight_commands_do_not_change_configuration() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    let original = f.config();
    for args in [
        vec!["weight", "codex", "--profile", "eng", "80"],
        vec!["weight", "codex", "--profile", "eng", "101"],
        vec!["weight", "codex", "--profile", "eng", "-1"],
        vec!["weight", "codex", "--profile", "eng", "25.5"],
        vec!["weight", "codex", "--profile", "eng", "auto"],
        vec!["weight", "codex", "--profile", "ghost", "25"],
        vec!["weight", "codex", "25"],
        vec!["weight", "codex", "--profile", "nik"],
        vec!["weight", "--reset"],
        vec!["weight", "codex", "--reset", "--profile", "nik", "25"],
        vec!["weight", "curl"],
    ] {
        f.failed(&args);
        assert_eq!(f.config(), original, "changed config for {args:?}");
    }
    f.run(&["weight", "codex", "--profile", "eng", "50"]);
    let original = f.config();
    let output = f.failed(&["weight", "codex", "--profile", "work team", "20"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("100%"));
    assert_eq!(f.config(), original);
}

#[test]
fn weighted_launches_persist_across_processes_and_explicit_runs_leave_schedule_alone() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    for _ in 0..8 {
        f.run(&["codex"]);
    }
    let stats = usage::aggregate(&usage::read_events(&f.paths.usage_file()).unwrap(), None);
    assert_eq!(stats["codex"]["nik"].runs, 2);
    assert_eq!(stats["codex"]["eng"].runs, 3);
    assert_eq!(stats["codex"]["work team"].runs, 3);
    let before = std::fs::read(f.paths.state_file()).unwrap();
    f.run(&["codex", "--profile", "nik"]);
    assert_eq!(std::fs::read(f.paths.state_file()).unwrap(), before);
    let overview = f.run(&["ls"]);
    assert!(overview.contains("SHARE"), "{overview}");
    assert!(overview.contains("37.5%"), "{overview}");
    assert!(f
        .run(&["show", "codex", "--profile", "nik"])
        .contains("25%"));
}

#[test]
fn zero_share_is_manual_only_and_concurrent_launches_keep_the_distribution() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "0"]);
    for _ in 0..4 {
        f.run(&["codex"]);
    }
    let events = usage::read_events(&f.paths.usage_file()).unwrap();
    assert!(events.iter().all(|event| event.profile != "nik"));
    f.run(&["codex", "--profile", "nik"]);
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    let children: Vec<_> = (0..24)
        .map(|_| {
            f.command(&["codex"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let events = usage::read_events(&f.paths.usage_file()).unwrap();
    let stats = usage::aggregate(&events[5..], None);
    assert_eq!(stats["codex"]["nik"].runs, 6);
    assert_eq!(stats["codex"]["eng"].runs, 9);
    assert_eq!(stats["codex"]["work team"].runs, 9);
}

#[test]
fn disabling_profiles_never_inflates_a_fixed_share_and_reset_recovers() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    f.run(&["disable", "codex", "--profile", "eng"]);
    let shown = f.run(&["weight", "codex"]);
    assert!(shown.contains("75%"), "{shown}");
    assert!(shown.contains("disabled"), "{shown}");
    f.run(&["disable", "codex", "--profile", "work team"]);
    assert!(f.run(&["weight", "codex"]).contains("unavailable"));
    f.failed(&["codex"]);
    f.run(&["codex", "--profile", "nik"]);
    assert!(f.run(&["weight", "codex", "--reset"]).contains("100%"));
    f.run(&["codex"]);
}

#[test]
fn concurrent_share_edits_preserve_comments_quoted_names_and_private_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut first = f
        .command(&["weight", "codex", "--profile", "nik", "25"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut second = f
        .command(&["weight", "codex", "--profile", "work team", "25"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    let config = Config::load(&f.paths.config_file()).unwrap();
    assert_eq!(
        config.tool("codex").unwrap().profiles["nik"].share_percent,
        Some(25)
    );
    assert_eq!(
        config.tool("codex").unwrap().profiles["work team"].share_percent,
        Some(25)
    );
    assert!(f.config().contains("bypass=false # keep"));
    assert_eq!(
        std::fs::metadata(f.paths.config_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let original = f
        .config()
        .replace("share_percent = 25", "share_percent = 25 # low quota");
    std::fs::write(f.paths.config_file(), original).unwrap();
    f.run(&["weight", "codex", "--profile", "nik", "20"]);
    assert!(f.config().contains("share_percent = 20 # low quota"));
    let before = f.config();
    f.run(&["weight", "codex", "--profile", "nik", "20"]);
    assert_eq!(f.config(), before);
}

#[test]
fn enable_rejects_conflicting_retained_share_and_reset_includes_disabled_profiles() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    f.run(&["disable", "codex", "--profile", "nik"]);
    f.run(&["weight", "codex", "--profile", "eng", "100"]);
    let original = f.config();
    f.failed(&["enable", "codex", "--profile", "nik"]);
    assert_eq!(f.config(), original);
    for _ in 0..4 {
        f.run(&["codex"]);
    }
    assert!(usage::read_events(&f.paths.usage_file())
        .unwrap()
        .iter()
        .all(|event| event.profile == "eng"));
    f.run(&["weight", "codex", "--reset"]);
    f.run(&["enable", "codex", "--profile", "nik"]);
    assert!(!f.config().contains("share_percent"));
    assert_eq!(f.run(&["weight", "codex"]).matches("33.33%").count(), 3);
}

#[test]
fn invalid_hand_edited_totals_can_be_inspected_and_reset_without_breaking_explicit_runs() {
    let f = Fixture::new();
    let config = f
        .config()
        .replace(
            "[tools.codex.profiles.eng]",
            "[tools.codex.profiles.eng]\nshare_percent=80",
        )
        .replace(
            "[tools.codex.profiles.nik]",
            "[tools.codex.profiles.nik]\nshare_percent=80",
        );
    std::fs::write(f.paths.config_file(), config).unwrap();
    assert!(f.run(&["weight"]).contains("exceeding 100%"));
    assert!(f.run(&["ls"]).contains("unavailable"));
    f.failed(&["codex"]);
    f.run(&["codex", "--profile", "nik"]);
    f.run(&["weight", "codex", "--reset"]);
    f.run(&["codex"]);
}

#[test]
fn preparation_failure_does_not_consume_weighted_selection() {
    let f = Fixture::new();
    f.run(&["weight", "codex", "--profile", "nik", "25"]);
    f.run(&["codex"]);
    let before = std::fs::read(f.paths.state_file()).unwrap();
    let original = f.config();
    std::fs::write(
        f.paths.config_file(),
        original.replacen(
            "copy=[]",
            "copy=[{source='missing-source',destination='skills'}]",
            1,
        ),
    )
    .unwrap();
    f.failed(&["codex"]);
    assert_eq!(std::fs::read(f.paths.state_file()).unwrap(), before);
    std::fs::write(f.paths.config_file(), original).unwrap();
    for _ in 0..7 {
        f.run(&["codex"]);
    }
    let stats = usage::aggregate(&usage::read_events(&f.paths.usage_file()).unwrap(), None);
    assert_eq!(stats["codex"]["nik"].runs, 2);
    assert_eq!(stats["codex"]["eng"].runs, 3);
    assert_eq!(stats["codex"]["work team"].runs, 3);
}
