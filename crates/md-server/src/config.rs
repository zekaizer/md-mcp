//! Server configuration, read from the environment and CLI args (fail-closed).
//!
//! Transport selection (ADR-0013): a `--http` / `--stdio` CLI flag wins; else the
//! `MD_TRANSPORT` env var; else HTTP. The parsing and precedence are pure
//! functions so they are unit-testable without touching the process environment.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Default HTTP bind address: loopback, port 7654 (an uncommon port, chosen to
/// avoid the heavily-contended 8080/8000 band). Override with `MD_HTTP_ADDR`.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:7654";

/// The transport the server speaks.
#[derive(Debug, Clone)]
pub enum Transport {
    /// MCP over stdio (stdin/stdout). Logs go to stderr.
    Stdio,
    /// MCP over Streamable HTTP.
    Http(HttpConfig),
}

/// A selectable transport, before any HTTP config is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

/// HTTP transport configuration.
///
/// The two allowlists share one shape: `None` = apply the guard's secure default,
/// `Some(vec![])` = the literal `*` (disable the guard), `Some(non-empty)` =
/// restrict to that list. A malformed list (e.g. `,`) is a hard error, never a
/// silent disable — see [`parse_allowlist`].
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Socket address to bind (`MD_HTTP_ADDR`).
    pub addr: SocketAddr,
    /// When `Some`, every request must carry `Authorization: Bearer <token>`
    /// (`MD_HTTP_TOKEN`). Trimmed; blank means no auth.
    pub token: Option<String>,
    /// `Host`-header allowlist (`MD_HTTP_ALLOWED_HOSTS`). `None` keeps rmcp's
    /// loopback default; `Some(vec![])` disables the guard.
    pub allowed_hosts: Option<Vec<String>>,
    /// `Origin`-header allowlist (`MD_HTTP_ALLOWED_ORIGINS`). `None` applies the
    /// loopback-origin default (blocks cross-site browser requests while letting
    /// header-less non-browser clients through); `Some(vec![])` disables the guard.
    pub allowed_origins: Option<Vec<String>>,
    /// Directory for server state (`MD_STATE_DIR`) — holds the OAuth token store
    /// (ADR-0014). Used only when OAuth is enabled (`token` set).
    pub state_dir: PathBuf,
}

impl HttpConfig {
    /// Resolve HTTP config from raw string inputs (pure; env-free).
    fn resolve(
        addr: Option<String>,
        token: Option<String>,
        allowed_hosts: Option<String>,
        allowed_origins: Option<String>,
        state_dir: PathBuf,
    ) -> Result<Self> {
        let addr_str = addr
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_string());
        let addr: SocketAddr = addr_str
            .trim()
            .parse()
            .with_context(|| format!("MD_HTTP_ADDR is not a valid socket address: {addr_str:?}"))?;

        // Trim the token: a trailing newline from `MD_HTTP_TOKEN=$(cat secret)` is
        // never sent in an HTTP header, so it would otherwise wedge auth shut.
        let token = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());

        let allowed_hosts = parse_allowlist(allowed_hosts, "MD_HTTP_ALLOWED_HOSTS")?;
        let allowed_origins = parse_allowlist(allowed_origins, "MD_HTTP_ALLOWED_ORIGINS")?;

        Ok(Self {
            addr,
            token,
            allowed_hosts,
            allowed_origins,
            state_dir,
        })
    }

    /// True when the co-hosted OAuth server should run: HTTP with a token set (the
    /// token is the `/authorize` ownership gate — ADR-0014).
    #[must_use]
    pub fn oauth_enabled(&self) -> bool {
        self.token.is_some()
    }
}

/// Resolve the server state directory: `MD_STATE_DIR`, else `$XDG_STATE_HOME/md-mcp`,
/// else `$HOME/.local/state/md-mcp`, else a relative fallback (created on first write).
fn resolve_state_dir(
    md_state_dir: Option<String>,
    xdg_state_home: Option<String>,
    home: Option<String>,
) -> PathBuf {
    let non_empty = |s: String| (!s.trim().is_empty()).then_some(s);
    if let Some(dir) = md_state_dir.and_then(non_empty) {
        return PathBuf::from(dir.trim());
    }
    if let Some(xdg) = xdg_state_home.and_then(non_empty) {
        return PathBuf::from(xdg.trim()).join("md-mcp");
    }
    if let Some(home) = home.and_then(non_empty) {
        return PathBuf::from(home.trim()).join(".local/state/md-mcp");
    }
    PathBuf::from(".md-mcp-state")
}

