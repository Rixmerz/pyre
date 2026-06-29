//! GitHub OAuth Device Flow — Tauri command module.
//!
//! Architecture:
//!   - All GitHub tokens are stored in a 0600 plaintext file at
//!     ~/.config/pyre/github-token (XDG_CONFIG_HOME respected).
//!     They never touch state.db, logs, stdout, or the OS keychain.
//!   - client_id is a compile-time default, overridable via PYRE_GITHUB_CLIENT_ID env.
//!     Device flow requires no client_secret — none is shipped.
//!   - In-flight device_code is held in GithubState (Tauri managed, per-app-instance Mutex).
//!   - Pure parser functions are factored out so tests never touch the network or the file.

use std::path::PathBuf;
use serde::Serialize;
use tauri::State;
use tokio::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const DEFAULT_CLIENT_ID: &str = "Ov23li1g0XoYJex02nIG";
const GITHUB_SCOPE: &str = "read:user";

fn client_id() -> String {
    std::env::var("PYRE_GITHUB_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Token file helpers — the ONLY place the token is read from / written to disk
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the absolute path to the token file.
///
/// Respects `XDG_CONFIG_HOME`; falls back to `$HOME/.config/pyre/github-token`.
fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            h.push(".config");
            h
        });
    base.join("pyre").join("github-token")
}

/// Write `token` to the token file with 0600 permissions (atomically on unix).
///
/// The parent directory is created with 0700 permissions if absent.
/// On unix the file open uses `OpenOptionsExt::mode(0o600)` so the file is
/// NEVER briefly world- or group-readable — no write-then-chmod race.
/// On non-unix a plain create/truncate is used (Linux-first per project rules).
fn store_token(token: &str) -> std::io::Result<()> {
    use std::io::Write;

    let path = token_path();
    let parent = path.parent().expect("token path always has a parent dir");
    std::fs::create_dir_all(parent)?;

    // Best-effort: restrict parent dir to owner-only access.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            parent,
            std::fs::Permissions::from_mode(0o700),
        );
    }

    // Build OpenOptions; on unix add mode 0600 before open() so the inode is
    // created with the correct permissions — never a world-readable moment.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts.open(&path)?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

