//! Client configuration in `$BOARD_HOME` and the device token.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use board_proto::{BoardSlug, DeviceName};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Keychain service name for the device token.
const KEYCHAIN_SERVICE: &str = "agent-board";
/// Keychain account of the default `$BOARD_HOME`; other homes add a suffix
/// ([`keychain_account`]).
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
        let root = Self::default_root().context("no home directory; set BOARD_HOME")?;
        Ok(Self::at(root))
    }

    /// `~/.local/share/agent-board`, the home used when `$BOARD_HOME` is unset.
    pub fn default_root() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".local/share/agent-board"))
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
    /// How many runs, workers and judges together, `board watch` has at once on this
    /// device across its daemon-dispatch boards; without it, one per 4 GiB of host memory
    /// and at least 2.
    pub runs_at_once: Option<std::num::NonZeroU32>,
    /// Local T3 project folders by repo key, for starting coordinator threads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, PathBuf>,
    /// Local T3 project folders by board slug, for a board none of whose repos is under
    /// `projects` (such as a feedback board with no repo).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub board_projects: BTreeMap<BoardSlug, PathBuf>,
    /// The T3 provider instance and model for the threads `board watch` starts; without
    /// it they get the project's defaults.
    pub thread_model: Option<ThreadModel>,
}

/// A T3 provider instance and a model of it: `t3 thread start` takes them only together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadModel {
    /// The provider instance's id in T3's settings.
    #[serde(deserialize_with = "non_empty")]
    pub provider: String,
    /// The model's slug.
    #[serde(deserialize_with = "non_empty")]
    pub model: String,
}

/// A string with text in it: an empty one would fail every thread start.
fn non_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.trim().is_empty() {
        return Err(serde::de::Error::custom(
            "is empty: give the T3 provider instance's id and a model slug",
        ));
    }
    Ok(text)
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
    load_token_from(
        home,
        std::env::var("BOARD_TOKEN").ok(),
        TokenStore::for_platform(),
        &SecurityCli,
    )
}

fn load_token_from(
    home: &Home,
    env_token: Option<String>,
    store: TokenStore,
    keychain: &dyn Keychain,
) -> Result<Option<String>> {
    if let Some(token) = env_token
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
    {
        return Ok(Some(token));
    }
    if store == TokenStore::Keychain {
        match keychain.read(&keychain_account_for(home)) {
            Ok(Some(token)) => return Ok(Some(token)),
            Ok(None) => {}
            // Locked, or no user interaction (SSH, launchd): the file may hold the token.
            Err(err) => tracing::debug!("reading the Keychain: {err:#}"),
        }
    }
    match std::fs::read_to_string(home.token_path()) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", home.token_path().display())),
    }
}

/// Where [`store_token`] put the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub store: TokenStore,
    /// Why the token is in the file although the platform uses the Keychain.
    pub note: Option<String>,
}

/// Stores the token and returns where it went. Never prints the token.
///
/// When the Keychain refuses it (locked, or no user interaction as over SSH or under
/// launchd), the token goes to `$BOARD_HOME/token` (0600) instead, so a token the server
/// has already issued is not lost.
pub fn store_token(home: &Home, token: &str) -> Result<StoredToken> {
    store_token_in(home, token, TokenStore::for_platform(), &SecurityCli)
}

fn store_token_in(
    home: &Home,
    token: &str,
    store: TokenStore,
    keychain: &dyn Keychain,
) -> Result<StoredToken> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic() && b != b'"') {
        bail!("the token must be one line of printable characters without quotes");
    }
    let mut note = None;
    if store == TokenStore::Keychain {
        let account = keychain_account_for(home);
        let Err(err) = keychain.write(&account, token) else {
            // A token file from an earlier fallback would be read when the Keychain is not.
            remove_if_present(&home.token_path())?;
            return Ok(StoredToken { store, note });
        };
        // An older token for this home would still be read before the file.
        keychain.delete(&account);
        let remove = format!(
            "remove it from a desktop session with `security delete-generic-password -s \
             {KEYCHAIN_SERVICE} -a {account}`"
        );
        let stale = match keychain.read(&account) {
            Ok(Some(old)) if old != token => Some(format!(
                "an older token for this home is still in the Keychain and is read before \
                 the file: {remove}"
            )),
            Ok(_) => None,
            Err(_) => Some(format!(
                "the Keychain could not be checked for an older token for this home, which \
                 would be read before the file in a desktop session; if `board` then fails \
                 with unauthorized, {remove}"
            )),
        };
        let mut text = format!("the Keychain refused the token ({err:#})");
        if let Some(stale) = stale {
            text.push_str("; ");
            text.push_str(&stale);
        }
        note = Some(text);
    }
    std::fs::create_dir_all(home.root())
        .with_context(|| format!("creating {}", home.root().display()))?;
    write_private(&home.token_path(), token.as_bytes())?;
    Ok(StoredToken {
        store: TokenStore::File,
        note,
    })
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The Keychain operations the token store needs; tests use a fake.
trait Keychain {
    /// `Ok(None)` when the account has no item; an error when the Keychain cannot be read.
    fn read(&self, account: &str) -> Result<Option<String>>;
    fn write(&self, account: &str, token: &str) -> Result<()>;
    /// Best effort: removes the account's item if it can.
    fn delete(&self, account: &str);
}