/// Parse a comma-separated allowlist. `None`/blank → `None` (caller's secure
/// default); the literal `*` → `Some(vec![])` (disable the guard); otherwise the
/// non-empty entries. A non-`*` value that yields no entries (`,`, ` , `) is an
/// error, so a malformed list can never fail open into a disabled guard.
fn parse_allowlist(raw: Option<String>, var: &str) -> Result<Option<Vec<String>>> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some("*") => Ok(Some(Vec::new())),
        Some(list) => {
            let items: Vec<String> = list
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect();
            if items.is_empty() {
                bail!("{var} lists no entries; use \"*\" to disable the guard or unset it");
            }
            Ok(Some(items))
        }
    }
}

/// Event journal + commit hook configuration (ADR-0017).
#[derive(Debug, Clone, Default)]
pub struct EventsConfig {
    /// Whether the journal is written (`MD_EVENTS=1`, or implied by a hook).
    pub enabled: bool,
    /// Hook command (`MD_ON_COMMIT_HOOK`), run per record with JSON on stdin.
    pub hook: Option<String>,
}

impl EventsConfig {
    /// Resolve from raw env values (pure; env-free). A hook implies the
    /// journal: its catch-up story depends on the journal existing.
    fn resolve(events: Option<String>, hook: Option<String>) -> Result<Self> {
        let hook = hook.map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
        let enabled = parse_flag(events, "MD_EVENTS")? || hook.is_some();
        Ok(Self { enabled, hook })
    }
}

/// Parse an on/off env flag. Unset/blank → off; `1`/`true` → on; anything else
/// is a hard error, never a silent off (fail-closed, as everywhere here).
fn parse_flag(raw: Option<String>, var: &str) -> Result<bool> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(false),
        Some("1" | "true") => Ok(true),
        Some(other) => bail!("{var} must be \"1\" or \"true\" (or unset), got {other:?}"),
    }
}

/// Git sync configuration (ADR-0016) and automation layers (ADR-0018).
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    /// Whether git sync is requested (`MD_GIT_SYNC=1`); gates the
    /// `sync_vault` tool and is the base every automation layer requires.
    pub sync: bool,
    /// Per-batch auto-commit (`MD_GIT_AUTO_COMMIT=1`).
    pub auto_commit: bool,
}

impl GitConfig {
    fn resolve(sync: Option<String>, auto_commit: Option<String>) -> Result<Self> {
        let sync = parse_flag(sync, "MD_GIT_SYNC")?;
        let auto_commit = parse_flag(auto_commit, "MD_GIT_AUTO_COMMIT")?;
        // An automation layer without the base is a misconfiguration, not a
        // silent no-op (ADR-0018).
        if auto_commit && !sync {
            bail!("MD_GIT_AUTO_COMMIT requires MD_GIT_SYNC=1");
        }
        Ok(Self { sync, auto_commit })
    }
}

/// Runtime configuration for the md-mcp server.
#[derive(Debug, Clone)]
pub struct Config {
    /// The vault root directory.
    pub vault_dir: PathBuf,
    /// The selected transport.
    pub transport: Transport,
    /// Event journal + hook (ADR-0017).
    pub events: EventsConfig,
    /// Git sync (ADR-0016).
    pub git: GitConfig,
}

