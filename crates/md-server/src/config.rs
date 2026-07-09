//! Server configuration, read from CLI args and the environment (fail-closed).
//!
//! Arguments are parsed with clap ([ADR-0023](../../../docs/adr/0023-cli-arguments-and-clap.md)):
//! every setting is a `--flag` whose value falls back to its `MD_*` env var, with
//! precedence CLI > env > default. Secrets never become value flags — the bearer
//! token stays in `MD_HTTP_TOKEN` or `--http-token-file <path>`, so it is never on
//! argv. Transport selection (ADR-0013): a `--http` / `--stdio` flag wins; else the
//! `MD_TRANSPORT` env var; else HTTP. The value parsing and precedence are pure
//! functions so they are unit-testable without touching the process environment.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

/// md-mcp server command line. Each option falls back to its `MD_*` env var
/// (ADR-0023); the bearer token is env/file only, never a value flag.
#[derive(Debug, Parser)]
#[command(
    name = "md-server",
    version,
    about = "md-mcp — an MCP server over a Markdown vault"
)]
pub struct Cli {
    /// Serve MCP over Streamable HTTP (the default transport).
    #[arg(long)]
    pub http: bool,
    /// Serve MCP over stdio (stdin/stdout); logs go to stderr.
    #[arg(long, conflicts_with = "http")]
    pub stdio: bool,

    /// Vault root directory (required).
    #[arg(long, env = "MD_VAULT")]
    pub vault: PathBuf,

    /// HTTP bind address [default: 127.0.0.1:7654].
    #[arg(long, env = "MD_HTTP_ADDR")]
    pub http_addr: Option<String>,
    /// Read the HTTP bearer token from this file (trimmed). Keeps the secret
    /// off argv; falls back to the MD_HTTP_TOKEN env var.
    #[arg(long, value_name = "PATH")]
    pub http_token_file: Option<PathBuf>,
    /// Comma-separated Host allowlist ("*" disables the guard).
    #[arg(long, env = "MD_HTTP_ALLOWED_HOSTS")]
    pub http_allowed_hosts: Option<String>,
    /// Comma-separated Origin allowlist ("*" disables the guard).
    #[arg(long, env = "MD_HTTP_ALLOWED_ORIGINS")]
    pub http_allowed_origins: Option<String>,
    /// Server state directory (OAuth token store).
    #[arg(long, env = "MD_STATE_DIR")]
    pub state_dir: Option<String>,

    /// Write the event journal.
    #[arg(long, env = "MD_EVENTS")]
    pub events: Option<String>,
    /// Commit hook command, run per record with JSON on stdin.
    #[arg(long, env = "MD_ON_COMMIT_HOOK")]
    pub on_commit_hook: Option<String>,

    /// Enable git sync (gates sync_vault and every automation layer).
    #[arg(long, env = "MD_GIT_SYNC")]
    pub git_sync: Option<String>,
    /// Per-batch auto-commit (requires git sync).
    #[arg(long, env = "MD_GIT_AUTO_COMMIT")]
    pub git_auto_commit: Option<String>,
    /// Debounced push, N seconds after the most recent commit.
    #[arg(long, env = "MD_GIT_AUTO_PUSH_SECS")]
    pub git_auto_push_secs: Option<String>,
    /// Periodic full sync interval, in seconds.
    #[arg(long, env = "MD_GIT_SYNC_INTERVAL_SECS")]
    pub git_sync_interval_secs: Option<String>,

    /// Vault-relative intro note advertised in the server instructions.
    #[arg(long, env = "MD_INTRO_NOTE")]
    pub intro_note: Option<String>,
}

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

/// Log output format (`MD_LOG_FORMAT`, ADR-0021).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines (the dev default).
    #[default]
    Text,
    /// One flattened JSON object per line, for log shipping.
    Json,
}