/// The macOS Keychain through the `security` tool.
struct SecurityCli;

impl Keychain for SecurityCli {
    fn read(&self, account: &str) -> Result<Option<String>> {
        keychain_read(account)
    }

    fn write(&self, account: &str, token: &str) -> Result<()> {
        keychain_write(account, token)
    }

    fn delete(&self, account: &str) {
        let _ = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                account,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The Keychain account holding the token of the home at `root`: [`KEYCHAIN_ACCOUNT`] for
/// the default home (where existing installs keep it), and for any other home that name
/// plus a hash of the home's canonical path, so a second login with another
/// `$BOARD_HOME` (such as a `--user` token) never replaces this device's token.
fn keychain_account(root: &Path, default_root: Option<&Path>) -> String {
    let root = canonical(root);
    if default_root.is_some_and(|default| canonical(default) == root) {
        return KEYCHAIN_ACCOUNT.to_owned();
    }
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    format!("{KEYCHAIN_ACCOUNT}-{}", hex(&digest[..8]))
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn keychain_account_for(home: &Home) -> String {
    keychain_account(home.root(), Home::default_root().as_deref())
}

/// `path` made absolute with symlinks resolved as far as it exists, so a home that does
/// not exist yet names the same account once it does.
fn canonical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return rest.iter().rev().fold(resolved, |p, part| p.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

fn keychain_read(account: &str) -> Result<Option<String>> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .args(["-a", account, "-w"])
        .stderr(Stdio::null())
        .output();
    let out = output.context("running `security`")?;
    if out.status.success() {
        let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        return Ok(Some(token).filter(|t| !t.is_empty()));
    }
    // errSecItemNotFound.
    if out.status.code() == Some(44) {
        return Ok(None);
    }
    bail!("`security find-generic-password` failed: {}", out.status)
}

/// Writes through `security -i` so the token never appears in a process's arguments.
fn keychain_write(account: &str, token: &str) -> Result<()> {
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `security`")?;
    let command =
        format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {account} -w \"{token}\"\n");
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
    fn a_thread_model_needs_its_provider_and_its_model() {
        let config: Config =
            toml::from_str("[thread_model]\nprovider = \"claude-agents\"\nmodel = \"a-model\"\n")
                .unwrap();
        let model = ThreadModel {
            provider: "claude-agents".into(),
            model: "a-model".into(),
        };
        assert_eq!(config.thread_model, Some(model));
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);

        assert_eq!(toml::from_str::<Config>("").unwrap().thread_model, None);
        let err =
            toml::from_str::<Config>("[thread_model]\nprovider = \"claude-agents\"\n").unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
        let err = toml::from_str::<Config>("[thread_model]\nprovider = \"\"\nmodel = \"m\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
    }

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
            board_projects: [("board-feedback".parse().unwrap(), PathBuf::from("/src/fb"))].into(),
            ..Config::default()
        };
        config.save(&home).unwrap();
        assert_eq!(Config::load(&home).unwrap(), config);
        let text = std::fs::read_to_string(home.config_path()).unwrap();
        assert!(
            text.contains("[board_projects]\nboard-feedback = \"/src/fb\""),
            "{text}"
        );
    }

    #[test]
    fn keychain_accounts_are_per_home_and_keep_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("share/agent-board");
        std::fs::create_dir_all(&default).unwrap();
        let account = |root: &Path| keychain_account(root, Some(&default));

        // The default home keeps the account existing installs use, however it is spelled.
        assert_eq!(account(&default), "device-token");
        assert_eq!(account(&default.join("../agent-board")), "device-token");
        assert_eq!(
            keychain_account(&default, None).len(),
            "device-token-".len() + 16
        );