impl Config {
    /// Read configuration from the environment and CLI args. `MD_VAULT` is
    /// required; `args` is the process arguments **excluding** the program name.
    pub fn from_env_and_args(args: &[String]) -> Result<Self> {
        let vault_dir: PathBuf = std::env::var("MD_VAULT")
            .context("MD_VAULT must be set to the vault directory")?
            .into();

        let cli_kind = parse_cli_transport(args)?;
        // A set-but-blank MD_TRANSPORT (e.g. `export MD_TRANSPORT=`) falls through
        // to the default rather than hard-failing.
        let env_kind = match std::env::var("MD_TRANSPORT") {
            Ok(v) if !v.trim().is_empty() => Some(parse_transport_kind(&v)?),
            _ => None,
        };

        let transport = match select_transport(cli_kind, env_kind) {
            TransportKind::Stdio => Transport::Stdio,
            TransportKind::Http => Transport::Http(HttpConfig::resolve(
                std::env::var("MD_HTTP_ADDR").ok(),
                std::env::var("MD_HTTP_TOKEN").ok(),
                std::env::var("MD_HTTP_ALLOWED_HOSTS").ok(),
                std::env::var("MD_HTTP_ALLOWED_ORIGINS").ok(),
                resolve_state_dir(
                    std::env::var("MD_STATE_DIR").ok(),
                    std::env::var("XDG_STATE_HOME").ok(),
                    std::env::var("HOME").ok(),
                ),
            )?),
        };

        let events = EventsConfig::resolve(
            std::env::var("MD_EVENTS").ok(),
            std::env::var("MD_ON_COMMIT_HOOK").ok(),
        )?;
        let git = GitConfig::resolve(
            std::env::var("MD_GIT_SYNC").ok(),
            std::env::var("MD_GIT_AUTO_COMMIT").ok(),
        )?;

        Ok(Self {
            vault_dir,
            transport,
            events,
            git,
        })
    }
}

/// Resolve the transport kind: CLI flag wins, else env, else HTTP.
fn select_transport(cli: Option<TransportKind>, env: Option<TransportKind>) -> TransportKind {
    cli.or(env).unwrap_or(TransportKind::Http)
}

/// Parse a `--http` / `--stdio` flag out of `args`. Errors on an unknown
/// argument or on both flags being present; `None` when neither is given.
fn parse_cli_transport(args: &[String]) -> Result<Option<TransportKind>> {
    let mut kind = None;
    for arg in args {
        let next = match arg.as_str() {
            "--http" => TransportKind::Http,
            "--stdio" => TransportKind::Stdio,
            other => bail!("unknown argument {other:?} (expected --http or --stdio)"),
        };
        match kind {
            None => kind = Some(next),
            Some(prev) if prev == next => {}
            Some(_) => bail!("--http and --stdio are mutually exclusive"),
        }
    }
    Ok(kind)
}

