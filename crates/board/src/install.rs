//! `board install`: hook and MCP configuration for Claude Code and Codex, and service files
//! for the device daemon.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::cli::expand_tilde;
use crate::config::{Home, write_atomic};
use crate::hook::HookEvent;

const LAUNCHD_LABEL: &str = "dev.agent-board.watch";
const SYSTEMD_UNIT: &str = "agent-board-watch.service";

#[derive(Debug, Subcommand)]
pub enum InstallCommand {
    /// Hooks and the MCP server for Claude Code.
    Claude {
        /// Merge the hooks into this settings file, such as ~/.claude/settings.json.
        #[arg(long)]
        write: Option<PathBuf>,
        /// With --write, merge the MCP server into this file (user scope: ~/.claude.json).
        #[arg(long, requires = "write")]
        mcp_config: Option<PathBuf>,
        /// The board binary the entries run (default: this one).
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Hooks (hooks.json) and the MCP server (config.toml snippet) for Codex.
    Codex {
        /// Merge the hooks into this hooks file, such as ~/.codex/hooks.json.
        #[arg(long)]
        write: Option<PathBuf>,
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// A launchd agent running `board watch` (macOS).
    Launchd {
        /// Write the plist (default ~/Library/LaunchAgents/dev.agent-board.watch.plist).
        #[arg(long, num_args = 0..=1)]
        write: Option<Option<PathBuf>>,
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// A systemd user unit running `board watch` (Linux).
    Systemd {
        /// Write the unit (default ~/.config/systemd/user/agent-board-watch.service).
        #[arg(long, num_args = 0..=1)]
        write: Option<Option<PathBuf>>,
        #[arg(long)]
        bin: Option<PathBuf>,
    },
}

/// One hook entry: the event, its matcher and its timeout in seconds.
struct HookSpec {
    event: HookEvent,
    matcher: Option<&'static str>,
    timeout: u32,
}

/// Claude Code: matchers per event (tool name, start source, error type, trigger).
const CLAUDE_HOOKS: &[HookSpec] = &[
    HookSpec {
        event: HookEvent::SessionStart,
        matcher: Some("startup|resume|clear|compact"),
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::UserPromptSubmit,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::PostToolUse,
        matcher: Some("*"),
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::Stop,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::StopFailure,
        matcher: Some("rate_limit"),
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::SessionEnd,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::PreCompact,
        matcher: Some("manual|auto"),
        timeout: 5,
    },
];

/// Codex: no StopFailure event, and PreCompact cannot add instructions. SessionEnd
/// hooks may run at most 3 seconds.
const CODEX_HOOKS: &[HookSpec] = &[
    HookSpec {
        event: HookEvent::SessionStart,
        matcher: Some("startup|resume|clear|compact|fork"),
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::UserPromptSubmit,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::PostToolUse,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::Stop,
        matcher: None,
        timeout: 5,
    },
    HookSpec {
        event: HookEvent::SessionEnd,
        matcher: None,
        timeout: 3,
    },
];

pub fn run(cmd: InstallCommand) -> Result<()> {
    match cmd {
        InstallCommand::Claude {
            write,
            mcp_config,
            bin,
        } => {
            let bin = binary(bin);
            let hooks = hooks_json(&bin, "claude", CLAUDE_HOOKS);
            let mcp = json!({"mcpServers": {"board": claude_mcp_server(&bin)}});
            match write {
                None => {
                    println!("# Merge into ~/.claude/settings.json:");
                    println!("{}", serde_json::to_string_pretty(&hooks)?);
                    println!(
                        "# Merge into ~/.claude.json (user-scoped MCP servers), or run: claude mcp add --scope user board -- {} mcp",
                        shell_word(&bin)
                    );
                    println!("{}", serde_json::to_string_pretty(&mcp)?);
                }
                Some(settings) => {
                    let settings = expand_tilde(settings);
                    merge_hooks_file(&settings, &hooks)?;
                    println!("merged board hooks into {}", settings.display());
                    let mcp_path = match mcp_config {
                        Some(path) => expand_tilde(path),
                        None => dirs::home_dir()
                            .context("no home directory")?
                            .join(".claude.json"),
                    };
                    merge_mcp_file(&mcp_path, claude_mcp_server(&bin))?;
                    println!("merged the board MCP server into {}", mcp_path.display());
                }
            }
        }
        InstallCommand::Codex { write, bin } => {
            let bin = binary(bin);
            let hooks = hooks_json(&bin, "codex", CODEX_HOOKS);
            match write {
                None => {
                    println!("# Merge into ~/.codex/hooks.json:");
                    println!("{}", serde_json::to_string_pretty(&hooks)?);
                }
                Some(path) => {
                    let path = expand_tilde(path);
                    merge_hooks_file(&path, &hooks)?;
                    println!("merged board hooks into {}", path.display());
                }
            }
            println!("# Add to ~/.codex/config.toml:");
            print!("{}", codex_mcp_toml(&bin));
        }
        InstallCommand::Launchd { write, bin } => {
            let home = Home::from_env()?;
            let text = launchd_plist(&binary(bin), &home);
            let default = || -> Result<PathBuf> {
                Ok(dirs::home_dir()
                    .context("no home directory")?
                    .join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
            };
            emit_service(write, default, &text, |path| {
                format!(
                    "load it with: launchctl bootstrap gui/$(id -u) {}",
                    path.display()
                )
            })?;
        }
        InstallCommand::Systemd { write, bin } => {
            let home = Home::from_env()?;
            let text = systemd_unit(&binary(bin), &home);
            let default = || -> Result<PathBuf> {
                Ok(dirs::config_dir()
                    .filter(|_| cfg!(target_os = "linux"))
                    .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
                    .context("no home directory")?
                    .join(format!("systemd/user/{SYSTEMD_UNIT}")))
            };
            emit_service(write, default, &text, |_| {
                format!(
                    "start it with: systemctl --user daemon-reload && systemctl --user enable --now {SYSTEMD_UNIT}"
                )
            })?;
        }
    }
    Ok(())
}

fn binary(bin: Option<PathBuf>) -> PathBuf {
    bin.map(expand_tilde)
        .unwrap_or_else(crate::hook::current_exe)
}

fn emit_service(
    write: Option<Option<PathBuf>>,
    default: impl FnOnce() -> Result<PathBuf>,
    text: &str,
    hint: impl FnOnce(&Path) -> String,
) -> Result<()> {
    match write {
        None => print!("{text}"),
        Some(path) => {
            let path = match path {
                Some(p) => expand_tilde(p),
                None => default()?,
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            write_atomic(&path, text.as_bytes())?;
            println!("wrote {}; {}", path.display(), hint(&path));
        }
    }
    Ok(())
}

/// Quotes a path for a shell command line when it needs quoting.
fn shell_word(path: &Path) -> String {
    let s = path.display().to_string();
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+~@%:,".contains(&b))
    {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn hook_command(bin: &Path, event: HookEvent, runtime: &str) -> String {
    format!(
        "{} hook {} --runtime {runtime}",
        shell_word(bin),
        event.arg()
    )
}

/// Is this hook command one that `board install` wrote (for any binary path)?
fn is_board_hook(command: &str) -> bool {
    let Some((program, rest)) = split_program(command) else {
        return false;
    };
    let name = Path::new(&program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name == "board" && rest.trim_start().starts_with("hook ") && rest.contains("--runtime ")
}

fn split_program(command: &str) -> Option<(String, &str)> {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('\'') {
        let end = quoted.find('\'')?;
        Some((quoted[..end].to_owned(), &quoted[end + 1..]))
    } else {
        let (program, rest) = command.split_once(' ')?;
        Some((program.to_owned(), rest))
    }
}

fn hooks_json(bin: &Path, runtime: &str, specs: &[HookSpec]) -> Value {
    let mut hooks = Map::new();
    for spec in specs {
        let mut group = Map::new();
        if let Some(matcher) = spec.matcher {
            group.insert("matcher".into(), json!(matcher));
        }
        group.insert(
            "hooks".into(),
            json!([{
                "type": "command",
                "command": hook_command(bin, spec.event, runtime),
                "timeout": spec.timeout,
            }]),
        );
        hooks.insert(spec.event.name().into(), json!([group]));
    }
    json!({ "hooks": hooks })
}

fn claude_mcp_server(bin: &Path) -> Value {
    json!({"type": "stdio", "command": bin.display().to_string(), "args": ["mcp"]})
}

fn codex_mcp_toml(bin: &Path) -> String {
    let command = toml::Value::String(bin.display().to_string());
    format!(
        "[mcp_servers.board]\ncommand = {command}\nargs = [\"mcp\"]\nenv_vars = [\"T3CODE_THREAD_ID\"]\n"
    )
}

/// Parses a settings file's text; empty text is an empty object.
fn parse_json_object(path: &Path, text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// The file's text, or `None` when it does not exist.
fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// The file a path names once symlinks are followed (the link target may not exist yet), so a
/// settings file linked into a dotfiles repository stays a link.
fn resolve_links(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)
                    .with_context(|| format!("reading the link {}", path.display()))?;
                path = match path.parent() {
                    Some(dir) => dir.join(target),
                    None => target,
                };
            }
            Ok(_) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }
    bail!("{}: too many levels of symbolic links", path.display())
}

/// How many times a merge is redone when another program rewrites the file meanwhile.
const MERGE_ATTEMPTS: usize = 5;

/// Applies `edit` to the JSON object in `path` and writes the result back in place of the file
/// a symlink points to, keeping that file's permissions (0600 for a new file: `~/.claude.json`
/// holds OAuth and MCP secrets) and replacing it by an atomic rename in its own directory.
///
/// Claude Code rewrites these files while it runs. The file is read again just before the
/// rename and the merge redone on the newer text when it changed, which narrows the window
/// but cannot close it: a write landing between that last read and the rename is lost.
/// No lock is shared with Claude Code; quitting it first avoids the race.
fn update_json_file(
    path: &Path,
    mut edit: impl FnMut(&mut Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let target = resolve_links(path)?;
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    for _ in 0..MERGE_ATTEMPTS {
        let before = read_text(&target)?;
        let mut root = parse_json_object(&target, before.as_deref().unwrap_or_default())?;
        edit(&mut root)?;
        let mut text = serde_json::to_string_pretty(&Value::Object(root))?;
        text.push('\n');
        let tmp = write_temp_beside(&target, text.as_bytes())?;
        let committed = (|| -> Result<bool> {
            if read_text(&target)? != before {
                return Ok(false);
            }
            std::fs::rename(&tmp, &target)
                .with_context(|| format!("writing {}", target.display()))?;
            Ok(true)
        })();
        if !matches!(committed, Ok(true)) {
            // Best effort: the temporary file is ours and holds nothing new.
            let _ = std::fs::remove_file(&tmp);
        }
        if committed? {
            return Ok(());
        }
    }
    bail!(
        "{} kept changing while it was merged; quit Claude Code and run this again",
        target.display()
    )
}

/// Writes `bytes` to a new temporary file next to `target`, with `target`'s permissions, or
/// 0600 when `target` does not exist.
fn write_temp_beside(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write as _;
    let permissions = match std::fs::metadata(target) {
        Ok(meta) => Some(meta.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", target.display())),
    };
    let mut name = std::ffi::OsString::from(".");
    name.push(target.file_name().unwrap_or_default());
    name.push(format!(".board-{}.tmp", std::process::id()));
    let tmp = target.with_file_name(name);
    // A leftover from an interrupted run would keep its own mode: start from a new file.
    match std::fs::remove_file(&tmp) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(err).with_context(|| format!("removing {}", tmp.display()));
        }
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let written = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.context(format!("writing {}", tmp.display())));
    }
    Ok(tmp)
}

/// Merges `ours` (`{"hooks": {...}}`) into the file: previous board hook entries are
/// replaced, every other key and hook is kept, and running it twice changes nothing.
pub fn merge_hooks_file(path: &Path, ours: &Value) -> Result<()> {
    update_json_file(path, |root| merge_hooks(root, ours))
}

fn merge_hooks(root: &mut Map<String, Value>, ours: &Value) -> Result<()> {
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("\"hooks\" is not an object")?;
    // Drop board entries from every event, including events no longer installed.
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| !h["command"].as_str().is_some_and(is_board_hook));
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|l| !l.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    for (event, groups) in ours["hooks"].as_object().into_iter().flatten() {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;
        list.extend(groups.as_array().into_iter().flatten().cloned());
    }
    Ok(())
}

/// Sets `mcpServers.board` in the file, keeping everything else.
pub fn merge_mcp_file(path: &Path, server: Value) -> Result<()> {
    update_json_file(path, |root| {
        root.entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("\"mcpServers\" is not an object")?
            .insert("board".into(), server.clone());
        Ok(())
    })
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn service_path_env() -> String {
    // launchd and systemd start with a minimal PATH; `t3` and `git` must be found.
    let mut dirs = vec![];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".t3/bin").display().to_string());
        dirs.push(home.join(".cargo/bin").display().to_string());
        dirs.push(home.join(".local/bin").display().to_string());
    }
    dirs.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    dirs.join(":")
}

pub fn launchd_plist(bin: &Path, home: &Home) -> String {
    let log = home.root().join("watch.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>watch</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>BOARD_HOME</key>
    <string>{home}</string>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>30</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        bin = xml(&bin.display().to_string()),
        home = xml(&home.root().display().to_string()),
        path = xml(&service_path_env()),
        log = xml(&log.display().to_string()),
    )
}

pub fn systemd_unit(bin: &Path, home: &Home) -> String {
    let quote = |s: String| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    format!(
        "[Unit]\nDescription=agent-board device daemon (board watch)\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={bin} watch\nEnvironment={home}\nEnvironment={path}\nRestart=always\nRestartSec=30\n\n[Install]\nWantedBy=default.target\n",
        bin = quote(bin.display().to_string()),
        home = quote(format!("BOARD_HOME={}", home.root().display())),
        path = quote(format!("PATH={}", service_path_env())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_hook_commands_are_recognized() {
        assert!(is_board_hook(
            "/usr/local/bin/board hook stop --runtime claude"
        ));
        assert!(is_board_hook(
            "'/Users/me/My Tools/board' hook stop --runtime codex"
        ));
        assert!(is_board_hook("board hook session-start --runtime claude"));
        assert!(!is_board_hook("/bin/other hook stop --runtime claude"));
        assert!(!is_board_hook("board status"));
        assert!(!is_board_hook("echo board hook"));
    }

    #[test]
    fn hooks_merge_is_idempotent_and_replaces_old_paths() {
        let mut root: Map<String, Value> = serde_json::from_value(json!({
            "hooks": {
                "Stop": [
                    {"hooks": [{"type": "command", "command": "/old/board hook stop --runtime claude"}]},
                    {"hooks": [{"type": "command", "command": "notify-send done"}]}
                ],
                "Notification": [{"hooks": [{"type": "command", "command": "say hi"}]}]
            }
        }))
        .unwrap();
        let ours = hooks_json(Path::new("/new/board"), "claude", CLAUDE_HOOKS);
        merge_hooks(&mut root, &ours).unwrap();
        let once = root.clone();
        merge_hooks(&mut root, &ours).unwrap();
        assert_eq!(root, once);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "/new/board hook stop --runtime claude"
        );
        assert_eq!(
            root["hooks"]["Notification"][0]["hooks"][0]["command"],
            "say hi"
        );
        assert_eq!(root["hooks"]["StopFailure"][0]["matcher"], "rate_limit");
        assert_eq!(root["hooks"]["PreCompact"][0]["matcher"], "manual|auto");
    }

    #[test]
    fn a_file_changed_during_a_merge_is_merged_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.json");
        std::fs::write(&path, r#"{"a": 1}"#).unwrap();
        let mut calls = 0;
        update_json_file(&path, |root| {
            calls += 1;
            if calls == 1 {
                // Claude Code rewrites the file between our read and our rename.
                std::fs::write(&path, r#"{"a": 1, "b": 2}"#).unwrap();
            }
            root.insert("board".into(), json!(true));
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        let merged: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged, json!({"a": 1, "b": 2, "board": true}));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["claude.json"], "no temporary file is left");
    }

    #[test]
    fn service_files_name_the_binary_and_home() {
        let home = Home::at(PathBuf::from("/data/board & co"));
        let plist = launchd_plist(Path::new("/opt/board"), &home);
        assert!(plist.contains("<string>/opt/board</string>\n    <string>watch</string>"));
        assert!(plist.contains("/data/board &amp; co"));
        let unit = systemd_unit(Path::new("/opt/my board"), &home);
        assert!(unit.contains("ExecStart=\"/opt/my board\" watch"));
        assert!(unit.contains("Environment=\"BOARD_HOME=/data/board & co\""));
    }

    #[test]
    fn codex_snippet_forwards_the_t3_thread_id() {
        let snippet = codex_mcp_toml(Path::new("/opt/board"));
        let parsed: toml::Table = toml::from_str(&snippet).unwrap();
        let server = &parsed["mcp_servers"]["board"];
        assert_eq!(server["command"].as_str(), Some("/opt/board"));
        assert_eq!(server["env_vars"][0].as_str(), Some("T3CODE_THREAD_ID"));
    }
}