/// Read the stored token, trimming trailing whitespace.
///
/// Returns `None` when the file is absent or empty.
fn load_token() -> Option<String> {
    let content = std::fs::read_to_string(token_path()).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Delete the token file. Idempotent — ignores `NotFound`.
fn clear_token() {
    match std::fs::remove_file(token_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public types returned to the GUI (all Serialize)
// ─────────────────────────────────────────────────────────────────────────────

/// Returned by `github_device_start`. The GUI uses user_code + verification_uri
/// to display the device-code modal. device_code is intentionally excluded.
#[derive(Serialize)]
pub struct DeviceStart {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u32,
    pub interval: u32,
}

/// Polling outcome returned to the GUI. Uses a discriminated-union tag.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PollState {
    Pending,
    Authorized,
    Denied,
    Expired,
    SlowDown,
}

/// GitHub user identity returned by `github_account`.
#[derive(Serialize)]
pub struct Account {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
    pub html_url: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Managed state
// ─────────────────────────────────────────────────────────────────────────────

/// Tauri-managed state holding the in-flight device_code for the current
/// authorization attempt.  Cleared on success, expiry, or denial.
#[derive(Default)]
pub struct GithubState {
    device_code: Mutex<Option<String>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal poll result (carries the token so the command can write it to disk)
// ─────────────────────────────────────────────────────────────────────────────

/// Internal parse result for `github_device_poll`. Carries the raw token
/// in the `Authorized` variant so the command (not the pure parser) writes
/// it to the token file. Tests inspect this type without touching the file.
#[derive(Debug)]
pub(crate) enum PollResult {
    Pending,
    /// Contains the raw access token — stored by the command, not the parser.
    Authorized(String),
    Denied,
    Expired,
    SlowDown,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure parser functions — NO network, NO file I/O, safe to unit-test
// ─────────────────────────────────────────────────────────────────────────────

/// Parse GitHub's `POST /login/device/code` JSON response.
///
/// Returns `(DeviceStart, device_code)` where `device_code` is kept server-side
/// for polling and never sent to the GUI.
pub(crate) fn parse_device_start(json: &str) -> Result<(DeviceStart, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("device/code response parse error: {e}"))?;

    let device_code = v["device_code"]
        .as_str()
        .ok_or("device/code response: missing device_code")?
        .to_string();
    let user_code = v["user_code"]
        .as_str()
        .ok_or("device/code response: missing user_code")?
        .to_string();
    let verification_uri = v["verification_uri"]
        .as_str()
        .ok_or("device/code response: missing verification_uri")?
        .to_string();
    let expires_in = v["expires_in"]
        .as_u64()
        .ok_or("device/code response: missing expires_in")? as u32;
    let interval = v["interval"]
        .as_u64()
        .ok_or("device/code response: missing interval")? as u32;

    Ok((
        DeviceStart {
            user_code,
            verification_uri,
            expires_in,
            interval,
        },
        device_code,
    ))
}

/// Parse GitHub's `POST /login/oauth/access_token` JSON response.
///
/// Maps GitHub error codes to `PollResult` variants:
/// - `authorization_pending` / `slow_down` → `Pending`
/// - `expired_token` → `Expired`
/// - `access_denied` → `Denied`
/// - `access_token` present → `Authorized(token)` (token must be stored by caller)
/// - any other error → `Err(description)`
pub(crate) fn parse_poll(json: &str) -> Result<PollResult, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("access_token response parse error: {e}"))?;

    // Success path: access_token present and non-empty.
    if let Some(token) = v["access_token"].as_str() {
        if !token.is_empty() {
            return Ok(PollResult::Authorized(token.to_string()));
        }
    }

    // Error path.
    let error = v["error"].as_str().unwrap_or("");
    match error {
        "authorization_pending" => Ok(PollResult::Pending),
        "slow_down" => Ok(PollResult::SlowDown),
        "expired_token" => Ok(PollResult::Expired),
        "access_denied" => Ok(PollResult::Denied),
        "" => Err("access_token response has neither access_token nor error field".to_string()),
        other => {
            let desc = v["error_description"].as_str().unwrap_or(other);
            Err(desc.to_string())
        }
    }
}

/// Parse GitHub's `GET /user` JSON response.
pub(crate) fn parse_account(json: &str) -> Result<Account, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("user response parse error: {e}"))?;

    let login = v["login"]
        .as_str()
        .ok_or("user response: missing login")?
        .to_string();
    let name = v["name"].as_str().map(|s| s.to_string());
    let avatar_url = v["avatar_url"]
        .as_str()
        .ok_or("user response: missing avatar_url")?
        .to_string();
    let html_url = v["html_url"]
        .as_str()
        .ok_or("user response: missing html_url")?
        .to_string();

    Ok(Account {
        login,
        name,
        avatar_url,
        html_url,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP client helper
// ─────────────────────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("pyre")
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tauri commands
// ─────────────────────────────────────────────────────────────────────────────

/// Start the GitHub Device Flow.
///
/// POSTs to `https://github.com/login/device/code`, stores the returned
/// `device_code` in managed state, and returns `DeviceStart` (without the
/// device_code) for the GUI to render the modal.
#[tauri::command]
pub async fn github_device_start(
    state: State<'_, GithubState>,
) -> Result<DeviceStart, String> {
    let params = [
        ("client_id", client_id()),
        ("scope", GITHUB_SCOPE.to_string()),
    ];

    let client = http_client()?;
    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("device/code request failed: {e}"))?;

    let body = resp
        .text()
        .await
        .map_err(|e| format!("device/code read body failed: {e}"))?;

    let (device_start, device_code) = parse_device_start(&body)?;

    *state.device_code.lock().await = Some(device_code);

    Ok(device_start)
}

/// Poll the GitHub Device Flow for authorization status.
///
/// Reads the in-flight `device_code` from managed state. On success, writes
/// the token to ~/.config/pyre/github-token (0600) and clears the in-flight
/// code. Returns a `PollState` discriminant — the token NEVER surfaces to the GUI.
///
/// The GUI should call this on the `interval` returned by `github_device_start`.
#[tauri::command]
pub async fn github_device_poll(
    state: State<'_, GithubState>,
) -> Result<PollState, String> {
    let device_code = state
        .device_code
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no device flow in progress".to_string())?;

    let params = [
        ("client_id", client_id()),
        ("device_code", device_code),
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
    ];

    let client = http_client()?;
    let resp = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("access_token request failed: {e}"))?;

    let body = resp
        .text()
        .await
        .map_err(|e| format!("access_token read body failed: {e}"))?;

    match parse_poll(&body)? {
        PollResult::Authorized(token) => {
            // GitHub granted the token — always evict the device_code so we never re-poll a spent code.
            *state.device_code.lock().await = None;
            // Token goes ONLY into ~/.config/pyre/github-token (0600) — never logged, never serialized.
            // Tag store failures with TOKEN_STORE_FAILED: so the GUI treats them as terminal.
            store_token(&token).map_err(|e| {
                format!("TOKEN_STORE_FAILED: couldn't write ~/.config/pyre/github-token: {e}")
            })?;
            Ok(PollState::Authorized)
        }
        PollResult::Pending => Ok(PollState::Pending),
        PollResult::SlowDown => Ok(PollState::SlowDown),
        PollResult::Denied => Ok(PollState::Denied),
        PollResult::Expired => Ok(PollState::Expired),
    }
}

/// Fetch the linked GitHub account identity from the API.
///
/// Returns `None` when no token file exists or when the stored token is stale
/// (401 from the API → token file deleted automatically).
/// Returns `Some(Account)` on success.
#[tauri::command]
pub async fn github_account() -> Result<Option<Account>, String> {
    let token = match load_token() {
        Some(t) => t,
        None => return Ok(None),
    };

    let client = http_client()?;
    let resp = client
        .get("https://api.github.com/user")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("user request failed: {e}"))?;

    let status = resp.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        // Stale or revoked token — purge it from the file.
        clear_token();
        return Ok(None);
    }

    if !status.is_success() {
        return Err(format!("user request failed with HTTP {status}"));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("user read body failed: {e}"))?;

    Ok(Some(parse_account(&body)?))
}

