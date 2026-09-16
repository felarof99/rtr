//! Percentage command and presentation shared with profile inspection. Edits
//! hold only the config lock; displaying allocations never touches scheduling
//! state, so inspecting shares cannot consume a launch or restart rotation.
use std::fmt::Write as _;

use anyhow::Result;

use crate::{
    cli::WeightArgs,
    config::{self, Config, Profile},
    output::{display_width, Style, Tone},
    paths::Paths,
    selection::{self, Allocation},
    tool_specs,
};

pub fn run(paths: &Paths, args: &WeightArgs) -> Result<()> {
    if let Some(tool_name) = args.tool.as_deref() {
        tool_specs::get(tool_name)?;
    }
    let path = paths.config_file();
    let config = if args.reset || args.percent.is_some() {
        crate::file_lock::with_exclusive_lock(&crate::file_lock::lock_path(&path), || {
            let mut config = Config::load(&path)?;
            // Clap requires a tool for mutations and a profile for percentages.
            // Keep the transaction's snapshot for the report instead of rereading
            // after unlocking and possibly showing another command's change.
            config::set_profile_shares_in_file(
                &path,
                &mut config,
                args.tool.as_deref().expect("mutation requires a tool"),
                args.profile.as_deref().zip(args.percent),
            )?;
            Ok(config)
        })?
    } else {
        Config::load(&path)?
    };
    if let Some(tool_name) = args.tool.as_deref() {
        config.tool(tool_name)?;
    }
    print!(
        "{}",
        render(&config, args.tool.as_deref(), args.output.color.stdout())
    );
    Ok(())
}

pub(crate) fn format_percent(percent: f64) -> String {
    let number = format!("{percent:.2}");
    format!("{}%", number.trim_end_matches('0').trim_end_matches('.'))
}

pub(crate) fn share_label(
    name: &str,
    profile: &Profile,
    allocation: Option<&Allocation>,
) -> String {
    if !profile.enabled {
        return "0%".into();
    }
    allocation
        .and_then(|allocation| allocation.percent(name))
        .map_or_else(|| "-".into(), format_percent)
}

fn render(config: &Config, only_tool: Option<&str>, style: Style) -> String {
    let headers = ["AGENT", "PROFILE", "SHARE", "SETTING", "STATE"];
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for (tool_name, tool) in &config.tools {
        if only_tool.is_some_and(|only| only != tool_name) {
            continue;
        }
        let allocation = selection::allocation(tool_name, tool);
        for (name, profile) in &tool.profiles {
            rows.push([
                tool_name.clone(),
                name.clone(),
                share_label(name, profile, allocation.as_ref().ok()),
                profile
                    .share_percent
                    .map_or_else(|| "remainder".into(), |value| format!("fixed {value}%")),
                if profile.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
                .into(),
            ]);
        }
        if let Err(error) = allocation {
            notes.push(format!("Automatic selection unavailable: {error}"));
        } else if tool.profiles.values().all(|profile| !profile.enabled) {
            notes.push(format!(
                "Automatic selection unavailable: tool '{tool_name}' has no enabled profiles"
            ));
        }
    }
    let mut out = format!(
        "{}\n\n",
        style.paint("Automatic launch shares", Tone::Strong)
    );
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            style.paint("No configured profiles.", Tone::Muted)
        );
    } else {
        let mut widths = headers.map(str::len);
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
        let _ = writeln!(
            out,
            "{}",
            headers
                .iter()
                .enumerate()
                .map(|(i, text)| style.padded(text, Tone::Muted, widths[i], i == 2))
                .collect::<Vec<_>>()
                .join("  ")
        );
        for row in rows {
            let _ = writeln!(
                out,
                "{}",
                row.iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let tone = if row[4] == "disabled" {
                            Tone::Muted
                        } else if i == 2 {
                            Tone::Accent
                        } else {
                            Tone::Normal
                        };
                        style.padded(text, tone, widths[i], i == 2)
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            );
        }
    }
    for note in notes {
        let _ = writeln!(out, "\n{}", style.paint(&note, Tone::Warning));
    }
    let _ = writeln!(out, "\n{}", style.paint("Profiles without an override split the remainder equally. Shares apply to automatic launches, not quota or elapsed time.", Tone::Muted));
    out
}