/// Parse `MD_LOG_FORMAT` from the environment. Unset/blank → text; anything
/// other than `text`/`json` is a hard error (fail-closed, as everywhere here).
pub fn log_format_from_env() -> Result<LogFormat> {
    parse_log_format(std::env::var("MD_LOG_FORMAT").ok())
}

fn parse_log_format(raw: Option<String>) -> Result<LogFormat> {
    match raw.as_deref().map(str::trim) {
        None | Some("") | Some("text") => Ok(LogFormat::Text),
        Some("json") => Ok(LogFormat::Json),
        Some(other) => {
            bail!("MD_LOG_FORMAT must be \"text\" or \"json\" (or unset), got {other:?}")
        }
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
    /// Debounced push, N seconds after the most recent commit
    /// (`MD_GIT_AUTO_PUSH_SECS`).
    pub auto_push_secs: Option<u64>,
    /// Periodic full sync (`MD_GIT_SYNC_INTERVAL_SECS`).
    pub sync_interval_secs: Option<u64>,
}

impl GitConfig {
    fn resolve(
        sync: Option<String>,
        auto_commit: Option<String>,
        auto_push_secs: Option<String>,
        sync_interval_secs: Option<String>,
    ) -> Result<Self> {
        let sync = parse_flag(sync, "MD_GIT_SYNC")?;
        let auto_commit = parse_flag(auto_commit, "MD_GIT_AUTO_COMMIT")?;
        let auto_push_secs = parse_secs(auto_push_secs, "MD_GIT_AUTO_PUSH_SECS")?;
        let sync_interval_secs = parse_secs(sync_interval_secs, "MD_GIT_SYNC_INTERVAL_SECS")?;
        // An automation layer without the base is a misconfiguration, not a
        // silent no-op (ADR-0018).
        if !sync {
            if auto_commit {
                bail!("MD_GIT_AUTO_COMMIT requires MD_GIT_SYNC=1");
            }
            if auto_push_secs.is_some() {
                bail!("MD_GIT_AUTO_PUSH_SECS requires MD_GIT_SYNC=1");
            }
            if sync_interval_secs.is_some() {
                bail!("MD_GIT_SYNC_INTERVAL_SECS requires MD_GIT_SYNC=1");
            }
        }
        Ok(Self {
            sync,
            auto_commit,
            auto_push_secs,
            sync_interval_secs,
        })
    }
}

/// Parse a positive-seconds env var. Unset/blank → `None`; `0` or a non-number
/// is a hard error, never a silent off.
fn parse_secs(raw: Option<String>, var: &str) -> Result<Option<u64>> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => {
            let n: u64 = s
                .parse()
                .with_context(|| format!("{var} must be a positive integer, got {s:?}"))?;
            if n == 0 {
                bail!("{var} must be a positive integer, got 0");
            }
            Ok(Some(n))
        }
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
    /// Vault-relative path of the introduction note advertised in the server
    /// instructions (`MD_INTRO_NOTE`). Unset/blank → no advertisement.
    pub intro_note: Option<String>,
}

impl Config {
    /// Build a `Config` from parsed CLI args. The bearer token and `MD_TRANSPORT`
    /// are read here from the file/environment (they are not value flags), so the
    /// argv is never a channel for the secret.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        // A set-but-blank MD_TRANSPORT (e.g. `export MD_TRANSPORT=`) falls through
        // to the default rather than hard-failing.
        let env_kind = match std::env::var("MD_TRANSPORT") {
            Ok(v) if !v.trim().is_empty() => Some(parse_transport_kind(&v)?),
            _ => None,
        };

        let transport = match select_transport(cli_transport(cli.http, cli.stdio), env_kind) {
            TransportKind::Stdio => Transport::Stdio,
            TransportKind::Http => Transport::Http(HttpConfig::resolve(
                cli.http_addr,
                resolve_token(cli.http_token_file.as_deref())?,
                cli.http_allowed_hosts,
                cli.http_allowed_origins,
                resolve_state_dir(
                    cli.state_dir,
                    std::env::var("XDG_STATE_HOME").ok(),
                    std::env::var("HOME").ok(),
                ),
            )?),
        };

