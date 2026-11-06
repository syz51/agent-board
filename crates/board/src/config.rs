//! Client configuration in `$BOARD_HOME` and the device token.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use board_proto::{BoardSlug, DeviceName, PeerFraming, SnapshotFormat};
use board_server::Style;
use serde::{Deserialize, Serialize};

/// Keychain service name for the device token.
const KEYCHAIN_SERVICE: &str = "agent-board";
const KEYCHAIN_ACCOUNT: &str = "device-token";
/// Board that `board feedback` files tasks on unless configured otherwise.
pub const DEFAULT_FEEDBACK_BOARD: &str = "board-feedback";

/// The client's data directory: `config.toml`, `token`, `cache/` and `state/`.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// `$BOARD_HOME`, else `~/.local/share/agent-board`.
    pub fn from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("BOARD_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::at(PathBuf::from(root)));
        }
        let home = dirs::home_dir().context("no home directory; set BOARD_HOME")?;
        Ok(Self::at(home.join(".local/share/agent-board")))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn token_path(&self) -> PathBuf {
        self.root.join("token")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
}

/// `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the board server, such as `https://board.example.ts.net`.
    pub server: Option<String>,
    /// This device's name, as registered on the server.
    pub device: Option<DeviceName>,
    /// Board that `board feedback` files tasks on.
    pub feedback_board: Option<BoardSlug>,
    /// Path of the `t3` binary used by `board watch`.
    pub t3_bin: Option<PathBuf>,
    /// Local T3 project folders by repo key, for starting coordinator threads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, PathBuf>,
    /// Settings that exist only to be benchmarked; off unless set.
    #[serde(default, skip_serializing_if = "Experimental::is_unset")]
    pub experimental: Experimental,
}

/// `[experimental]`: the text format and framing of hook text and tool results, for the
/// format benchmark (`docs/DESIGN.md`, "Text format"). Unset means the production format,
/// lines with quoted strings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Experimental {
    /// `lines`, `json`, `yaml` or `toon`.
    pub text_format: Option<SnapshotFormat>,
    /// `quoted`, `bare` or `datamarked`.
    pub peer_framing: Option<PeerFraming>,
}

impl Experimental {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

impl Config {
    /// Reads `config.toml`; a missing file is an empty configuration.
    pub fn load(home: &Home) -> Result<Self> {
        let path = home.config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, home: &Home) -> Result<()> {
        std::fs::create_dir_all(home.root())
            .with_context(|| format!("creating {}", home.root().display()))?;
        let text = toml::to_string_pretty(self).context("serializing config")?;
        write_atomic(&home.config_path(), text.as_bytes())
    }

    /// The style of hook text and tool results.
    pub fn style(&self) -> Style {
        Style::new(
            self.experimental.text_format.unwrap_or_default(),
            self.experimental.peer_framing.unwrap_or_default(),
        )
    }

    pub fn feedback_board(&self) -> BoardSlug {
        self.feedback_board.clone().unwrap_or_else(|| {
            DEFAULT_FEEDBACK_BOARD
                .parse()
                .expect("default feedback board is a valid slug")
        })
    }

    /// Server URL and device name, or an error saying how to log in.
    pub fn require_login(&self) -> Result<(&str, &DeviceName)> {
        match (self.server.as_deref(), self.device.as_ref()) {
            (Some(server), Some(device)) => Ok((server, device)),
            _ => bail!("not logged in: run `board login --server <url> --device <name>`"),
        }
    }
}

/// Where the device token is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStore {
    Keychain,
    File,
}

impl TokenStore {
    /// The macOS Keychain when available, unless `BOARD_KEYCHAIN=0`; otherwise a file.
    pub fn for_platform() -> Self {
        let disabled = std::env::var("BOARD_KEYCHAIN").is_ok_and(|v| v == "0");
        if cfg!(target_os = "macos") && !disabled {
            Self::Keychain
        } else {
            Self::File
        }
    }
}

/// The device token: `BOARD_TOKEN`, else the Keychain, else `$BOARD_HOME/token`.
pub fn load_token(home: &Home) -> Result<Option<String>> {
    if let Ok(token) = std::env::var("BOARD_TOKEN") {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    if TokenStore::for_platform() == TokenStore::Keychain
        && let Some(token) = keychain_read()?
    {
        return Ok(Some(token));
    }
    match std::fs::read_to_string(home.token_path()) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", home.token_path().display())),
    }
}

/// Stores the token and returns where it went. Never prints the token.
pub fn store_token(home: &Home, token: &str) -> Result<TokenStore> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic() && b != b'"') {
        bail!("the token must be one line of printable characters without quotes");
    }
    let store = TokenStore::for_platform();
    match store {
        TokenStore::Keychain => keychain_write(token)?,
        TokenStore::File => {
            std::fs::create_dir_all(home.root())?;
            write_private(&home.token_path(), token.as_bytes())?;
        }
    }
    Ok(store)
}

fn keychain_read() -> Result<Option<String>> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .args(["-a", KEYCHAIN_ACCOUNT, "-w"])
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            Ok(Some(token).filter(|t| !t.is_empty()))
        }
        // Not found (exit 44) or no `security` binary: fall back to the file.
        Ok(_) | Err(_) => Ok(None),
    }
}

/// Writes through `security -i` so the token never appears in a process's arguments.
fn keychain_write(token: &str) -> Result<()> {
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `security`")?;
    let command = format!(
        "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -w \"{token}\"\n"
    );
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(command.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() || !out.stderr.is_empty() {
        bail!(
            "storing the token in the Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a temporary file and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Like [`write_atomic`], with mode 0600 on Unix.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults_feedback_board() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let empty = Config::load(&home).unwrap();
        assert_eq!(empty, Config::default());
        assert_eq!(empty.feedback_board().as_str(), "board-feedback");
        assert!(empty.require_login().is_err());

        let config = Config {
            server: Some("http://127.0.0.1:1".into()),
            device: Some("mac-air".parse().unwrap()),
            projects: [("github.com/o/r".into(), PathBuf::from("/src/r"))].into(),
            ..Config::default()
        };
        config.save(&home).unwrap();
        assert_eq!(Config::load(&home).unwrap(), config);
    }

    #[test]
    fn experimental_style_is_off_unless_set() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        assert_eq!(Config::default().style(), Style::default());
        Config::default().save(&home).unwrap();
        let saved = std::fs::read_to_string(home.config_path()).unwrap();
        assert!(!saved.contains("experimental"), "{saved}");

        std::fs::write(
            home.config_path(),
            "[experimental]\ntext_format = \"json\"\npeer_framing = \"bare\"\n",
        )
        .unwrap();
        let style = Config::load(&home).unwrap().style();
        assert_eq!(
            (style.format, style.framing),
            (SnapshotFormat::Json, PeerFraming::Bare)
        );
        std::fs::write(
            home.config_path(),
            "[experimental]\ntext_format = \"xml\"\n",
        )
        .unwrap();
        assert!(Config::load(&home).is_err());
        std::fs::write(home.config_path(), "[experimental]\nframing = \"bare\"\n").unwrap();
        assert!(Config::load(&home).is_err(), "unknown keys are refused");
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_private(&path, b"secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
