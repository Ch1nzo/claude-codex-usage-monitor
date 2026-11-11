use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AppUsageData, UsageData, UsageSection};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CREATE_NO_WINDOW: u32 = 0x08000000;

const MODEL_FALLBACK_CHAIN: &[&str] = &["claude-haiku-4-5"];

/// How long to wait for `claude --version` / `where.exe` style probes.
const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// CLI token refreshes are attempted on every poll until this many polls in a
/// row have failed to refresh, then at most once per `CLI_REFRESH_THROTTLE`.
const CLI_REFRESH_FREE_ATTEMPTS: u32 = 5;
const CLI_REFRESH_THROTTLE: Duration = Duration::from_secs(15 * 60);
/// How long after a reset time fast polling may still expect fresh data.
const RESET_FAST_POLL_WINDOW: Duration = Duration::from_secs(120);

/// (consecutive polls whose CLI refresh failed, time of the last such failure)
static CLI_REFRESH_BACKOFF: Mutex<(u32, Option<Instant>)> = Mutex::new((0, None));

#[derive(Debug)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
    /// Newer response format; elements are parsed one by one so an unknown
    /// or malformed entry doesn't fail the whole response.
    #[serde(default)]
    limits: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct UsageLimit {
    kind: String,
    #[serde(default)]
    scope: Option<serde_json::Value>,
    #[serde(alias = "utilization")]
    percent: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
}

pub fn poll(show_claude_code: bool, show_codex: bool) -> Result<AppUsageData, PollError> {
    let mut data = AppUsageData::default();

    if show_claude_code {
        data.claude_code = Some(poll_claude_code()?);
    }

    if show_codex {
        match poll_codex() {
            Ok(codex) => data.codex = Some(codex),
            Err(error) if !show_claude_code => return Err(error),
            Err(error) => diagnose::log(format!("Codex usage poll failed: {error:?}")),
        }
    }

    if data.claude_code.is_none() && data.codex.is_none() {
        Err(PollError::RequestFailed)
    } else {
        Ok(data)
    }
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            cli_refresh_codex_token();
            let refreshed = read_codex_credentials().ok_or(PollError::TokenExpired)?;
            fetch_codex_usage(&refreshed.access_token, refreshed.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    // One throttle decision and at most one recorded failure per poll, however
    // many expired sources it walks through.
    let may_refresh = {
        let (failures, last_failure) = *CLI_REFRESH_BACKOFF
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        refresh_allowed(
            failures,
            last_failure.map_or(Duration::MAX, |at| at.elapsed()),
        )
    };
    let mut attempted = false;

    let result = loop {
        if !is_token_expired(creds.expires_at) {
            break Ok(creds);
        }

        let source = creds.source.clone();
        if may_refresh {
            attempted = true;
            cli_refresh_token(&source);

            match read_credentials_from_source(&source) {
                Some(refreshed) if !is_token_expired(refreshed.expires_at) => {
                    reset_refresh_backoff();
                    return Ok(refreshed);
                }
                Some(_) => diagnose::log(format!(
                    "credentials from {source:?} still expired after refresh attempt"
                )),
                None => diagnose::log(format!(
                    "credentials from {source:?} unavailable after refresh attempt"
                )),
            }
        } else {
            diagnose::log(format!(
                "skipping CLI token refresh for {source:?}: throttled after repeated failures"
            ));
        }

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => break Err(PollError::TokenExpired),
        }
    };

    // A refresh ran but produced no fresh token (even if a later source was
    // fresh): count this poll as one failure.
    if attempted {
        let mut backoff = CLI_REFRESH_BACKOFF
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *backoff = (backoff.0.saturating_add(1), Some(Instant::now()));
    }
    result
}

/// Whether a CLI token refresh may run now, given the consecutive failed polls
/// so far and the time since the last failure. Keeps a logged-out machine from
/// launching `claude -p .` on every poll.
fn refresh_allowed(failures: u32, since_last: Duration) -> bool {
    failures < CLI_REFRESH_FREE_ATTEMPTS || since_last >= CLI_REFRESH_THROTTLE
}

