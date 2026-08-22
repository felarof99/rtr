# Design

## Goal

rtr launches Claude Code and Codex with complete, isolated subscription state
while keeping the underlying CLIs in charge of authentication, settings,
sessions, terminal behavior, and runtime arguments.

## Native Homes Are the Identity Boundary

One subscription profile owns one native tool home:

- Codex: `CODEX_HOME=<state>/homes/codex/<profile>`
- Claude: `CLAUDE_CONFIG_DIR=<state>/homes/claude/<profile>`
- Claude secure storage: `CLAUDE_SECURESTORAGE_CONFIG_DIR=<same profile home>`

This boundary includes more than an access token. It keeps refresh state,
account metadata, tool settings, sessions, and tool-owned files together. rtr
does not modify or copy those files; conversation discovery reads their native
metadata and leaves ownership with the tool.

Claude's config directory owns user settings, app state, session history,
plugins, and side-by-side account context. Claude Code 2.1.205 also lets the
secure-storage path qualify the macOS Keychain service, so rtr pins both Claude
variables to the selected native boundary without accessing credentials.

## Launch Flow

1. Load strict tool and profile configuration.
2. Resolve an explicit profile or choose the next enabled profile.
3. Create and validate its owner-only native home.
4. Refresh the tool's skills into that home under a lock.
5. Persist the automatic cursor only after preparation succeeds.
6. Spawn the configured command with the native-home variable and runtime args.
7. Map the child status to a shell exit code and append a usage event.

`rtr add` first serializes a duplicate check and profile-table update under the
config lock, then enters the same launch flow with the new profile forced for
the sign-in run. Duplicate adds do not touch the existing home or skills.

`rtr fix` validates an existing profile, removes only its recognized stale
credential lock, and enters the same preparation and child-execution flow
without profile selection. This lets even a disabled profile re-authenticate in
place while leaving its settings, sessions, and rotation cursor unchanged.

`rtr rm` validates and confirms the exact encoded native-home path before
mutation. It removes only that profile's TOML table with a lossless editor under
the config lock, then recursively deletes that one real directory. Config-first
ordering favors recoverable orphaned files over credential loss if deletion
fails.

Forced selection does not update automatic rotation. A spawn error still emits
a usage event with no exit code, making failed launch attempts visible without
inventing a child result.

## Native Conversation Operations

RTR treats a conversation as `(tool, profile, native session ID)`. Names and
generated titles are native mutable presentation metadata: Codex owns names in
`session_index.jsonl`, while Claude records `agent-name` and `ai-title` events.
Users rename the active conversation with each CLI's `/rename`; RTR does not
create another name store.

Discovery translates both native stores into one catalog while preserving the
owning profile. Codex's rollout corpus can be gigabytes, so cataloging reads only
the bounded metadata prefix and timestamp tail of each rollout plus its small
history/name indexes. Claude top-level project transcripts are materially
smaller and are scanned for cwd, prompts, and title records; nested subagent
transcripts are not resumable top-level conversations and are excluded.

Resume uses the native command in the exact original isolated home. Disabled and
normally bypassed source profiles remain recoverable without changing rotation.
Fork selects a destination through normal enabled-profile share allocation;
optional `--to-profile` forces an enabled destination without moving the cursor.
`--profile` continues to select the source. The recorded cwd is restored when it
still exists, and forks always use isolated homes despite ordinary bypass policy.

Same-profile forks use native `codex fork` or Claude `--resume --fork-session`.
Cross-profile forks copy native history with a fresh ID, then resume that ID in
the destination. Codex paginated parent prefixes become one independent rollout;
Claude retains message UUID links and copies session companions. Native CLIs own
replay and index rebuilding. Credentials and settings stay in their owning homes;
queued work, account bridge ownership, and Claude file-rewind state are excluded.

Automatic selection and destination preparation hold the normal state lock, then
commit the cursor before bulk copying. Later failure consumes the reserved slot;
rolling it back could undo a concurrent launch. Private staging and exclusive
publication prevent overwriting existing sessions. Assets publish before the
transcript, and only unpublished operation-owned files are removed on failure.
After publication the new session is retained even if its native launch fails.

The built-in terminal picker separates rendered rows, searchable text, and the
encoded identity returned on selection. A loader discovers metadata and indexes
complete human dialogue; a worker handles queries and bounded previews. The
terminal only handles input and painting. Refresh generations and query
revisions prevent stale background results from changing what Enter opens.

