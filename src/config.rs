//! Configuration file (`~/.config/lazygrafana/config.yml`) and token resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicsMode {
    #[default]
    Auto,
    Kitty,
    Sixel,
    Iterm,
    Unicode,
}

impl std::str::FromStr for GraphicsMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "kitty" => Ok(Self::Kitty),
            "sixel" => Ok(Self::Sixel),
            "iterm" | "iterm2" => Ok(Self::Iterm),
            "unicode" | "off" => Ok(Self::Unicode),
            _ => Err(format!("unknown graphics mode: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnicodeMarker {
    #[default]
    Braille,
    Octant,
    Block,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub url: String,
    pub org: Option<i64>,
    pub token_file: Option<PathBuf>,
    pub token_env: Option<String>,
    pub token_cmd: Option<String>,
    /// Literal token. Discouraged, but handy for throwaway setups.
    pub token: Option<String>,
    /// OS keyring entry: `account` or `service/account` (service defaults to `lazygrafana`).
    /// Store one with `lazygrafana auth set <context>`.
    pub keyring: Option<String>,
    pub readonly: bool,
    pub cloud: Option<bool>,
    pub ca_file: Option<PathBuf>,
    pub insecure_skip_verify: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_context: Option<String>,
    pub default_time: String,
    pub refresh: String,
    pub graphics: GraphicsMode,
    pub unicode_marker: UnicodeMarker,
    pub mouse: bool,
    pub notifications: bool,
    pub theme: String,
    pub contexts: BTreeMap<String, ContextConfig>,
    /// Action name -> key spec overrides.
    pub keys: BTreeMap<String, String>,
    #[serde(skip)]
    pub base_dir: PathBuf,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_context: None,
            default_time: "now-6h".into(),
            refresh: "off".into(),
            graphics: GraphicsMode::Auto,
            unicode_marker: UnicodeMarker::Braille,
            mouse: true,
            notifications: true,
            theme: "auto".into(),
            contexts: BTreeMap::new(),
            keys: BTreeMap::new(),
            base_dir: PathBuf::from("."),
            path: None,
        }
    }
}

/// A fully resolved context, ready to build a client from.
#[derive(Debug, Clone)]
pub struct Context {
    pub name: String,
    pub url: String,
    pub org: Option<i64>,
    pub token: Option<String>,
    pub token_error: Option<String>,
    pub readonly: bool,
    pub cloud: bool,
    pub ca_file: Option<PathBuf>,
    pub insecure_skip_verify: bool,
}

pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lazygrafana").map(|d| d.config_dir().join("config.yml"))
}

pub fn state_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "lazygrafana").map(|d| d.state_dir().unwrap_or_else(|| d.data_local_dir()).to_path_buf())
}