/// Remove the stored GitHub token file.
///
/// Idempotent — treats "token not found" as success. Does NOT revoke the
/// grant on GitHub; the GUI should link to
/// github.com/settings/connections/applications/<client_id> for server-side
/// revocation.
#[tauri::command]
pub async fn github_disconnect() -> Result<(), String> {
    clear_token();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// PR/CI public types
// ─────────────────────────────────────────────────────────────────────────────

/// Folded CI state for the open PR's head commit check-runs.
///
/// Serialized as lowercase strings (e.g. `"success"`, `"failure"`, `"none"`).
///
/// # Variant note
/// `CiState::None` is a valid Rust enum variant name — it is not the
/// `Option::None` keyword.  It means "PR found but no check-runs recorded
/// yet", distinct from the outer `Option<PrCiInfo>::None` which hides the
/// chip entirely.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CiState {
    Success,
    Failure,
    Pending,
    Running,
    None,
}

/// PR number, URL, and folded CI state for the current branch.
///
/// Returned as `Option<PrCiInfo>` by `github_pr_ci`.  The outer `None` hides
/// the chip (no token / no remote / 401 / any error).  The inner
/// `ci_state = CiState::None` means "PR exists but no check-runs yet".
#[derive(Serialize)]
pub struct PrCiInfo {
    pub pr_number: Option<u32>,
    pub pr_url: Option<String>,
    pub ci_state: CiState,
}

// ─────────────────────────────────────────────────────────────────────────────
// PR/CI pure parser helpers — no network, no I/O, safe to unit-test
// ─────────────────────────────────────────────────────────────────────────────

/// Parse the first open PR from a GitHub `GET /pulls` JSON array.
///
/// Returns `(pr_number, html_url, head_sha)` for the first entry, or `None`
/// when the array is empty or required fields are absent.
pub(crate) fn parse_pulls(json: &str) -> Option<(u32, String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let pr = v.as_array()?.first()?;
    let number = pr["number"].as_u64()? as u32;
    let url = pr["html_url"].as_str()?.to_string();
    let sha = pr["head"]["sha"].as_str()?.to_string();
    Some((number, url, sha))
}

/// Fold a GitHub `GET /check-runs` JSON response into a single `CiState`.
///
/// Priority (highest wins): Failure > Running > Pending > Success > None.
///
/// - Failure conclusions: `failure`, `timed_out`, `action_required`, `cancelled`.
/// - Running: any run with `status = "in_progress"`.
/// - Pending: any run with `status = "queued"`.
/// - Success: all completed runs with `success`, `neutral`, `skipped`, or `stale`.
/// - None: empty `check_runs` array or unparseable response.
pub(crate) fn parse_check_runs(json: &str) -> CiState {
    let v = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) => v,
        Err(_) => return CiState::None,
    };
    let runs = match v["check_runs"].as_array() {
        Some(r) => r,
        None => return CiState::None,
    };
    if runs.is_empty() {
        return CiState::None;
    }

    let mut has_failure = false;
    let mut has_running = false;
    let mut has_pending = false;
    let mut has_success = false;

    for run in runs {
        let status = run["status"].as_str().unwrap_or("");
        let conclusion = run["conclusion"].as_str().unwrap_or("");

        match status {
            "in_progress" => has_running = true,
            "queued" => has_pending = true,
            "completed" => match conclusion {
                "failure" | "timed_out" | "action_required" | "cancelled" => has_failure = true,
                "success" | "neutral" | "skipped" | "stale" => has_success = true,
                _ => {}
            },
            _ => {}
        }
    }

    if has_failure {
        CiState::Failure
    } else if has_running {
        CiState::Running
    } else if has_pending {
        CiState::Pending
    } else if has_success {
        CiState::Success
    } else {
        CiState::None
    }
}