Conversation, Matches, and Details previews keep recent context, matching old
passages, and launch settings separate. Explicit model and effort descriptions
reuse the runner's argument merging; unknown native defaults remain unknown.
Enter follows the invoking command, and Ctrl-F / Ctrl-R select an explicit mode.
A terminal guard restores normal input and screen state before native launch.

## Terminal Ownership

The child inherits stdin, stdout, and stderr. This preserves full-screen TUI
behavior, prompts, and output formatting. While waiting, rtr catches common
Unix terminal signals and forwards them to the direct child so it can finish
and produce a real status before usage is recorded. rtr also sets
`kill_on_drop` for cancellation paths.

## Switching a Shell

Launches express profile choice in the child's environment. A shell switch has
the opposite direction: the process that must change is rtr's own parent, and no
child can write to it. `switch` therefore produces `export` lines as data and
leaves application to the shell, either through the `shell-init` wrapper or an
explicit `eval`.

Only `switch` is intercepted by that wrapper. Every other subcommand reaches the
binary unchanged, so the shell integration cannot drift from the real command
surface as commands are added.

A trailing command is evaluated in the caller's shell rather than a child so
aliases and functions resolve exactly as typed; that is the point of
`rtr switch <profile> <alias>`. When rtr must run the command itself it prefers
a direct child for terminal and signal fidelity, and falls back to `$SHELL -i -c`
only for names no `PATH` lookup can resolve.

The switched profile is stored beside the rotation cursors but is read only by
shells. Selection ignores it, so switching never silently changes which profile
a bare `rtr codex` picks.

## Skills Refresh

The selected home receives a fresh `skills` tree on every launch. rtr copies to
a temporary sibling, replaces the destination only after a complete copy, and
serializes concurrent refreshes with a profile-local lock.

An explicit missing source is an error and leaves the previous tree intact. A
missing default source removes a stale destination because the user's global
tool home is the source of truth for default skills.

Claude supports symlinked skill directories. External relative links are
rebased to remain valid from the copied profile tree without resolving later
alias changes. Internal and dangling relative links stay verbatim. Codex links
use the same relocation policy.

Codex keeps `HOME` and the working directory, so native personal, repository,
and admin skill roots remain discoverable. rtr copies only a distinct legacy or
configured root into `$CODEX_HOME/skills`, excludes source `.system`, preserves
Codex's generated `.system` cache, and rolls back if staged installation fails.

## Verified Claude Code Contract

- [Environment variables](https://code.claude.com/docs/en/env-vars) documents
  `CLAUDE_CONFIG_DIR` as the override for user settings, session history,
  plugins, and non-macOS credential files.
- [The `.claude` directory](https://code.claude.com/docs/en/claude-directory)
  separates user state from project `.claude/*` state.
- [Skills](https://code.claude.com/docs/en/slash-commands) documents personal
  skills and symlinked skill-directory support.
- [Authentication](https://code.claude.com/docs/en/team) documents macOS
  Keychain storage and config-directory credential files on Linux and Windows.

## State and Concurrency

Config remains hand-editable and is never rewritten during launch. Mutable
round-robin cursors, weighted scores, and shell-switched profiles live in
`state.toml`; usage events live in `usage.jsonl`.
Both use advisory locks, and state replacement is atomic.

Percentage overrides are fixed shares of automatic selections; unassigned
enabled profiles split the remainder equally. The scheduler uses exact integer
ratios and smooth weighted rotation to avoid random streaks. Its persisted
allocation detects policy changes and discards stale scheduling credit under
the state lock. All-default tools retain the existing equal-rotation cursor.
Explicit launches, resumes, and inspection do not change automatic progress.

The weight command serializes lossless percentage edits under the config lock,
validates the resulting allocation before writing, and prints that transaction's
distribution. Disabled profiles retain settings but receive no automatic share;
zero-share enabled profiles remain available explicitly. Incomplete or excessive
fixed totals are reported instead of silently inflating a low-quota profile's
share. Reset removes all overrides for a tool, including disabled profiles.

Profile-table removal preserves unrelated comments, formatting, and quoted
keys. Equal rotation normalizes stale cursors by modulo; weighted rotation
detects the changed allocation. Profile deletion needs no state rewrite, but
removing the last profile without an override can leave fixed totals incomplete.

Native-home path segments are encoded before joining. Directory creation
rejects symlink components and tightens permissions to `0700`.

## Configuration Philosophy

The schema contains only behavior the launcher uses: commands, skills sources,
profiles, profile enablement, and percentage shares. Unknown fields fail deserialization so stale
or misspelled settings cannot look active while doing nothing.