        let events = EventsConfig::resolve(cli.events, cli.on_commit_hook)?;
        let git = GitConfig::resolve(
            cli.git_sync,
            cli.git_auto_commit,
            cli.git_auto_push_secs,
            cli.git_sync_interval_secs,
        )?;

        let intro_note = cli
            .intro_note
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());

        Ok(Self {
            vault_dir: cli.vault,
            transport,
            events,
            git,
            intro_note,
        })
    }
}

/// Resolve the bearer token from its non-argv sources: `--http-token-file` (read
/// and trimmed) wins over the `MD_HTTP_TOKEN` env var. A given token file that
/// cannot be read is a hard error — never a silent fall-through to no auth.
fn resolve_token(token_file: Option<&std::path::Path>) -> Result<Option<String>> {
    if let Some(path) = token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read --http-token-file {}", path.display()))?;
        return Ok(Some(raw));
    }
    Ok(std::env::var("MD_HTTP_TOKEN").ok())
}

/// The transport a `--http` / `--stdio` flag pair selects; `None` when neither is
/// given (clap rejects both). Env/default precedence is applied by the caller.
fn cli_transport(http: bool, stdio: bool) -> Option<TransportKind> {
    match (http, stdio) {
        (_, true) => Some(TransportKind::Stdio),
        (true, false) => Some(TransportKind::Http),
        (false, false) => None,
    }
}

/// Resolve the transport kind: CLI flag wins, else env, else HTTP.
fn select_transport(cli: Option<TransportKind>, env: Option<TransportKind>) -> TransportKind {
    cli.or(env).unwrap_or(TransportKind::Http)
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

    #[test]
    fn cli_parses_flags_and_env_fallback() {
        // Explicit flags win.
        let cli = Cli::try_parse_from(["md-server", "--stdio", "--vault", "/v"]).unwrap();
        assert!(cli.stdio && !cli.http);
        assert_eq!(cli.vault, PathBuf::from("/v"));
        // --http and --stdio are mutually exclusive.
        assert!(Cli::try_parse_from(["md-server", "--http", "--stdio", "--vault", "/v"]).is_err());
        // An unknown flag is rejected by clap.
        assert!(Cli::try_parse_from(["md-server", "--bogus", "--vault", "/v"]).is_err());
        // vault is required (no --vault and no MD_VAULT in this parse).
        assert!(Cli::try_parse_from(["md-server"]).is_err());
    }

    #[test]
    fn cli_transport_flag_maps_to_kind() {
        assert_eq!(cli_transport(true, false), Some(TransportKind::Http));
        assert_eq!(cli_transport(false, true), Some(TransportKind::Stdio));
        assert_eq!(cli_transport(false, false), None);
    }

    #[test]
    fn resolve_token_reads_file_over_env_and_errors_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tok");
        std::fs::write(&path, "s3cret\n").unwrap();
        // The file's contents come back raw (HttpConfig::resolve trims).
        assert_eq!(
            resolve_token(Some(&path)).unwrap().as_deref(),
            Some("s3cret\n")
        );
        // No file → falls back to the env var (absent here → None).
        // A given-but-unreadable file is a hard error, never silent no-auth.
        assert!(resolve_token(Some(&dir.path().join("missing"))).is_err());
    }

    #[test]
    fn log_format_parses_fail_closed() {
        assert_eq!(parse_log_format(None).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some(" ".into())).unwrap(), LogFormat::Text);
        assert_eq!(
            parse_log_format(Some("text".into())).unwrap(),
            LogFormat::Text
        );
        assert_eq!(
            parse_log_format(Some("json".into())).unwrap(),
            LogFormat::Json
        );
        assert!(parse_log_format(Some("yaml".into())).is_err());
    }

    fn sd() -> PathBuf {
        PathBuf::from("/tmp/md-mcp-test-state")
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
