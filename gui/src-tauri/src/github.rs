//! GitHub OAuth Device Flow — Tauri command module.
//!
//! Architecture:
//!   - All GitHub tokens go ONLY into the OS keychain (keyring crate).
//!     They never touch state.db, logs, stdout, or any plaintext file.
//!   - client_id is a compile-time default, overridable via PYRE_GITHUB_CLIENT_ID env.
//!     Device flow requires no client_secret — none is shipped.
//!   - In-flight device_code is held in GithubState (Tauri managed, per-app-instance Mutex).
//!   - Pure parser functions are factored out so tests never touch the network or keychain.

use serde::Serialize;
use tauri::State;
use tokio::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const DEFAULT_CLIENT_ID: &str = "Ov23li1g0XoYJex02nIG";
const GITHUB_SCOPE: &str = "read:user";
const KEYRING_SERVICE: &str = "pyre";
const KEYRING_ACCOUNT: &str = "github-token";

fn client_id() -> String {
    std::env::var("PYRE_GITHUB_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
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
// Internal poll result (carries the token so the command can write keychain)
// ─────────────────────────────────────────────────────────────────────────────

/// Internal parse result for `github_device_poll`. Carries the raw token
/// in the `Authorized` variant so the command (not the pure parser) writes
/// it to the keychain. Tests inspect this type without touching the keychain.
#[derive(Debug)]
pub(crate) enum PollResult {
    Pending,
    /// Contains the raw access token — stored by the command, not the parser.
    Authorized(String),
    Denied,
    Expired,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure parser functions — NO network, NO keychain, safe to unit-test
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
        "slow_down" => Ok(PollResult::Pending),
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
/// the token to the OS keychain and clears the in-flight code. Returns a
/// `PollState` discriminant — the token NEVER surfaces to the GUI.
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
            // Token goes ONLY into the OS keychain — never logged, never plaintext.
            // Tag keychain failures with KEYRING_FAILED: so the GUI treats them as terminal.
            keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
                .map_err(|e| format!("KEYRING_FAILED: couldn't open the OS keychain: {e}"))?
                .set_password(&token)
                .map_err(|e| format!("KEYRING_FAILED: couldn't save the token to the OS keychain: {e}"))?;
            Ok(PollState::Authorized)
        }
        PollResult::Pending => Ok(PollState::Pending),
        PollResult::Denied => Ok(PollState::Denied),
        PollResult::Expired => Ok(PollState::Expired),
    }
}

/// Fetch the linked GitHub account identity from the API.
///
/// Returns `None` when no token is stored or when the stored token is stale
/// (401 from the API → token deleted from keychain automatically).
/// Returns `Some(Account)` on success.
#[tauri::command]
pub async fn github_account() -> Result<Option<Account>, String> {
    // Retrieve token from keychain (released before the await below).
    let token = {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|e| format!("keyring init failed: {e}"))?;
        match entry.get_password() {
            Ok(t) => t,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(format!("keyring get failed: {e}")),
        }
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
        // Stale or revoked token — purge it from the keychain.
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
            let _ = entry.delete_credential();
        }
        return Ok(None);
    }

    if !status.is_success() {
        return Err(format!(
            "user request failed with HTTP {status}"
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("user read body failed: {e}"))?;

    Ok(Some(parse_account(&body)?))
}

/// Remove the stored GitHub token from the OS keychain.
///
/// Idempotent — treats "token not found" as success. Does NOT revoke the
/// grant on GitHub; the GUI should link to
/// github.com/settings/connections/applications/<client_id> for server-side
/// revocation.
#[tauri::command]
pub async fn github_disconnect() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|e| format!("keyring init failed: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keyring delete failed: {e}")),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests — pure parser coverage, no network, no keychain
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
    fn parse_poll_slow_down_maps_to_pending() {
        let json = r#"{"error":"slow_down","error_description":"Too many polling requests."}"#;
        let result = parse_poll(json).expect("should parse");
        assert!(matches!(result, PollResult::Pending));
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
}