/// Parse a GitHub remote URL into `(owner, repo)`.
///
/// Handles SSH (`git@github.com:owner/repo[.git]`) and HTTPS/HTTP
/// (`https://github.com/owner/repo[.git]`).  Returns `None` for non-GitHub
/// remotes or unrecognized formats.
pub(crate) fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    // Strip optional .git suffix before splitting.
    let url = url.strip_suffix(".git").unwrap_or(url);

    // SSH: git@github.com:owner/repo
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let (owner, repo) = rest.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }

    // HTTPS or HTTP.
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let (owner, repo) = rest.split_once('/')?;
            return Some((owner.to_string(), repo.to_string()));
        }
    }

    None
}

// ─────────────────────────────────────────────────────────────────────────────
// PR/CI private helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Invoke `git -C {cwd} remote get-url origin` and return the trimmed URL,
/// or `None` on any error (git not found, not a git repo, no origin, etc.).
async fn get_remote_origin_url(cwd: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let trimmed = url.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

// ─────────────────────────────────────────────────────────────────────────────
// PR/CI Tauri command
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch the open PR and folded CI state for `branch` in the repo rooted at
/// `cwd`.
///
/// Returns `None` (chip hidden) on: no stored token / `cwd` is not a git repo
/// / no `origin` remote / non-GitHub remote / no open PR for this branch /
/// HTTP 401 / any network failure.  Never panics.
///
/// # How to get `cwd` and `branch`
/// - `branch` comes from `git_status(session_id)` → `GitInfoDto.branch`.
/// - `cwd` is NOT yet exposed by `SessionDto`.  Until a lightweight
///   `get_session_cwd` command is added (would require daemon changes), the
///   GUI can obtain it via `inspect_pid(pane_id)` → `env` → the `PWD` entry.
///   No daemon/proto changes are required for this command itself; the
///   architectural fork is flagged here for awareness.
///
/// # Security
/// The stored token is used only in `Authorization: Bearer` request headers
/// and is NEVER serialized, logged, or included in the returned value.
#[tauri::command]
pub async fn github_pr_ci(cwd: String, branch: String) -> Result<Option<PrCiInfo>, String> {
    // Guard: no token → chip hidden.
    let token = match load_token() {
        Some(t) => t,
        None => return Ok(None),
    };

    // Resolve owner/repo from the git remote origin URL.
    let remote_url = match get_remote_origin_url(&cwd).await {
        Some(u) => u,
        None => return Ok(None),
    };
    let (owner, repo) = match parse_owner_repo(&remote_url) {
        Some(p) => p,
        None => return Ok(None),
    };

    let client = match http_client() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    // Fetch open PRs for this branch.
    let pulls_url = format!(
        "https://api.github.com/repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open"
    );
    let pulls_resp = match client
        .get(&pulls_url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };

    if pulls_resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    if !pulls_resp.status().is_success() {
        return Ok(None);
    }

    let pulls_body = match pulls_resp.text().await {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    // No open PR for this branch → chip hidden.
    let (pr_number, pr_url, head_sha) = match parse_pulls(&pulls_body) {
        Some(t) => t,
        None => return Ok(None),
    };

    // Fetch CI check-runs for the PR head SHA.
    let checks_url = format!(
        "https://api.github.com/repos/{owner}/{repo}/commits/{head_sha}/check-runs"
    );
    let ci_state = match client
        .get(&checks_url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => parse_check_runs(&body),
            Err(_) => CiState::None,
        },
        _ => CiState::None,
    };

    // Token was used only in Authorization headers above — never logged or returned.
    Ok(Some(PrCiInfo {
        pr_number: Some(pr_number),
        pr_url: Some(pr_url),
        ci_state,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests — pure parser coverage + token file round-trip; no network
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_device_start ───────────────────────────────────────────────────

    #[test]
    fn parse_device_start_success() {
        let json = r#"{
            "device_code": "gho_device_abc123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let (ds, dc) = parse_device_start(json).expect("should parse");
        assert_eq!(ds.user_code, "ABCD-1234");
        assert_eq!(ds.verification_uri, "https://github.com/login/device");
        assert_eq!(ds.expires_in, 900);
        assert_eq!(ds.interval, 5);
        // device_code is captured but not in DeviceStart (never sent to GUI)
        assert_eq!(dc, "gho_device_abc123");
    }

    #[test]
    fn parse_device_start_missing_field_errors() {
        let json = r#"{"device_code":"x","user_code":"ABCD-1234"}"#;
        assert!(parse_device_start(json).is_err());
    }

    // ── parse_poll ───────────────────────────────────────────────────────────

    #[test]
    fn parse_poll_authorization_pending() {
        let json = r#"{"error":"authorization_pending","error_description":"The authorization request is still pending."}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::Pending));
    }

    #[test]
    fn parse_poll_slow_down_maps_to_slow_down() {
        let json = r#"{"error":"slow_down","error_description":"Too many polling requests."}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::SlowDown));
    }

    #[test]
    fn parse_poll_access_denied() {
        let json = r#"{"error":"access_denied","error_description":"The user has denied your application access."}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::Denied));
    }

    #[test]
    fn parse_poll_expired_token() {
        let json = r#"{"error":"expired_token","error_description":"The `device_code` has expired."}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::Expired));
    }

    #[test]
    fn parse_poll_success_returns_authorized_with_token() {
        let json = r#"{"access_token":"gho_abc123XYZ","token_type":"bearer","scope":"read:user"}"#;
        let result = parse_poll(json).expect("should parse");
        match result {
            PollResult::Authorized(token) => assert_eq!(token, "gho_abc123XYZ"),
            other => panic!("expected PollResult::Authorized, got {other:?}"),
        }
    }

    #[test]
    fn parse_poll_unknown_error_returns_err() {
        let json = r#"{"error":"unsupported_grant_type","error_description":"The grant type is not supported."}"#;
        let err = parse_poll(json).expect_err("unknown error should be Err");
        assert!(err.contains("The grant type is not supported."));
    }

    #[test]
    fn parse_poll_empty_access_token_not_authorized() {
        // GitHub shouldn't send this, but defensive: empty string is not a valid token.
        let json = r#"{"access_token":"","error":"authorization_pending"}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::Pending));
    }

    // ── parse_account ────────────────────────────────────────────────────────

    #[test]
    fn parse_account_with_name() {
        let json = r#"{
            "login": "octocat",
            "name": "The Octocat",
            "avatar_url": "https://github.com/images/error/octocat_happy.gif",
            "html_url": "https://github.com/octocat"
        }"#;
        let account = parse_account(json).expect("should parse");
        assert_eq!(account.login, "octocat");
        assert_eq!(account.name, Some("The Octocat".to_string()));
        assert_eq!(
            account.avatar_url,
            "https://github.com/images/error/octocat_happy.gif"
        );
        assert_eq!(account.html_url, "https://github.com/octocat");
    }

    #[test]
    fn parse_account_null_name_is_none() {
        let json = r#"{
            "login": "octocat",
            "name": null,
            "avatar_url": "https://avatars.githubusercontent.com/u/583231",
            "html_url": "https://github.com/octocat"
        }"#;
        let account = parse_account(json).expect("should parse");
        assert_eq!(account.name, None);
    }

    #[test]
    fn parse_account_missing_login_errors() {
        let json = r#"{"name":"x","avatar_url":"y","html_url":"z"}"#;
        assert!(parse_account(json).is_err());
    }

    // ── PR/CI: parse_pulls ───────────────────────────────────────────────────

    #[test]
    fn parse_pulls_first_pr_number_url_and_sha() {
        let json = r#"[{
            "number": 42,
            "html_url": "https://github.com/owner/repo/pull/42",
            "head": {
                "sha": "abc123def456789",
                "label": "owner:feature-branch"
            }
        }]"#;
        let (num, url, sha) = parse_pulls(json).expect("should parse first PR");
        assert_eq!(num, 42);
        assert_eq!(url, "https://github.com/owner/repo/pull/42");
        assert_eq!(sha, "abc123def456789");
    }

    #[test]
    fn parse_pulls_empty_array_returns_none() {
        assert!(parse_pulls("[]").is_none(), "empty array must return None");
    }

    #[test]
    fn parse_pulls_missing_head_sha_returns_none() {
        // head object is present but sha is absent — must return None.
        let json = r#"[{"number": 1, "html_url": "https://github.com/o/r/pull/1", "head": {}}]"#;
        assert!(parse_pulls(json).is_none());
    }

    // ── PR/CI: parse_check_runs ──────────────────────────────────────────────

    #[test]
    fn parse_check_runs_all_success_maps_to_success() {
        let json = r#"{
            "total_count": 2,
            "check_runs": [
                {"status": "completed", "conclusion": "success"},
                {"status": "completed", "conclusion": "neutral"}
            ]
        }"#;
        assert!(matches!(parse_check_runs(json), CiState::Success));
    }

    #[test]
    fn parse_check_runs_any_failure_maps_to_failure() {
        let json = r#"{
            "check_runs": [
                {"status": "completed", "conclusion": "success"},
                {"status": "completed", "conclusion": "failure"}
            ]
        }"#;
        assert!(matches!(parse_check_runs(json), CiState::Failure));
    }

    #[test]
    fn parse_check_runs_in_progress_maps_to_running() {
        let json = r#"{
            "check_runs": [
                {"status": "completed", "conclusion": "success"},
                {"status": "in_progress", "conclusion": null}
            ]
        }"#;
        assert!(matches!(parse_check_runs(json), CiState::Running));
    }

    #[test]
    fn parse_check_runs_queued_only_maps_to_pending() {
        let json = r#"{"check_runs": [{"status": "queued", "conclusion": null}]}"#;
        assert!(matches!(parse_check_runs(json), CiState::Pending));
    }

    #[test]
    fn parse_check_runs_empty_array_maps_to_none() {
        let json = r#"{"total_count": 0, "check_runs": []}"#;
        assert!(matches!(parse_check_runs(json), CiState::None));
    }

    #[test]
    fn parse_check_runs_failure_beats_running() {
        // Failure has higher priority than Running.
        let json = r#"{
            "check_runs": [
                {"status": "in_progress", "conclusion": null},
                {"status": "completed", "conclusion": "timed_out"}
            ]
        }"#;
        assert!(matches!(parse_check_runs(json), CiState::Failure));
    }

    // ── PR/CI: parse_owner_repo ──────────────────────────────────────────────

    #[test]
    fn parse_owner_repo_ssh_with_dot_git() {
        let (owner, repo) = parse_owner_repo("git@github.com:acme/myproject.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myproject");
    }

    #[test]
    fn parse_owner_repo_ssh_without_dot_git() {
        let (owner, repo) = parse_owner_repo("git@github.com:acme/myproject").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myproject");
    }

    #[test]
    fn parse_owner_repo_https_with_dot_git() {
        let (owner, repo) = parse_owner_repo("https://github.com/acme/myproject.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myproject");
    }

    #[test]
    fn parse_owner_repo_https_without_dot_git() {
        let (owner, repo) = parse_owner_repo("https://github.com/acme/myproject").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myproject");
    }

    #[test]
    fn parse_owner_repo_non_github_returns_none() {
        assert!(parse_owner_repo("https://gitlab.com/acme/repo.git").is_none());
    }

    // ── token file round-trip + 0600 mode ────────────────────────────────────

    #[test]
    fn token_roundtrip_and_mode_0600() {
        // Redirect the token path to a temp directory so the test is isolated
        // from the real ~/.config/pyre. XDG_CONFIG_HOME is set here; the other
        // tests in this module do not read home paths, so the conflict risk is low.
        let tmp = std::env::temp_dir()
            .join(format!("pyre-test-github-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: test-only env mutation; the other tests in this file don't
        // call token_path() so the race window is isolated to this test.
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let secret = "gho_roundtrip_test_ABCDEF123456";

        // Write the token.
        store_token(secret).expect("store_token should succeed");

        // Read it back — must round-trip exactly.
        let loaded = load_token().expect("load_token must return Some after store_token");
        assert_eq!(loaded, secret, "round-trip mismatch: stored != loaded");

        // On unix: assert the file was created with mode 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(token_path())
                .expect("token file must exist after store_token");
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "token file must be mode 0600 (owner read/write only)"
            );
        }

        // Clear — the file must disappear.
        clear_token();
        assert!(
            load_token().is_none(),
            "load_token must return None after clear_token"
        );

        // clear_token must be idempotent (NotFound must not panic).
        clear_token();

        // Restore env and clean up.
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