/// Forget earlier refresh failures (on success, or when the user asks for a
/// manual refresh).
pub fn reset_refresh_backoff() {
    *CLI_REFRESH_BACKOFF
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = (0, None);
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

fn cli_refresh_codex_token() {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&codex_path).args(args);
        c
    } else if is_ps1 {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        c
    } else {
        let mut c = Command::new(&codex_path);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if run_with_timeout(
            Command::new(name)
                .arg("--version")
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
            CLI_PROBE_TIMEOUT,
        )
        .is_some()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Some(output) = run_with_timeout(
            Command::new("where.exe")
                .arg(name)
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null()),
            CLI_PROBE_TIMEOUT,
        ) {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if run_with_timeout(
            Command::new(name)
                .arg("--version")
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
            CLI_PROBE_TIMEOUT,
        )
        .is_some()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Some(output) = run_with_timeout(
            Command::new("where.exe")
                .arg(name)
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null()),
            CLI_PROBE_TIMEOUT,
        ) {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // Try the dedicated usage endpoint first
    match try_usage_endpoint(token)? {
        Some(data) => {
            // If reset timers are missing, fill them in from the Messages API —
            // but only for a window already in use: the request itself would
            // otherwise start a new 5-hour window.
            let needs_reset = |s: &UsageSection| s.resets_at.is_none() && s.percentage > 0.0;
            if needs_reset(&data.session) || needs_reset(&data.weekly) {
                if let Ok(fallback) = fetch_usage_via_messages(token) {
                    let mut merged = data;
                    if merged.session.resets_at.is_none() {
                        merged.session.resets_at = fallback.session.resets_at;
                    }
                    if merged.weekly.resets_at.is_none() {
                        merged.weekly.resets_at = fallback.weekly.resets_at;
                    }
                    return Ok(merged);
                }
            }
            return Ok(data);
        }
        None => {}
    }

    // Fall back to Messages API with rate limit headers
    let result = fetch_usage_via_messages(token);
    if result.is_err() {
        diagnose::log("usage endpoint and Messages API fallback both failed");
    }
    result
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => {
            diagnose::log(format!("usage endpoint returned status {}", resp.status()));
            resp
        }
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "usage endpoint returned auth error status {code}; re-login required"
            ));
            return Err(PollError::AuthRequired);
        }
        // Rate limited / server trouble: retry later rather than spending a
        // real Messages API request on every poll.
        Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
            diagnose::log(format!(
                "usage endpoint returned status {code}; retrying later"
            ));
            return Err(PollError::RequestFailed);
        }
        Err(ureq::Error::Status(code, _)) => {
            diagnose::log(format!(
                "usage endpoint returned status {code}; falling back to Messages API"
            ));
            return Ok(None);
        }
        Err(ureq::Error::Transport(error)) => {
            diagnose::log_error("usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse usage response", error);
            return Ok(None);
        }
    };
    let data = usage_from_response(response);
    if data.is_none() {
        diagnose::log("usage response had no recognizable limits; falling back to Messages API");
    }
    Ok(data)
}

/// Build usage data from the legacy `five_hour` / `seven_day` fields, falling
/// back to the unscoped `session` / `weekly_all` entries of `limits` for any
/// legacy field that is missing. None if neither window is present.
fn usage_from_response(response: UsageResponse) -> Option<UsageData> {
    let from_bucket = |bucket: UsageBucket| UsageSection {
        percentage: bucket.utilization,
        resets_at: parse_iso8601(bucket.resets_at.as_deref()),
    };
    let mut session = response.five_hour.map(from_bucket);
    let mut weekly = response.seven_day.map(from_bucket);

    for value in response.limits {
        let Ok(limit) = serde_json::from_value::<UsageLimit>(value) else {
            continue;
        };
        if limit.scope.is_some() {
            continue;
        }
        let slot = match limit.kind.as_str() {
            "session" => &mut session,
            "weekly_all" => &mut weekly,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(UsageSection {
                percentage: limit.percent,
                resets_at: parse_iso8601(limit.resets_at.as_deref()),
            });
        }
    }

    if session.is_none() && weekly.is_none() {
        return None;
    }
    Some(UsageData {
        session: session.unwrap_or_default(),
        weekly: weekly.unwrap_or_default(),
    })
}

fn fetch_usage_via_messages(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    for model in MODEL_FALLBACK_CHAIN {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                diagnose::log(format!(
                    "messages endpoint returned auth error status {code}; re-login required"
                ));
                return Err(PollError::AuthRequired);
            }
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> UsageData {
    let mut data = UsageData::default();

    data.session.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-5h-utilization") * 100.0;
    data.session.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-5h-reset",
    ));

    data.weekly.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-7d-utilization") * 100.0;
    data.weekly.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-7d-reset",
    ));

    let overall_reset = get_header_i64(response, "anthropic-ratelimit-unified-reset");

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        let status = response.header("anthropic-ratelimit-unified-status");
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = unix_to_system_time(overall_reset);
        }
    }

    data
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut data = UsageData::default();

    if let Some(window) = details.primary_window.flatten() {
        data.session = codex_section_from_window(&window);
    }

    if let Some(window) = details.secondary_window.flatten() {
        data.weekly = codex_section_from_window(&window);
    }

    Some(data)
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
    }
}