impl Config {
    /// Loads the config from `path`, or the default location. A missing default
    /// config is fine as long as `GRAFANA_URL` is set.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path.is_some();
        let path = path.map(Path::to_path_buf).or_else(default_config_path);
        let mut config = match &path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p).wrap_err_with(|| format!("reading {}", p.display()))?;
                let mut c = Self::parse(&text).wrap_err_with(|| format!("parsing {}", p.display()))?;
                c.base_dir = p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
                c.path = Some(p.clone());
                c
            }
            Some(p) if explicit => bail!("config file not found: {}", p.display()),
            _ => Self::default(),
        };
        config.add_env_context();
        if config.contexts.is_empty() {
            bail!(
                "no Grafana contexts configured.\n\
                 Create {} (see dev/lazygrafana.yml for an example),\n\
                 or set GRAFANA_URL and GRAFANA_TOKEN.",
                default_config_path().map(|p| p.display().to_string()).unwrap_or_else(|| "a config file".into())
            );
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = serde_saphyr::from_str(text)?;
        for (name, ctx) in &config.contexts {
            if ctx.url.is_empty() {
                bail!("context '{name}' has no url");
            }
        }
        if let Some(d) = &config.default_context
            && !config.contexts.contains_key(d)
        {
            bail!("default_context '{d}' is not defined under contexts");
        }
        Ok(config)
    }

    /// `GRAFANA_URL` (+ `GRAFANA_TOKEN`) adds an implicit `env` context.
    fn add_env_context(&mut self) {
        let Ok(url) = std::env::var("GRAFANA_URL") else { return };
        if url.is_empty() || self.contexts.contains_key("env") {
            return;
        }
        self.contexts.insert("env".into(), ContextConfig { url, token_env: Some("GRAFANA_TOKEN".into()), ..Default::default() });
    }

    pub fn context_names(&self) -> Vec<String> {
        self.contexts.keys().cloned().collect()
    }

    pub fn initial_context(&self, requested: Option<&str>) -> Result<String> {
        if let Some(r) = requested {
            if !self.contexts.contains_key(r) {
                bail!("unknown context '{r}' (available: {})", self.context_names().join(", "));
            }
            return Ok(r.to_string());
        }
        Ok(self.default_context.clone().or_else(|| self.contexts.keys().next().cloned()).expect("at least one context"))
    }

    pub fn resolve_context(&self, name: &str) -> Result<Context> {
        let c = self.contexts.get(name).ok_or_else(|| eyre!("unknown context '{name}'"))?;
        let (token, token_error) = match resolve_token(c, &self.base_dir) {
            Ok(t) => (t, None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let cloud = c.cloud.unwrap_or_else(|| is_cloud_url(&c.url));
        Ok(Context {
            name: name.to_string(),
            url: c.url.trim_end_matches('/').to_string(),
            org: c.org,
            token,
            token_error,
            readonly: c.readonly,
            cloud,
            ca_file: c.ca_file.as_ref().map(|p| self.base_dir.join(p)),
            insecure_skip_verify: c.insecure_skip_verify,
        })
    }
}

pub fn is_cloud_url(url: &str) -> bool {
    url.split("://").nth(1).and_then(|rest| rest.split(['/', ':']).next()).is_some_and(|host| host.ends_with(".grafana.net"))
}

fn resolve_token(c: &ContextConfig, base: &Path) -> Result<Option<String>> {
    if let Some(t) = &c.token {
        return Ok(Some(t.clone()));
    }
    if let Some(var) = &c.token_env {
        return match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
            _ => bail!("environment variable {var} is not set"),
        };
    }
    if let Some(file) = &c.token_file {
        let path = base.join(file);
        let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("reading token file {}", path.display()))?;
        return Ok(Some(text.trim().to_string()));
    }
    if let Some(entry) = &c.keyring {
        let (service, account) = keyring_entry(entry);
        return keyring_get(&service, &account).map(Some);
    }
    if let Some(cmd) = &c.token_cmd {
        let out =
            Command::new("sh").arg("-c").arg(cmd).current_dir(base).output().wrap_err_with(|| format!("running token_cmd `{cmd}`"))?;
        if !out.status.success() {
            bail!("token_cmd `{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let token = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().trim().to_string();
        if token.is_empty() {
            bail!("token_cmd `{cmd}` printed nothing");
        }
        return Ok(Some(token));
    }
    Ok(None)
}

/// Splits `service/account` (service defaults to `lazygrafana`).
pub fn keyring_entry(entry: &str) -> (String, String) {
    match entry.split_once('/') {
        Some((s, a)) if !s.is_empty() && !a.is_empty() => (s.to_string(), a.to_string()),
        _ => ("lazygrafana".to_string(), entry.to_string()),
    }
}

fn run_with_stdin(mut cmd: Command, input: Option<&str>) -> Result<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err_with(|| format!("running {program} (is it installed?)"))?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.contains("not activatable") || err.contains("org.freedesktop.secrets") {
            bail!("no Secret Service provider is running (start gnome-keyring, KeePassXC or KWallet), or use token_cmd/token_file instead");
        }
        bail!("{program} failed: {err}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Reads a secret from the OS keyring (Secret Service via `secret-tool`, or the macOS Keychain).
pub fn keyring_get(service: &str, account: &str) -> Result<String> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("security");
        c.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
        c
    } else {
        let mut c = Command::new("secret-tool");
        c.args(["lookup", "service", service, "account", account]);
        c
    };
    cmd.env("LANG", "C");
    let token = run_with_stdin(cmd, None)?;
    if token.is_empty() {
        bail!("no keyring entry for service={service} account={account} (run `lazygrafana auth set <context>`)");
    }
    Ok(token)
}

/// Stores a secret in the OS keyring.
pub fn keyring_set(service: &str, account: &str, secret: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("security");
        c.args(["add-generic-password", "-U", "-s", service, "-a", account, "-w", secret]);
        run_with_stdin(c, None)?;
    } else {
        let mut c = Command::new("secret-tool");
        c.args(["store", "--label", &format!("lazygrafana: {account}"), "service", service, "account", account]);
        run_with_stdin(c, Some(secret))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyring_entries() {
        assert_eq!(keyring_entry("prod"), ("lazygrafana".into(), "prod".into()));
        assert_eq!(keyring_entry("grafana/prod"), ("grafana".into(), "prod".into()));
        assert_eq!(keyring_entry("/x"), ("lazygrafana".into(), "/x".into()));
    }

    #[test]
    fn parses_example_config() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/dev/lazygrafana.yml")).unwrap();
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.default_context.as_deref(), Some("local"));
        assert!(c.contexts["local-viewer"].readonly);
        assert_eq!(c.contexts["team-b"].org, Some(2));
    }

    #[test]
    fn rejects_unknown_fields_and_bad_default() {
        assert!(Config::parse("contexts: {a: {url: x, tokn: y}}").is_err());
        assert!(Config::parse("default_context: b\ncontexts: {a: {url: x}}").is_err());
        assert!(Config::parse("contexts: {a: {org: 1}}").is_err());
    }

    #[test]
    fn token_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t"), "abc\n").unwrap();
        let mut c = Config::parse(
            "contexts:\n  f: {url: 'http://x/', token_file: t}\n  c: {url: http://x, token_cmd: 'echo xyz'}\n  n: {url: http://x}\n  e: {url: http://x, token_env: LG_TEST_MISSING_VAR}",
        )
        .unwrap();
        c.base_dir = dir.path().to_path_buf();
        let f = c.resolve_context("f").unwrap();
        assert_eq!(f.token.as_deref(), Some("abc"));
        assert_eq!(f.url, "http://x");
        assert_eq!(c.resolve_context("c").unwrap().token.as_deref(), Some("xyz"));
        assert_eq!(c.resolve_context("n").unwrap().token, None);
        let e = c.resolve_context("e").unwrap();
        assert!(e.token.is_none() && e.token_error.is_some());
    }

    #[test]
    fn detects_cloud() {
        assert!(is_cloud_url("https://mystack.grafana.net"));
        assert!(is_cloud_url("https://mystack.grafana.net:443/"));
        assert!(!is_cloud_url("https://grafana.example.com"));
        assert!(!is_cloud_url("https://grafana.net.evil.com"));
    }
}