        // Another home gets its own account, stable before and after the home exists.
        let user = dir.path().join("user-home");
        let before = account(&user);
        std::fs::create_dir_all(&user).unwrap();
        assert_eq!(account(&user), before);
        assert!(before.starts_with("device-token-"));
        assert_ne!(before, "device-token");
        assert_ne!(account(&dir.path().join("other")), before);
        assert!(
            before
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        );

        #[cfg(unix)]
        {
            // A symlink to a home names the same account as the home.
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&user, &link).unwrap();
            assert_eq!(account(&link), before);
            let default_link = dir.path().join("default-link");
            std::os::unix::fs::symlink(&default, &default_link).unwrap();
            assert_eq!(account(&default_link), "device-token");
        }
    }

    /// A Keychain that holds items in memory and can refuse writes and deletes.
    #[derive(Default)]
    struct FakeKeychain {
        items: std::cell::RefCell<BTreeMap<String, String>>,
        refuse_read: bool,
        refuse_write: bool,
        refuse_delete: bool,
    }

    impl Keychain for FakeKeychain {
        fn read(&self, account: &str) -> Result<Option<String>> {
            if self.refuse_read {
                bail!("User interaction is not allowed.");
            }
            Ok(self.items.borrow().get(account).cloned())
        }

        fn write(&self, account: &str, token: &str) -> Result<()> {
            if self.refuse_write {
                bail!("User interaction is not allowed. add-generic-password: returned -25308");
            }
            self.items
                .borrow_mut()
                .insert(account.to_owned(), token.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) {
            if !self.refuse_delete {
                self.items.borrow_mut().remove(account);
            }
        }
    }

    #[test]
    fn tokens_go_to_the_keychain_and_load_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let keychain = FakeKeychain::default();
        // A token file left by an earlier fallback goes once the Keychain has the token.
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.token_path(), "revoked").unwrap();
        let stored = store_token_in(&home, "tok-1", TokenStore::Keychain, &keychain).unwrap();
        assert_eq!(
            stored,
            StoredToken {
                store: TokenStore::Keychain,
                note: None
            }
        );
        assert!(!home.token_path().exists());
        let load = |env: Option<&str>| {
            load_token_from(
                &home,
                env.map(str::to_owned),
                TokenStore::Keychain,
                &keychain,
            )
            .unwrap()
        };
        assert_eq!(load(None).as_deref(), Some("tok-1"));
        // BOARD_TOKEN still comes first.
        assert_eq!(load(Some(" env-tok ")).as_deref(), Some("env-tok"));
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_keychain_write_falls_back_to_the_private_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let account = keychain_account_for(&home);
        // An older token for this home, which the fallback removes.
        let keychain = FakeKeychain {
            refuse_write: true,
            ..FakeKeychain::default()
        };
        keychain
            .items
            .borrow_mut()
            .insert(account.clone(), "old-tok".into());

        let stored = store_token_in(&home, "new-tok", TokenStore::Keychain, &keychain).unwrap();
        assert_eq!(stored.store, TokenStore::File);
        let note = stored.note.unwrap();
        assert!(note.contains("-25308"), "{note}");
        assert!(!note.contains("still in the Keychain"), "{note}");
        assert_eq!(
            std::fs::read_to_string(home.token_path()).unwrap(),
            "new-tok"
        );
        let mode = std::fs::metadata(home.token_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The usual order (BOARD_TOKEN, Keychain, file) finds it.
        assert_eq!(
            load_token_from(&home, None, TokenStore::Keychain, &keychain)
                .unwrap()
                .as_deref(),
            Some("new-tok")
        );

        // When the older token cannot be removed either, the note says how.
        let stuck = FakeKeychain {
            refuse_write: true,
            refuse_delete: true,
            ..FakeKeychain::default()
        };
        stuck
            .items
            .borrow_mut()
            .insert(account.clone(), "old-tok".into());
        let note = store_token_in(&home, "new-tok", TokenStore::Keychain, &stuck)
            .unwrap()
            .note
            .unwrap();
        assert!(note.contains("still in the Keychain"), "{note}");
        assert!(note.contains(&format!("-a {account}")), "{note}");

        // A locked Keychain refuses reads too: the token is still found in the file, and
        // the note says an older Keychain token could not be ruled out.
        let locked = FakeKeychain {
            refuse_read: true,
            refuse_write: true,
            refuse_delete: true,
            ..FakeKeychain::default()
        };
        let note = store_token_in(&home, "locked-tok", TokenStore::Keychain, &locked)
            .unwrap()
            .note
            .unwrap();
        assert!(note.contains("could not be checked"), "{note}");
        assert_eq!(
            load_token_from(&home, None, TokenStore::Keychain, &locked)
                .unwrap()
                .as_deref(),
            Some("locked-tok")
        );
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