fn get_header_f64(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn get_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|s| s.parse::<i64>().ok())
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens.filter(|tokens| !tokens.access_token.is_empty())
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        // Docker Desktop's internal distros never hold Claude credentials;
        // probing them just starts them.
        .filter(|line| !line.is_empty() && !line.starts_with("docker-desktop"))
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
///
/// Handles `Z`, `+hh:mm` and `-hh:mm` (and the colon-less `±hhmm`) offsets, and
/// normalizes the result to UTC. The API currently emits `+00:00`, but handling
/// the general case keeps the reset time correct (and crash-free) for any valid
/// offset rather than silently dropping or mis-converting it.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?.trim();
    // Split "YYYY-MM-DDThh:mm:ss[.frac]" from any trailing timezone designator.
    let t_idx = s.find('T')?;
    let date = &s[..t_idx];
    let time_and_tz = &s[t_idx + 1..];
    let (time_part, offset_secs) = split_timezone(time_and_tz);

    let naive = format!("{date}T{time_part}");
    let wall_secs = parse_datetime_to_unix(&naive).ok()?;

    // `wall_secs` is the wall-clock time read as if UTC; subtract the offset to
    // get true UTC (e.g. 08:00+09:00 -> 23:00 the previous day in UTC).
    let utc = wall_secs as i64 - offset_secs;
    if utc < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(utc as u64))
}

/// Split a time portion `"hh:mm:ss[.frac][Z|±hh:mm]"` into the bare time string
/// and the offset in seconds (positive east of UTC). Returns offset 0 when none
/// is present.
fn split_timezone(time: &str) -> (&str, i64) {
    if let Some(stripped) = time.strip_suffix('Z').or_else(|| time.strip_suffix('z')) {
        return (stripped, 0);
    }
    // The time itself contains no '+'/'-', so the right-most one marks the
    // offset (e.g. "08:00:00.5-05:00" -> sign '-', "05:00").
    for (i, c) in time.char_indices().rev() {
        if c == '+' || c == '-' {
            let sign = if c == '-' { -1 } else { 1 };
            let off = parse_offset(&time[i + 1..]).unwrap_or(0);
            return (&time[..i], sign * off);
        }
    }
    (time, 0)
}

/// Parse an offset body like "05:00", "0500", or "05" into seconds.
fn parse_offset(off: &str) -> Option<i64> {
    let off = off.trim();
    let (h, m) = if let Some((h, m)) = off.split_once(':') {
        (h, m)
    } else if off.len() == 4 {
        (&off[..2], &off[2..])
    } else if off.len() == 2 {
        (off, "0")
    } else {
        return None;
    };
    let hours: i64 = h.parse().ok()?;
    let mins: i64 = m.parse().ok()?;
    Some(hours * 3600 + mins * 60)
}

/// Minimal datetime parser — avoids pulling in chrono/time crates. Expects a
/// timezone-free `"YYYY-MM-DDTHH:MM:SS[.frac]"` string.
fn parse_datetime_to_unix(s: &str) -> Result<u64, ()> {
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;
    // Range-check before using `month` to index and `day` in a subtraction —
    // malformed values would otherwise panic (index out of bounds / overflow).
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(());
    }

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text. `show_pct` / `show_cd`
/// independently include the percentage and the reset countdown.
pub fn format_line(
    section: &UsageSection,
    strings: Strings,
    show_pct: bool,
    show_cd: bool,
    detailed: bool,
) -> String {
    let pct = if show_pct {
        format!("{:.0}%", section.percentage)
    } else {
        String::new()
    };
    let cd = if show_cd {
        format_countdown(section.resets_at, strings, detailed)
    } else {
        String::new()
    };
    match (pct.is_empty(), cd.is_empty()) {
        (false, false) => format!("{pct} \u{00b7} {cd}"),
        (false, true) => pct,
        (true, false) => cd,
        (true, true) => String::new(),
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings, detailed: bool) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings, detailed)
}