/// Parse the `MD_TRANSPORT` value.
fn parse_transport_kind(value: &str) -> Result<TransportKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "http" => Ok(TransportKind::Http),
        "stdio" => Ok(TransportKind::Stdio),
        other => bail!("MD_TRANSPORT must be \"http\" or \"stdio\", got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn sd() -> PathBuf {
        PathBuf::from("/tmp/md-mcp-test-state")
    }

    #[test]
    fn cli_flag_parses_each_transport() {
        assert_eq!(
            parse_cli_transport(&args(&["--http"])).unwrap(),
            Some(TransportKind::Http)
        );
        assert_eq!(
            parse_cli_transport(&args(&["--stdio"])).unwrap(),
            Some(TransportKind::Stdio)
        );
        assert_eq!(parse_cli_transport(&args(&[])).unwrap(), None);
    }

    #[test]
    fn cli_flag_rejects_conflicts_and_unknowns() {
        assert!(parse_cli_transport(&args(&["--http", "--stdio"])).is_err());
        assert!(parse_cli_transport(&args(&["--bogus"])).is_err());
        // A repeated identical flag is harmless.
        assert_eq!(
            parse_cli_transport(&args(&["--http", "--http"])).unwrap(),
            Some(TransportKind::Http)
        );
    }

    #[test]
    fn env_transport_kind_is_case_insensitive() {
        assert_eq!(parse_transport_kind("http").unwrap(), TransportKind::Http);
        assert_eq!(parse_transport_kind("STDIO").unwrap(), TransportKind::Stdio);
        assert_eq!(parse_transport_kind(" Http ").unwrap(), TransportKind::Http);
        assert!(parse_transport_kind("ws").is_err());
    }

    #[test]
    fn precedence_cli_over_env_over_default_http() {
        // CLI wins over env.
        assert_eq!(
            select_transport(Some(TransportKind::Stdio), Some(TransportKind::Http)),
            TransportKind::Stdio
        );
        // Env used when no CLI flag.
        assert_eq!(
            select_transport(None, Some(TransportKind::Stdio)),
            TransportKind::Stdio
        );
        // Default is HTTP.
        assert_eq!(select_transport(None, None), TransportKind::Http);
    }

    #[test]
    fn http_config_defaults_to_loopback_7654() {
        let cfg = HttpConfig::resolve(None, None, None, None, sd()).unwrap();
        assert_eq!(cfg.addr, "127.0.0.1:7654".parse().unwrap());
        assert!(cfg.token.is_none());
        assert!(cfg.allowed_hosts.is_none());
        assert!(cfg.allowed_origins.is_none());
        assert!(!cfg.oauth_enabled(), "no token → OAuth off");
    }

    #[test]
    fn http_config_parses_addr_token_and_lists() {
        let cfg = HttpConfig::resolve(
            Some("0.0.0.0:9000".into()),
            Some("s3cret".into()),
            Some("example.com, 10.0.0.5".into()),
            Some("https://example.com".into()),
            sd(),
        )
        .unwrap();
        assert_eq!(cfg.addr, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(cfg.token.as_deref(), Some("s3cret"));
        assert!(cfg.oauth_enabled(), "token set → OAuth on");
        assert_eq!(
            cfg.allowed_hosts,
            Some(vec!["example.com".to_string(), "10.0.0.5".to_string()])
        );
        assert_eq!(
            cfg.allowed_origins,
            Some(vec!["https://example.com".to_string()])
        );
    }

    #[test]
    fn http_config_empty_token_is_none_and_star_disables_guards() {
        let cfg = HttpConfig::resolve(
            None,
            Some(String::new()),
            Some("*".into()),
            Some("*".into()),
            sd(),
        )
        .unwrap();
        assert!(cfg.token.is_none(), "empty token must mean no auth");
        assert_eq!(cfg.allowed_hosts, Some(Vec::new()), "`*` disables hosts");
        assert_eq!(
            cfg.allowed_origins,
            Some(Vec::new()),
            "`*` disables origins"
        );
    }

    #[test]
    fn http_config_trims_token_and_drops_whitespace_only() {
        // A trailing newline (common from `$(cat secret)`) is trimmed.
        let cfg = HttpConfig::resolve(None, Some("s3cret\n".into()), None, None, sd()).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("s3cret"));
        // A whitespace-only token is treated as no auth, not a phantom secret.
        let cfg = HttpConfig::resolve(None, Some("   ".into()), None, None, sd()).unwrap();
        assert!(cfg.token.is_none());
    }

    #[test]
    fn state_dir_resolution_precedence() {
        // MD_STATE_DIR wins.
        assert_eq!(
            resolve_state_dir(
                Some("/explicit".into()),
                Some("/xdg".into()),
                Some("/home".into())
            ),
            PathBuf::from("/explicit")
        );
        // else XDG_STATE_HOME/md-mcp.
        assert_eq!(
            resolve_state_dir(None, Some("/xdg".into()), Some("/home".into())),
            PathBuf::from("/xdg/md-mcp")
        );
        // else HOME/.local/state/md-mcp.
        assert_eq!(
            resolve_state_dir(None, None, Some("/home".into())),
            PathBuf::from("/home/.local/state/md-mcp")
        );
        // blank values are skipped.
        assert_eq!(
            resolve_state_dir(Some("  ".into()), None, Some("/home".into())),
            PathBuf::from("/home/.local/state/md-mcp")
        );
    }

    #[test]
    fn allowlist_rejects_malformed_lists_instead_of_failing_open() {
        // A separators-only value must error, never silently disable the guard.
        assert!(parse_allowlist(Some(",".into()), "X").is_err());
        assert!(parse_allowlist(Some(" , ".into()), "X").is_err());
        // The explicit escape hatch and the unset case still work.
        assert_eq!(
            parse_allowlist(Some("*".into()), "X").unwrap(),
            Some(vec![])
        );
        assert_eq!(parse_allowlist(None, "X").unwrap(), None);
        assert_eq!(parse_allowlist(Some("".into()), "X").unwrap(), None);
    }

    #[test]
    fn http_config_rejects_bad_addr() {
        assert!(HttpConfig::resolve(Some("not-an-addr".into()), None, None, None, sd()).is_err());
    }
}