/// Calculate how long until the display text would change. When `detailed` is
/// on, the value changes on the finer sub-unit boundary (minute for `Xh Ym`,
/// hour for `Xd Yh`).
pub fn time_until_display_change(resets_at: Option<SystemTime>, detailed: bool) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs(), detailed))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings, detailed: bool) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        if detailed {
            let rem_hours = (total_secs % 86400) / 3600;
            format!(
                "{total_days}{} {rem_hours}{}",
                strings.day_suffix, strings.hour_suffix
            )
        } else {
            format!("{total_days}{}", strings.day_suffix)
        }
    } else if total_hours >= 1 {
        if detailed {
            let rem_mins = (total_secs % 3600) / 60;
            format!(
                "{total_hours}{} {rem_mins}{}",
                strings.hour_suffix, strings.minute_suffix
            )
        } else {
            format!("{total_hours}{}", strings.hour_suffix)
        }
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn time_until_display_change_from_secs(total_secs: u64, detailed: bool) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        if detailed {
            total_hours * 3600 // "Xd Yh" changes every hour
        } else {
            total_days * 86400
        }
    } else if total_hours >= 1 {
        if detailed {
            total_mins * 60 // "Xh Ym" changes every minute
        } else {
            total_hours * 3600
        }
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section's reset time passed within the last
/// `RESET_FAST_POLL_WINDOW`. Older resets mean the server hasn't rolled the
/// window over, so fast polling for it would never end.
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| {
        s.resets_at
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|elapsed| elapsed <= RESET_FAST_POLL_WINDOW)
    };
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.claude_code.as_ref().is_some_and(is_past_reset)
        || data.codex.as_ref().is_some_and(is_past_reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(json: &str) -> Option<UsageData> {
        usage_from_response(serde_json::from_str(json).expect("test JSON parses"))
    }

    fn resets_at(offset_from_now: Duration, past: bool) -> UsageData {
        let now = SystemTime::now();
        let at = if past {
            now - offset_from_now
        } else {
            now + offset_from_now
        };
        UsageData {
            session: UsageSection {
                percentage: 0.0,
                resets_at: Some(at),
            },
            weekly: UsageSection::default(),
        }
    }

    #[test]
    fn usage_from_legacy_fields() {
        let data = usage(
            r#"{"five_hour":{"utilization":12.5,"resets_at":"2026-01-01T05:00:00+00:00"},
                "seven_day":{"utilization":40.0,"resets_at":null}}"#,
        )
        .expect("legacy fields are recognized");
        assert_eq!(data.session.percentage, 12.5);
        assert_eq!(
            data.session.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_767_243_600))
        );
        assert_eq!(data.weekly.percentage, 40.0);
        assert_eq!(data.weekly.resets_at, None);
    }

    #[test]
    fn usage_from_limits_array() {
        let data = usage(
            r#"{"limits":[
                {"kind":"session","scope":{"model":"opus"},"percent":99.0},
                {"kind":"session","scope":null,"percent":7.0,"resets_at":"2026-01-01T05:00:00Z"},
                {"kind":"weekly_all","percent":"broken"},
                {"kind":"weekly_all","utilization":33.0},
                {"kind":"something_else","percent":1.0}
            ]}"#,
        )
        .expect("limits are recognized");
        assert_eq!(data.session.percentage, 7.0);
        assert_eq!(
            data.session.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_767_243_600))
        );
        assert_eq!(data.weekly.percentage, 33.0);
        assert_eq!(data.weekly.resets_at, None);

        // Legacy fields win; limits only fill what is missing.
        let mixed = usage(
            r#"{"five_hour":{"utilization":10.0,"resets_at":null},
                "limits":[{"kind":"session","percent":50.0},{"kind":"weekly_all","percent":20.0}]}"#,
        )
        .expect("mixed response is recognized");
        assert_eq!(mixed.session.percentage, 10.0);
        assert_eq!(mixed.weekly.percentage, 20.0);
    }

    #[test]
    fn usage_from_response_without_limits_is_none() {
        assert!(usage("{}").is_none());
        assert!(usage(r#"{"five_hour":null,"seven_day":null,"limits":[]}"#).is_none());
        assert!(usage(r#"{"limits":[{"kind":"session","scope":"x","percent":5.0}]}"#).is_none());
    }

    #[test]
    fn is_past_reset_only_shortly_after_reset() {
        assert!(is_past_reset(&resets_at(Duration::from_secs(1), true)));
        assert!(is_past_reset(&resets_at(Duration::from_secs(115), true)));
        assert!(!is_past_reset(&resets_at(Duration::from_secs(125), true)));
        assert!(!is_past_reset(&resets_at(Duration::from_secs(60), false)));
        assert!(!is_past_reset(&UsageData::default()));
    }

    #[test]
    fn refresh_allowed_throttles_after_repeated_failures() {
        assert!(refresh_allowed(0, Duration::ZERO));
        assert!(refresh_allowed(4, Duration::ZERO));
        assert!(!refresh_allowed(5, Duration::from_secs(60)));
        assert!(!refresh_allowed(10, Duration::from_secs(14 * 60)));
        assert!(refresh_allowed(5, Duration::from_secs(15 * 60)));
        assert!(refresh_allowed(u32::MAX, Duration::MAX));
    }
}
