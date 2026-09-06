use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::StreamExt;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

// Where the provider's local OpenAI-compatible model runtime listens. The
// default matches an oMLX server; Ollama users point this at
// http://127.0.0.1:11434/v1/models, LM Studio at http://127.0.0.1:1234/v1/models.
// Only the scheme/host/port is used when proxying consumer requests; the path is
// used as-is for model discovery.
const DEFAULT_RUNTIME_URL: &str = "http://127.0.0.1:8001/v1/models/status";
// Local address the metering gateway binds to; cloudflared forwards tunnel
// traffic here. Must match `GATEWAY_SERVICE` in services/api/doodleiq_api/
// cloudflare.py. Not 8080 on purpose — that port collides with Tomcat, dev
// proxies, Jenkins, and most other "http-alt" software.
const GATEWAY_ADDRESS: &str = "127.0.0.1:47100";
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const PROVIDER_HEARTBEAT_INTERVAL_SECONDS: u64 = 30;
/// Backoff between heartbeat retry attempts within a single tick. Keeping the sum
/// well under the tick interval means a control-plane restart (a few seconds of
/// connection-refused) is absorbed without the marketplace ever seeing a gap.
const HEARTBEAT_RETRY_BACKOFF_SECONDS: [u64; 2] = [3, 7];
const DEFAULT_CONTROL_PLANE_URL: &str = "https://api.doodleiq.com";

/// Rolling view of whether heartbeats to the control plane are landing, surfaced
/// on the agent's local `/health` endpoint and in the terminal.
#[derive(Default)]
struct HeartbeatHealth {
    /// Unix seconds of the last successful heartbeat; 0 = none yet.
    last_ok_unix: AtomicU64,
    consecutive_failures: AtomicU32,
    /// Monotonic heartbeat sequence number sent to the control plane.
    seq: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl HeartbeatHealth {
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_success(&self) {
        self.last_ok_unix.store(unix_timestamp(), Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
    }

    fn record_failure(&self, error: &str) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(error.to_string());
        }
    }

    fn snapshot(&self) -> Value {
        let last_ok = self.last_ok_unix.load(Ordering::Relaxed);
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        json!({
            "ok": failures == 0,
            "consecutive_failures": failures,
            "last_ok_unix": (last_ok != 0).then_some(last_ok),
            "seconds_since_last_ok": (last_ok != 0)
                .then(|| unix_timestamp().saturating_sub(last_ok)),
            "last_error": self.last_error.lock().ok().and_then(|slot| slot.clone()),
        })
    }
}

/// Whether a failed heartbeat is worth retrying inside the same tick. Transport
/// errors and 5xx/429 are transient; a 4xx means the request itself is rejected
/// and will fail identically on the next attempt.
fn is_retryable_heartbeat_error(error: &str) -> bool {
    error.contains("Unable to update provider availability")
        || error.contains("returned 5")
        || error.contains("returned 429")
}

fn control_plane_url() -> String {
    std::env::var("DOODLEIQ_CONTROL_PLANE_URL")
        .unwrap_or_else(|_| DEFAULT_CONTROL_PLANE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn provider_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|err| format!("Unable to initialize the HTTP client: {err}"))
}

fn load_environment_files() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Existing process variables have highest priority. For file-based values,
    // .env.local takes precedence over the shared .env file.
    let _ = dotenvy::from_path(directory.join(".env.local"));
    let _ = dotenvy::from_path(directory.join(".env"));
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct RuntimeConfig {
    url: String,
    api_key: String,
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    pairing_id: String,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    pairing_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairingStart {
    id: String,
    code: String,
    pairing_token: String,
    verification_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairingPoll {
    status: String,
    user_id: Option<String>,
    device_id: Option<String>,
    hostname: Option<String>,
    tunnel_token: Option<String>,
    tunnel_ready: bool,
}

#[derive(Debug, Serialize)]
struct ProviderIdentity {
    provider_id: String,
    device_id: String,
    hostname: String,
}

#[derive(Debug, Clone, Serialize)]
struct MachineIdentity {
    name: String,
    model: String,
    location: String,
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Best-effort human labels for this machine, cached for the process lifetime —
/// probing them can spawn a subprocess and the heartbeat asks every 30s.
fn machine_identity() -> MachineIdentity {
    static CACHE: std::sync::OnceLock<MachineIdentity> = std::sync::OnceLock::new();
    CACHE.get_or_init(compute_machine_identity).clone()
}

fn compute_machine_identity() -> MachineIdentity {
    MachineIdentity {
        name: machine_name(),
        model: machine_model(),
        location: machine_location(),
    }
}

fn machine_name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(value) = command_output("scutil", &["--get", "ComputerName"]) {
        return value;
    }
    #[cfg(windows)]
    if let Ok(value) = std::env::var("COMPUTERNAME") {
        if !value.trim().is_empty() {
            return value;
        }
    }
    command_output("hostname", &[])
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "DoodleIQ provider".to_string())
}

fn machine_model() -> String {
    #[cfg(target_os = "macos")]
    if let Some(value) = command_output("sysctl", &["-n", "hw.model"]) {
        return value;
    }
    #[cfg(target_os = "linux")]
    if let Ok(value) = fs::read_to_string("/sys/devices/virtual/dmi/id/product_name") {
        let value = value.trim();
        if !value.is_empty() && value != "System Product Name" {
            return value.to_string();
        }
    }
    #[cfg(windows)]
    if let Some(value) = command_output(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).Model",
        ],
    ) {
        return value;
    }
    std::env::consts::ARCH.to_string()
}

fn machine_location() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim();
        if !tz.is_empty() {
            return tz.to_string();
        }
    }
    // Reads the OS timezone directly on every platform (`/etc/localtime` on
    // Linux, `CFTimeZone` on macOS, the registry on Windows) — no subprocess.
    iana_time_zone::get_timezone().unwrap_or_else(|_| "Location unavailable".to_string())
}

#[derive(Debug, Deserialize)]
struct GrantValidation {
    model_id: String,
    max_tokens: u64,
    expires_at: String,
    expires_at_epoch: u64,
    started_at_epoch: u64,
}

#[derive(Debug, Serialize, Clone)]
struct UsageSnapshot {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    consumer_connected: bool,
    consumer_connected_seconds: u64,
    last_consumer_activity: u64,
    tokens_per_second: f64,
}

#[derive(Default)]
struct UsageMeter {
    requests: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    total_tokens: AtomicU64,
    last_consumer_activity: AtomicU64,
    consumer_expires_at: AtomicU64,
    consumer_connected_at: AtomicU64,
    active_grant_id: Mutex<Option<String>>,
    duration_ms: AtomicU64,
}

impl UsageMeter {
    fn reset_counters(&self) {
        self.requests.store(0, Ordering::Relaxed);
        self.input_tokens.store(0, Ordering::Relaxed);
        self.output_tokens.store(0, Ordering::Relaxed);
        self.total_tokens.store(0, Ordering::Relaxed);
        self.duration_ms.store(0, Ordering::Relaxed);
    }

    fn record(&self, usage: TokenUsage, duration_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.input_tokens
            .fetch_add(usage.input_tokens, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(usage.output_tokens, Ordering::Relaxed);
        self.total_tokens
            .fetch_add(usage.total_tokens, Ordering::Relaxed);
        self.duration_ms.fetch_add(duration_ms, Ordering::Relaxed);
        log::info!(
            "metered request input_tokens={} output_tokens={} total_tokens={}",
            usage.input_tokens,
            usage.output_tokens,
            usage.total_tokens
        );
    }

    fn snapshot(&self) -> UsageSnapshot {
        let last_consumer_activity = self.last_consumer_activity.load(Ordering::Relaxed);
        let consumer_expires_at = self.consumer_expires_at.load(Ordering::Relaxed);
        let now = unix_timestamp();
        let output_tokens = self.output_tokens.load(Ordering::Relaxed);
        let duration_ms = self.duration_ms.load(Ordering::Relaxed);
        UsageSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            input_tokens: self.input_tokens.load(Ordering::Relaxed),
            output_tokens,
            total_tokens: self.total_tokens.load(Ordering::Relaxed),
            consumer_connected: consumer_expires_at > now,
            consumer_connected_seconds: now
                .saturating_sub(self.consumer_connected_at.load(Ordering::Relaxed)),
            last_consumer_activity,
            tokens_per_second: if duration_ms > 0 {
                output_tokens as f64 * 1000.0 / duration_ms as f64
            } else {
                0.0
            },
        }
    }

    fn note_consumer_activity(&self, grant_id: &str, expires_at: u64, started_at: u64) {
        if let Ok(mut active_grant_id) = self.active_grant_id.lock() {
            if active_grant_id.as_deref() != Some(grant_id) {
                self.reset_counters();
            }
            *active_grant_id = Some(grant_id.to_string());
        }
        self.last_consumer_activity
            .store(unix_timestamp(), Ordering::Relaxed);
        self.consumer_expires_at
            .store(expires_at, Ordering::Relaxed);
        self.consumer_connected_at
            .store(started_at, Ordering::Relaxed);
    }

    fn disconnect(&self, grant_id: &str) -> bool {
        let Ok(mut active_grant_id) = self.active_grant_id.lock() else {
            return false;
        };
        if active_grant_id.as_deref() != Some(grant_id) {
            return false;
        }
        *active_grant_id = None;
        self.last_consumer_activity.store(0, Ordering::Relaxed);
        self.consumer_expires_at.store(0, Ordering::Relaxed);
        self.consumer_connected_at.store(0, Ordering::Relaxed);
        true
    }

    fn is_connected(&self, grant_id: &str) -> bool {
        let Ok(active_grant_id) = self.active_grant_id.lock() else {
            return false;
        };
        active_grant_id.as_deref() == Some(grant_id)
    }
}

#[derive(Debug, Deserialize)]
struct SessionDisconnect {
    grant_id: String,
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone)]
struct GatewayState {
    config: Arc<RwLock<RuntimeConfig>>,
    meter: Arc<UsageMeter>,
    client: reqwest::Client,
    /// HTTP client used exclusively to proxy chat/completions streams to the
    /// local model runtime. Unlike `client`, this one has NO total-request
    /// timeout, so a streaming response that generates tokens for minutes is
    /// not cut off mid-stream. (A global timeout on the shared client would
    /// otherwise abort long streaming generations on the consumer's side.)
    upstream: reqwest::Client,
    /// Rolling heartbeat delivery health, shared with the `/health` handler.
    heartbeat_health: Arc<HeartbeatHealth>,
    /// Ed25519 device key, loaded once at startup. Every usage receipt is
    /// signed with it, so it must not be re-read from disk per request.
    signing_key: Arc<SigningKey>,
}

#[derive(Clone, Default)]
struct CloudflaredState {
    /// A blocking `std::sync::Mutex` on purpose. It guards a plain process
    /// handle, not an async IO resource, and is only touched from synchronous
    /// functions (`launch_cloudflared`, `stop_cloudflared`, `cloudflared_running`)
    /// for non-blocking calls — `try_wait`, `kill`, an assignment. The guard is
    /// never held across an `.await`. Per Tokio's own guidance, a blocking mutex
    /// is the right primitive here; `tokio::sync::Mutex` would only add overhead.
    process: Arc<Mutex<Option<Child>>>,
}

#[derive(Clone)]
struct HeartbeatTarget {
    model_id: String,
}

/// Number of consecutive "no loaded model" observations before the provider is
/// allowed to report itself offline. A runtime can briefly report an empty model
/// set while it swaps or reloads a checkpoint; publishing `available=false` on the
/// first sighting flaps the marketplace listing and prematurely closes grants.
/// Debouncing prevents that flapping while still going offline for a genuine,
/// sustained unload.
const PROVIDER_MODEL_GONE_DEBOUNCE: u32 = 4;

#[derive(Clone)]
struct ProviderHeartbeatState {
    target: Arc<RwLock<Option<HeartbeatTarget>>>,
    model_gone_streak: Arc<std::sync::atomic::AtomicU32>,
}

impl Default for ProviderHeartbeatState {
    fn default() -> Self {
        Self {
            target: Arc::new(RwLock::new(None)),
            model_gone_streak: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

#[derive(Debug, Serialize)]
struct CloudflaredStatus {
    installed: bool,
    running: bool,
    version: Option<String>,
    path: Option<String>,
}

const CLOUDFLARED_BINARY: &str = if cfg!(windows) {
    "cloudflared.exe"
} else {
    "cloudflared"
};

/// Where `ensure_cloudflared` caches an auto-downloaded connector.
fn cloudflared_download_path() -> Option<PathBuf> {
    Some(config_path().ok()?.with_file_name(CLOUDFLARED_BINARY))
}

fn cloudflared_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join(CLOUDFLARED_BINARY));
        }
    }
    if let Some(downloaded) = cloudflared_download_path() {
        candidates.push(downloaded);
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin/cloudflared"));
        candidates.push(PathBuf::from("/usr/local/bin/cloudflared"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| {
            Command::new("cloudflared")
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| PathBuf::from("cloudflared"))
        })
}

/// Cloudflare's release asset for this OS/arch, or `None` when we can't fetch a
/// single-file binary (macOS ships a tarball — use Homebrew there).
fn cloudflared_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("cloudflared-linux-amd64"),
        ("linux", "aarch64") => Some("cloudflared-linux-arm64"),
        ("linux", "arm") => Some("cloudflared-linux-arm"),
        ("linux", "x86") => Some("cloudflared-linux-386"),
        ("windows", "x86_64") => Some("cloudflared-windows-amd64.exe"),
        ("windows", "x86") => Some("cloudflared-windows-386.exe"),
        _ => None,
    }
}

/// Make sure a `cloudflared` binary is available, downloading Cloudflare's
/// official release into the config directory when it is missing. macOS falls
/// back to a `brew install cloudflared` instruction.
async fn ensure_cloudflared(client: &reqwest::Client) -> Result<(), String> {
    if cloudflared_path().is_some() {
        return Ok(());
    }
    let asset = cloudflared_asset().ok_or_else(|| {
        "cloudflared was not found; install it (macOS: `brew install cloudflared`)".to_string()
    })?;
    let target = cloudflared_download_path()
        .ok_or_else(|| "unable to determine a cloudflared install path".to_string())?;
    if let Some(directory) = target.parent() {
        fs::create_dir_all(directory)
            .map_err(|err| format!("unable to create the cloudflared directory: {err}"))?;
    }
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    log::info!("downloading cloudflared from {url}");
    let bytes = client
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("unable to download cloudflared: {err}"))?
        .bytes()
        .await
        .map_err(|err| format!("unable to read the cloudflared download: {err}"))?;
    let temp = target.with_extension("download");
    fs::write(&temp, &bytes).map_err(|err| format!("unable to save cloudflared: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("unable to mark cloudflared executable: {err}"))?;
    }
    fs::rename(&temp, &target).map_err(|err| format!("unable to install cloudflared: {err}"))?;
    log::info!("installed cloudflared at {}", target.display());
    Ok(())
}

fn cloudflared_status(state: &CloudflaredState) -> CloudflaredStatus {
    let path = cloudflared_path();
    let version = path.as_ref().and_then(|binary| {
        Command::new(binary)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    });
    let running = cloudflared_running(state);
    CloudflaredStatus {
        installed: path.is_some(),
        running,
        version,
        path: path.map(|value| value.display().to_string()),
    }
}

fn cloudflared_running(state: &CloudflaredState) -> bool {
    state
        .process
        .lock()
        .ok()
        .and_then(|mut process| {
            process
                .as_mut()
                .map(|child| child.try_wait().ok().flatten().is_none())
        })
        .unwrap_or(false)
}

/// Kill the `cloudflared` connector a previous run of this agent launched but
/// did not clean up (e.g. after a crash). Uses the recorded PID file, so it
/// touches only our own orphan — never a user's other tunnels or a
/// system-installed `cloudflared` service. Leaving an orphan running would let
/// two connectors compete for the same public hostname, which surfaces as
/// intermittent network errors for consumers.
fn kill_stale_managed_cloudflared() {
    let Some(path) = cloudflared_pid_path() else { return };
    let Ok(contents) = fs::read_to_string(&path) else { return };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        let _ = fs::remove_file(&path);
        return;
    };
    if pid_is_cloudflared(pid) {
        #[cfg(unix)]
        let _ = Command::new("kill").arg(pid.to_string()).status();
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }
    let _ = fs::remove_file(&path);
}

fn launch_cloudflared(tunnel_token: &str, state: &CloudflaredState) -> Result<(), String> {
    if tunnel_token.trim().is_empty() {
        return Err("A Cloudflare tunnel token is required".to_string());
    }
    let binary = cloudflared_path().ok_or_else(|| {
        "cloudflared was not found; install it (macOS: `brew install cloudflared`)".to_string()
    })?;
    kill_stale_managed_cloudflared();
    let mut process = state
        .process
        .lock()
        .map_err(|_| "Unable to access the tunnel process".to_string())?;
    if let Some(child) = process.as_mut() {
        if child.try_wait().map_err(|err| err.to_string())?.is_none() {
            return Ok(());
        }
    }
    let child = Command::new(binary)
        .args([
            "tunnel",
            "--no-autoupdate",
            "run",
            "--token",
            tunnel_token.trim(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("Unable to start cloudflared: {err}"))?;
    record_cloudflared_pid(child.id());
    *process = Some(child);
    drop(process);
    std::thread::sleep(std::time::Duration::from_millis(750));
    if !cloudflared_running(state) {
        return Err("cloudflared exited before establishing the managed tunnel".to_string());
    }
    Ok(())
}

fn stop_cloudflared(state: &CloudflaredState) -> Result<(), String> {
    let mut process = state
        .process
        .lock()
        .map_err(|_| "Unable to access the tunnel process".to_string())?;
    if let Some(mut child) = process.take() {
        child
            .kill()
            .map_err(|err| format!("Unable to stop cloudflared: {err}"))?;
        let _ = child.wait();
    }
    clear_cloudflared_pid();
    Ok(())
}

fn config_path() -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("DOODLEIQ_CONFIG_DIR") {
        return Ok(PathBuf::from(directory).join("config.json"));
    }
    #[cfg(target_os = "windows")]
    if let Some(directory) = std::env::var_os("APPDATA") {
        return Ok(PathBuf::from(directory)
            .join("DoodleIQ")
            .join("config.json"));
    }
    if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(directory)
            .join("doodleiq")
            .join("config.json"));
    }
    let home = std::env::var("HOME")
        .map_err(|err| format!("Unable to locate the config directory: {err}"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("doodleiq")
        .join("config.json"))
}

fn device_key_path() -> Result<PathBuf, String> {
    Ok(config_path()?.with_file_name("device.key"))
}

/// Records the PID of the `cloudflared` connector this agent launched, so a
/// later run (including one after a crash) can clean up the exact orphan
/// rather than pattern-matching the process table.
fn cloudflared_pid_path() -> Option<PathBuf> {
    Some(config_path().ok()?.with_file_name("cloudflared.pid"))
}

/// True if a process with this PID is currently a running `cloudflared`.
/// Guards against PID reuse before we send a kill.
fn pid_is_cloudflared(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("cloudflared"))
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("cloudflared.exe"))
            .unwrap_or(false)
    }
}

fn record_cloudflared_pid(pid: u32) {
    if let Some(path) = cloudflared_pid_path() {
        let _ = fs::write(path, pid.to_string());
    }
}

fn clear_cloudflared_pid() {
    if let Some(path) = cloudflared_pid_path() {
        let _ = fs::remove_file(path);
    }
}

fn log_file_path() -> Result<PathBuf, String> {
    let config_file = config_path()?;
    let directory = config_file
        .parent()
        .ok_or_else(|| "Unable to determine config directory".to_string())?;
    Ok(directory.join("doodleiq.log"))
}

fn device_signing_key() -> Result<SigningKey, String> {
    let path = device_key_path()?;
    if path.exists() {
        let encoded = fs::read_to_string(&path)
            .map_err(|err| format!("Unable to read device identity: {err}"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .map_err(|_| "Device identity is invalid".to_string())?;
        let secret: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "Device identity has an invalid length".to_string())?;
        return Ok(SigningKey::from_bytes(&secret));
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)
            .map_err(|err| format!("Unable to create identity directory: {err}"))?;
    }
    let key = SigningKey::generate(&mut OsRng);
    fs::write(&path, URL_SAFE_NO_PAD.encode(key.to_bytes()))
        .map_err(|err| format!("Unable to save device identity: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("Unable to protect device identity: {err}"))?;
    }
    Ok(key)
}

fn legacy_config_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    vec![
        home.join("doodleiq").join("config.json"),
        home.join("blindfinder").join("config.json"),
    ]
}

fn load_config() -> Result<RuntimeConfig, String> {
    let current_file = config_path()?;
    let file = if current_file.exists() || std::env::var_os("DOODLEIQ_CONFIG_DIR").is_some() {
        current_file.clone()
    } else {
        legacy_config_paths()
            .into_iter()
            .find(|path| path.exists())
            .unwrap_or_else(|| current_file.clone())
    };
    if !file.exists() {
        return Ok(RuntimeConfig {
            url: DEFAULT_RUNTIME_URL.to_string(),
            api_key: String::new(),
            provider_id: String::new(),
            pairing_id: String::new(),
            device_id: String::new(),
            pairing_token: String::new(),
        });
    }
    let raw =
        fs::read_to_string(&file).map_err(|err| format!("Unable to read config file: {err}"))?;
    if file != current_file {
        if let Some(dir) = current_file.parent() {
            fs::create_dir_all(dir)
                .map_err(|err| format!("Unable to create DoodleIQ config directory: {err}"))?;
        }
        fs::write(&current_file, &raw)
            .map_err(|err| format!("Unable to migrate DoodleIQ config: {err}"))?;
    }
    let mut config: RuntimeConfig = serde_json::from_str(&raw).unwrap_or_default();
    if config.url.trim().is_empty() {
        config.url = DEFAULT_RUNTIME_URL.to_string();
    }
    Ok(config)
}

fn persist_config(config: &RuntimeConfig) -> Result<(), String> {
    let file = config_path()?;
    let directory = file
        .parent()
        .ok_or_else(|| "Unable to determine config directory".to_string())?;
    fs::create_dir_all(directory)
        .map_err(|err| format!("Unable to create config directory: {err}"))?;
    let raw = serde_json::to_string_pretty(config)
        .map_err(|err| format!("Unable to serialize config: {err}"))?;
    fs::write(&file, raw).map_err(|err| format!("Unable to save config file: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("Unable to protect config file: {err}"))?;
    }
    Ok(())
}

async fn save_runtime_config(
    url: String,
    api_key: Option<String>,
    state: &GatewayState,
) -> Result<(), String> {
    let saved = state.config.read().await.clone();
    let config = RuntimeConfig {
        url: if url.trim().is_empty() {
            DEFAULT_RUNTIME_URL.to_string()
        } else {
            url
        },
        // Keep the previously saved key when --api-key is omitted, so re-running
        // `configure` just to change the URL does not wipe it.
        api_key: api_key.unwrap_or(saved.api_key),
        provider_id: saved.provider_id,
        pairing_id: saved.pairing_id,
        device_id: saved.device_id,
        pairing_token: saved.pairing_token,
    };
    persist_config(&config)?;
    *state.config.write().await = config;
    Ok(())
}

/// Pick the model id the runtime is currently serving from its model-list
/// response. Handles the common shapes: a bare array, `{"models": [...]}`, or the
/// OpenAI `{"data": [...]}` list. When any entry carries a `loaded` flag (oMLX,
/// vLLM) the loaded one wins; when no entry reports load state (Ollama's
/// `/v1/models`, LM Studio) the first listed model is used.
fn loaded_model_id(payload: &Value) -> Option<String> {
    let models = payload
        .as_array()
        .or_else(|| payload.get("models").and_then(Value::as_array))
        .or_else(|| payload.get("data").and_then(Value::as_array))?;
    let id_of = |model: &Value| {
        model
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
    };
    let reports_load_state = models.iter().any(|model| model.get("loaded").is_some());
    if reports_load_state {
        models.iter().find_map(|model| {
            (model.get("loaded").and_then(Value::as_bool) == Some(true))
                .then(|| id_of(model))
                .flatten()
        })
    } else {
        models.iter().find_map(id_of)
    }
}

async fn discover_loaded_model(
    config: &RuntimeConfig,
    client: &reqwest::Client,
) -> Result<String, String> {
    let mut request = client.get(&config.url);
    if !config.api_key.trim().is_empty() {
        request = request.header(header::AUTHORIZATION, format!("Bearer {}", config.api_key));
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("unable to reach the model runtime: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("model runtime returned {}", response.status()));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|err| format!("invalid model runtime response: {err}"))?;
    loaded_model_id(&payload).ok_or_else(|| "the model runtime has no loaded model".to_string())
}

async fn begin_provider_pairing() -> Result<PairingStart, String> {
    let key = device_signing_key()?;
    let machine = machine_identity();
    let response = provider_http_client()?
        .post(format!("{}/v1/device-pairings", control_plane_url()))
        .json(&json!({
            "device_name": machine.name,
            "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
        }))
        .send()
        .await
        .map_err(|err| format!("Unable to start device pairing: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("Pairing service returned {}", response.status()));
    }
    let pairing: PairingStart = response
        .json()
        .await
        .map_err(|err| format!("Invalid pairing response: {err}"))?;
    Ok(pairing)
}

async fn poll_provider_pairing(
    pairing_id: String,
    pairing_token: String,
    state: &GatewayState,
) -> Result<Option<ProviderIdentity>, String> {
    let response = state
        .client
        .get(format!(
            "{}/v1/device-pairings/{pairing_id}",
            control_plane_url()
        ))
        .header("X-DoodleIQ-Pairing-Token", &pairing_token)
        .send()
        .await
        .map_err(|err| format!("Unable to check pairing: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("Pairing service returned {}", response.status()));
    }
    let pairing: PairingPoll = response
        .json()
        .await
        .map_err(|err| format!("Invalid pairing response: {err}"))?;
    if pairing.status != "complete" {
        return Ok(None);
    }
    let provider_id = pairing
        .user_id
        .ok_or_else(|| "Pairing has no user ID".to_string())?;
    let device_id = pairing
        .device_id
        .ok_or_else(|| "Pairing has no device ID".to_string())?;
    let hostname = pairing
        .hostname
        .ok_or_else(|| "Pairing has no hostname".to_string())?;
    if !pairing.tunnel_ready {
        log::warn!("Cloudflare provisioning is not configured; provider tunnel was not started");
    }
    let mut config = state.config.write().await;
    config.provider_id = provider_id.clone();
    config.pairing_id = pairing_id;
    config.device_id = device_id.clone();
    config.pairing_token = pairing_token;
    persist_config(&config)?;
    Ok(Some(ProviderIdentity {
        provider_id,
        device_id,
        hostname,
    }))
}

/// Make sure the managed Cloudflare connector is up, relaunching it from the
/// control plane when it has died. Returns true only once a connector is running
/// so callers know publishing an `available=true` heartbeat is safe.
async fn ensure_tunnel_healthy(state: &GatewayState, cloudflared_state: &CloudflaredState) -> bool {
    if cloudflared_running(cloudflared_state) {
        return true;
    }
    log::warn!("managed Cloudflare tunnel is not running; attempting to relaunch");
    match resume_provider_tunnel(state, cloudflared_state).await {
        Ok(true) => {
            log::info!("managed Cloudflare tunnel relaunched");
            true
        }
        Ok(false) => {
            log::warn!("control plane offered no tunnel token; staying offline");
            false
        }
        Err(err) => {
            log::warn!("unable to relaunch managed Cloudflare tunnel: {err}");
            false
        }
    }
}

async fn resume_provider_tunnel(
    state: &GatewayState,
    cloudflared_state: &CloudflaredState,
) -> Result<bool, String> {
    if cloudflared_running(cloudflared_state) {
        return Ok(true);
    }
    let config = state.config.read().await.clone();
    if config.pairing_id.is_empty() || config.pairing_token.is_empty() {
        return Ok(false);
    }
    let response = state
        .client
        .get(format!(
            "{}/v1/device-pairings/{}",
            control_plane_url(),
            config.pairing_id
        ))
        .header("X-DoodleIQ-Pairing-Token", &config.pairing_token)
        .send()
        .await
        .map_err(|err| format!("Unable to restore provider tunnel: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("Pairing service returned {}", response.status()));
    }
    let pairing: PairingPoll = response
        .json()
        .await
        .map_err(|err| format!("Invalid pairing response: {err}"))?;
    let Some(tunnel_token) = pairing.tunnel_token else {
        return Ok(false);
    };
    ensure_cloudflared(&state.client).await?;
    launch_cloudflared(&tunnel_token, cloudflared_state)?;
    Ok(true)
}

async fn post_provider_heartbeat_once(
    model_id: &str,
    available: bool,
    model_loaded: bool,
    reason: Option<&str>,
    state: &GatewayState,
    cloudflared_state: &CloudflaredState,
) -> Result<String, String> {
    let config = state.config.read().await.clone();
    if config.device_id.is_empty() || config.pairing_token.is_empty() {
        return Err("This desktop is not paired with a provider account".to_string());
    }
    if available && !cloudflared_running(cloudflared_state) {
        return Err("The managed Cloudflare tunnel is not running".to_string());
    }
    let machine = machine_identity();
    let tunnel_healthy = cloudflared_running(cloudflared_state);
    let seq = state.heartbeat_health.next_seq();
    let response = state
        .client
        .post(format!(
            "{}/v1/devices/{}/heartbeat",
            control_plane_url(),
            config.device_id
        ))
        .header("X-DoodleIQ-Pairing-Token", &config.pairing_token)
        .json(&json!({
            "model_id": model_id,
            "available": available,
            "model_loaded": model_loaded,
            "tunnel_healthy": tunnel_healthy,
            "seq": seq,
            "agent_version": env!("CARGO_PKG_VERSION"),
            "reason": reason,
            "machine_name": machine.name,
            "machine_model": machine.model,
            "machine_location": machine.location,
        }))
        .send()
        .await
        .map_err(|err| format!("Unable to update provider availability: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("Heartbeat service returned {}", response.status()));
    }
    Ok(format!("{}.doodleiq.com", config.provider_id))
}

/// Send a heartbeat, retrying transient failures within the tick so a brief
/// control-plane restart never surfaces as an offline provider. Records the
/// outcome in `state.heartbeat_health`.
async fn post_provider_heartbeat(
    model_id: &str,
    available: bool,
    model_loaded: bool,
    reason: Option<&str>,
    state: &GatewayState,
    cloudflared_state: &CloudflaredState,
) -> Result<String, String> {
    let mut attempt = 0usize;
    loop {
        match post_provider_heartbeat_once(
            model_id,
            available,
            model_loaded,
            reason,
            state,
            cloudflared_state,
        )
        .await
        {
            Ok(hostname) => {
                state.heartbeat_health.record_success();
                return Ok(hostname);
            }
            Err(err) => {
                if is_retryable_heartbeat_error(&err)
                    && attempt < HEARTBEAT_RETRY_BACKOFF_SECONDS.len()
                {
                    let delay = HEARTBEAT_RETRY_BACKOFF_SECONDS[attempt];
                    attempt += 1;
                    log::warn!("heartbeat attempt {attempt} failed ({err}); retrying in {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    continue;
                }
                state.heartbeat_health.record_failure(&err);
                return Err(err);
            }
        }
    }
}

async fn send_provider_heartbeat(
    model_id: String,
    available: bool,
    state: &GatewayState,
    cloudflared_state: &CloudflaredState,
    heartbeat_state: &ProviderHeartbeatState,
) -> Result<String, String> {
    if available {
        *heartbeat_state.target.write().await = Some(HeartbeatTarget {
            model_id: model_id.clone(),
        });
    } else {
        *heartbeat_state.target.write().await = None;
    }
    post_provider_heartbeat(
        &model_id,
        available,
        available,
        None,
        state,
        cloudflared_state,
    )
    .await
}

async fn run_provider_heartbeat(
    state: GatewayState,
    cloudflared_state: CloudflaredState,
    heartbeat_state: ProviderHeartbeatState,
) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut interval =
        tokio::time::interval(Duration::from_secs(PROVIDER_HEARTBEAT_INTERVAL_SECONDS));
    let mut first_tick = true;
    loop {
        if first_tick {
            // The first interval tick fires immediately; the initial heartbeat is
            // already sent by run_agent before spawning this task, so skip it.
            first_tick = false;
            interval.tick().await;
        }
        interval.tick().await;

        let config = state.config.read().await.clone();
        let current_target = heartbeat_state.target.read().await.clone();

        // 1. Resolve which model this provider is actually serving.
        let discovered = match discover_loaded_model(&config, &state.client).await {
            Ok(model_id) => Some(model_id),
            Err(err) if err == "the model runtime has no loaded model" => None,
            // A transient network/runtime error must never flip the provider
            // offline — treat it as "keep last known model available" so a
            // brief hiccup does not take the marketplace listing down.
            Err(err) => {
                log::warn!("unable to refresh the loaded model: {err}");
                current_target.clone().map(|target| target.model_id)
            }
        };

        match discovered {
            Some(model_id) => {
                // Provider is serving a concrete model: reset the debounce streak
                // and keep it online. If the served model changed, unpublish the
                // old one first *without* closing grants, then publish the new one.
                heartbeat_state
                    .model_gone_streak
                    .store(0, Ordering::Relaxed);
                // Only advertise online when the public transport is actually up.
                // Heal the tunnel first so a crashed connector does not publish an
                // unusable "online" state.
                if !ensure_tunnel_healthy(&state, &cloudflared_state).await {
                    continue;
                }
                if current_target
                    .as_ref()
                    .is_some_and(|target| target.model_id != model_id)
                {
                    let old_model_id = &current_target
                        .as_ref()
                        .expect("target checked above")
                        .model_id;
                    if let Err(err) = post_provider_heartbeat(
                        old_model_id,
                        false,
                        false,
                        Some("model_switch"),
                        &state,
                        &cloudflared_state,
                    )
                    .await
                    {
                        log::warn!("unable to unpublish replaced model: {err}");
                        continue;
                    }
                }
                if let Err(err) =
                    post_provider_heartbeat(&model_id, true, true, None, &state, &cloudflared_state)
                        .await
                {
                    log::warn!("background provider heartbeat failed: {err}");
                } else {
                    *heartbeat_state.target.write().await = Some(HeartbeatTarget { model_id });
                }
            }
            None => {
                // No model is loaded. Debounce so a transient reload window does
                // not flap availability. While debouncing, keep heart-beating
                // with model_loaded=false so the control plane reads the provider
                // as DEGRADED ("reloading") rather than STALE/OFFLINE, and keeps
                // any active grant open. Only after several consecutive empty
                // observations do we report the provider as fully offline.
                let streak = heartbeat_state
                    .model_gone_streak
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                if streak < PROVIDER_MODEL_GONE_DEBOUNCE {
                    if let Some(target) = current_target.as_ref() {
                        if let Err(err) = post_provider_heartbeat(
                            &target.model_id,
                            true,
                            false,
                            Some("model_reloading"),
                            &state,
                            &cloudflared_state,
                        )
                        .await
                        {
                            log::warn!("background provider heartbeat failed: {err}");
                        }
                    }
                } else if let Some(target) = current_target {
                    if post_provider_heartbeat(
                        &target.model_id,
                        false,
                        false,
                        Some("model_unloaded"),
                        &state,
                        &cloudflared_state,
                    )
                    .await
                    .is_ok()
                    {
                        *heartbeat_state.target.write().await = None;
                    }
                }
            }
        }
    }
}

async fn report_terminal_usage(meter: Arc<UsageMeter>) {
    use std::io::Write;

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut was_connected = false;
    loop {
        interval.tick().await;
        let snapshot = meter.snapshot();
        if snapshot.consumer_connected {
            if !was_connected {
                was_connected = true;
            }
            print!(
                "\x1b[2K\rConsumer connected (time: {} seconds)\n\x1b[2K\rInput: {} | output: {} | tokens/sec: {:.2}\x1b[1A",
                snapshot.consumer_connected_seconds,
                snapshot.input_tokens,
                snapshot.output_tokens,
                snapshot.tokens_per_second
            );
            let _ = std::io::stdout().flush();
        } else if was_connected {
            println!("\x1b[1B");
            println!("Consumer disconnected. Waiting for connection...");
            was_connected = false;
        }
    }
}

/// Print a terminal notice when control-plane heartbeats start failing (and again
/// when they recover) so the operator can react before the marketplace listing
/// drops.
async fn warn_on_heartbeat_health(health: Arc<HeartbeatHealth>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut warned = false;
    loop {
        interval.tick().await;
        let failures = health.consecutive_failures.load(Ordering::Relaxed);
        if failures >= 2 && !warned {
            warned = true;
            let last_ok = health.last_ok_unix.load(Ordering::Relaxed);
            let since = if last_ok == 0 {
                "never".to_string()
            } else {
                format!("{}s ago", unix_timestamp().saturating_sub(last_ok))
            };
            println!(
                "\n\u{26a0} control-plane heartbeat failing ({failures} in a row; last ok {since}). \
                 The marketplace listing may drop if this continues."
            );
        } else if failures == 0 && warned {
            warned = false;
            println!("\u{2713} control-plane heartbeat recovered.");
        }
    }
}

async fn health(State(state): State<GatewayState>) -> impl IntoResponse {
    let config = state.config.read().await;
    Json(json!({
        "status": "ok",
        "configured": !config.url.trim().is_empty(),
        "paired": !config.device_id.is_empty() && !config.pairing_token.is_empty(),
        "usage": state.meter.snapshot(),
        "heartbeat": state.heartbeat_health.snapshot(),
    }))
}

async fn connect_consumer_session(
    State(state): State<GatewayState>,
    Json(body): Json<SessionDisconnect>,
) -> Response<Body> {
    let config = state.config.read().await.clone();
    if config.device_id.is_empty() || config.pairing_token.is_empty() {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "the provider device is not paired",
        );
    }
    let response = state
        .client
        .get(format!(
            "{}/v1/devices/{}/inference-grants/{}/status",
            control_plane_url(),
            config.device_id,
            body.grant_id
        ))
        .header("X-DoodleIQ-Pairing-Token", &config.pairing_token)
        .send()
        .await;
    let Ok(response) = response else {
        return json_error(StatusCode::BAD_GATEWAY, "unable to verify consumer session");
    };
    if !response.status().is_success() {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "consumer session was not verified",
        );
    }
    let status = response.json::<Value>().await.ok();
    let is_issued = status
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        == Some("issued");
    let expires_at = status
        .as_ref()
        .and_then(|value| value.get("expires_at_epoch"))
        .and_then(Value::as_u64);
    let started_at = status
        .as_ref()
        .and_then(|value| value.get("started_at_epoch"))
        .and_then(Value::as_u64);
    if !is_issued || expires_at.is_none() || started_at.is_none() {
        return json_error(StatusCode::CONFLICT, "consumer session is not active");
    }
    state.meter.note_consumer_activity(
        &body.grant_id,
        expires_at.expect("expiry checked above"),
        started_at.expect("start checked above"),
    );
    Json(json!({"connected": true})).into_response()
}

async fn disconnect_consumer_session(
    State(state): State<GatewayState>,
    Json(body): Json<SessionDisconnect>,
) -> Response<Body> {
    let config = state.config.read().await.clone();
    if config.device_id.is_empty() || config.pairing_token.is_empty() {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "the provider device is not paired",
        );
    }
    // Clear the locally tracked consumer session immediately. The marketplace
    // DELETE (matching grant id) is the authoritative source of settlement, and
    // surfacing the disconnect locally must not depend on the marketplace having
    // already flipped the grant to "completed" (a race that left the terminal
    // streaming "Consumer connected" indefinitely). The marketplace status check
    // below is best-effort and only used to report revenue.
    let connected = state.meter.is_connected(&body.grant_id);
    let disconnected = state.meter.disconnect(&body.grant_id);

    // Best-effort marketplace status lookup for revenue reporting. Never block
    // the local disconnect on this; network/pairing hiccups should not stall the
    // terminal from clearing "Consumer connected".
    let mut provider_revenue = 0.0;
    let status_response = state
        .client
        .get(format!(
            "{}/v1/devices/{}/inference-grants/{}/status",
            control_plane_url(),
            config.device_id,
            body.grant_id
        ))
        .header("X-DoodleIQ-Pairing-Token", &config.pairing_token)
        .send()
        .await;
    if let Ok(response) = status_response {
        if response.status().is_success() {
            if let Ok(status) = response.json::<Value>().await {
                provider_revenue = status
                    .get("provider_revenue")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
            }
        }
    }
    if disconnected {
        log::info!("disconnected consumer session {}", body.grant_id);
    } else if connected {
        log::warn!(
            "disconnect requested for {} but matched a different/unknown session",
            body.grant_id
        );
    }
    if disconnected || connected {
        println!("Session revenue: ${provider_revenue:.4}");
    }
    Json(json!({"disconnected": disconnected, "provider_revenue": provider_revenue}))
        .into_response()
}

async fn proxy(State(state): State<GatewayState>, request: Request) -> Response<Body> {
    let started_at = Instant::now();
    let config = state.config.read().await.clone();
    if config.url.trim().is_empty() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the local model runtime is not configured; run `doodleiq configure`",
        );
    }
    let Some((grant_id, request_token)) = inference_grant(request.headers()) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "a DoodleIQ inference grant is required",
        );
    };
    let validation =
        match validate_inference_grant(&state, &config, &grant_id, &request_token).await {
            Ok(validation) => validation,
            Err(message) => {
                // The grant is no longer valid (expired, reaped for idleness, or
                // already closed). If it corresponds to the consumer session this
                // gateway is currently tracking, clear the local "connected" state
                // so the terminal stops showing "Consumer connected". Otherwise an
                // idle consumer that is kicked out server-side would leave the CLI
                // streaming "connected" indefinitely with no way to learn the grant
                // was closed.
                state.meter.disconnect(&grant_id);
                return json_error(StatusCode::UNAUTHORIZED, &message);
            }
        };
    state.meter.note_consumer_activity(
        &grant_id,
        validation.expires_at_epoch,
        validation.started_at_epoch,
    );

    let (parts, body) = request.into_parts();
    // The body is buffered (not streamed) on purpose: a streaming chat
    // completion needs `stream_options.include_usage` injected so we can meter
    // token usage, and that means parsing the JSON. The proxy serves one
    // consumer per provider at a time, so buffering a bounded payload is not a
    // throughput concern. Non-chat paths could be streamed straight through,
    // but that split isn't worth the extra code path at this concurrency.
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(body) => request_body_with_usage(body, parts.uri.path()),
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let target = match upstream_url(&config.url, &parts.uri) {
        Ok(target) => target,
        Err(message) => return json_error(StatusCode::BAD_GATEWAY, &message),
    };

    let mut upstream = state.upstream.request(parts.method.clone(), target);
    for (name, value) in &parts.headers {
        if should_forward_request_header(name) {
            upstream = upstream.header(name, value);
        }
    }
    if !config.api_key.trim().is_empty() {
        upstream = upstream.header(header::AUTHORIZATION, format!("Bearer {}", config.api_key));
    }
    if !body.is_empty() {
        upstream = upstream.body(body);
    }

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(err) => {
            log::warn!("model runtime request failed: {err}");
            return json_error(StatusCode::BAD_GATEWAY, "unable to reach the model runtime");
        }
    };
    let status = upstream.status();
    let response_headers = upstream.headers().clone();
    let is_stream = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));

    if is_stream {
        let meter = state.meter.clone();
        let receipt_state = state.clone();
        let receipt_config = config.clone();
        let receipt_grant_id = grant_id.clone();
        let receipt_token = request_token.clone();
        let stream = upstream.bytes_stream().scan(
            (Vec::<u8>::new(), false),
            move |(buffer, recorded), chunk| {
                if let Ok(bytes) = &chunk {
                    buffer.extend_from_slice(bytes);
                    if !*recorded {
                        if let Some(usage) = usage_from_sse_buffer(buffer) {
                            let receipt_meter = meter.clone();
                            let receipt_state = receipt_state.clone();
                            let receipt_config = receipt_config.clone();
                            let receipt_grant_id = receipt_grant_id.clone();
                            let receipt_token = receipt_token.clone();
                            let duration_ms = started_at.elapsed().as_millis() as u64;
                            tokio::spawn(async move {
                                if let Err(err) = submit_usage_receipt(
                                    &receipt_state,
                                    &receipt_config,
                                    &receipt_grant_id,
                                    &receipt_token,
                                    usage,
                                    duration_ms,
                                    "completed",
                                )
                                .await
                                {
                                    log::warn!("unable to submit usage receipt: {err}");
                                } else {
                                    receipt_meter.record(usage, duration_ms);
                                }
                            });
                            *recorded = true;
                        }
                    }
                    trim_sse_buffer(buffer);
                }
                futures_util::future::ready(Some(chunk))
            },
        );
        return build_response(status, &response_headers, Body::from_stream(stream));
    }

    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            log::warn!("unable to read model runtime response: {err}");
            return json_error(
                StatusCode::BAD_GATEWAY,
                "unable to read the model runtime response",
            );
        }
    };
    if let Some(usage) = usage_from_json(&bytes) {
        if let Err(err) = submit_usage_receipt(
            &state,
            &config,
            &grant_id,
            &request_token,
            usage,
            started_at.elapsed().as_millis() as u64,
            if status.is_success() {
                "completed"
            } else {
                "failed"
            },
        )
        .await
        {
            log::warn!("unable to submit usage receipt: {err}");
        } else {
            state
                .meter
                .record(usage, started_at.elapsed().as_millis() as u64);
        }
    }
    build_response(status, &response_headers, Body::from(bytes))
}

fn inference_grant(headers: &HeaderMap) -> Option<(String, String)> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|credential| credential.split_once('.'))
        .filter(|(grant_id, token)| !grant_id.is_empty() && token.len() >= 20)
        .map(|(grant_id, token)| (grant_id.to_string(), token.to_string()))
}

async fn validate_inference_grant(
    state: &GatewayState,
    config: &RuntimeConfig,
    grant_id: &str,
    request_token: &str,
) -> Result<GrantValidation, String> {
    if config.device_id.is_empty() || config.pairing_token.is_empty() {
        return Err("the provider device is not paired".to_string());
    }
    let response = state
        .client
        .post(format!(
            "{}/v1/devices/{}/inference-grants/validate",
            control_plane_url(),
            config.device_id
        ))
        .header("X-DoodleIQ-Pairing-Token", &config.pairing_token)
        .json(&json!({
            "grant_id": grant_id,
            "request_token": request_token,
        }))
        .send()
        .await
        .map_err(|err| format!("unable to validate inference grant: {err}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read validation response".to_string());
        log::warn!("inference grant validation failed: {status}: {detail}");
        return Err(format!(
            "DoodleIQ API key validation failed ({status}): {detail}"
        ));
    }
    let validation: GrantValidation = response
        .json()
        .await
        .map_err(|err| format!("invalid grant validation response: {err}"))?;
    log::info!(
        "validated inference grant model={} max_tokens={} expires_at={}",
        validation.model_id,
        validation.max_tokens,
        validation.expires_at
    );
    Ok(validation)
}

async fn submit_usage_receipt(
    state: &GatewayState,
    config: &RuntimeConfig,
    grant_id: &str,
    request_token: &str,
    usage: TokenUsage,
    duration_ms: u64,
    status: &str,
) -> Result<(), String> {
    let request_id = format!(
        "{}-{}",
        grant_id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_nanos()
    );
    let body = serde_json::to_vec(&json!({
        "grant_id": grant_id,
        "request_token": request_token,
        "request_id": request_id,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "duration_ms": duration_ms,
        "status": status,
    }))
    .map_err(|err| format!("unable to serialize usage receipt: {err}"))?;
    let signature = state.signing_key.sign(&body);
    let response = state
        .client
        .post(format!("{}/v1/usage-receipts", control_plane_url()))
        .header("Content-Type", "application/json")
        .header("X-DoodleIQ-Device", &config.device_id)
        .header(
            "X-DoodleIQ-Signature",
            URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        )
        .body(body)
        .send()
        .await
        .map_err(|err| format!("usage receipt service is unavailable: {err}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read error response".to_string());
        if status == StatusCode::UNAUTHORIZED && detail.contains("Invalid device signature") {
            return Err(
                "the local device key no longer matches this pairing; run `doodleiq reset`, then `doodleiq pair`"
                    .to_string(),
            );
        }
        return Err(format!("usage receipt service returned {status}: {detail}"));
    }
    Ok(())
}

fn upstream_url(base: &str, uri: &axum::http::Uri) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(base).map_err(|_| "invalid model runtime URL".to_string())?;
    url.set_path(uri.path());
    url.set_query(uri.query());
    Ok(url)
}

fn should_forward_request_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization" | "host" | "content-length" | "connection" | "transfer-encoding"
    )
}

fn should_forward_response_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "content-length" | "connection" | "transfer-encoding" | "content-encoding"
    )
}

fn build_response(status: StatusCode, headers: &HeaderMap, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in headers {
        if should_forward_response_header(name) {
            response.headers_mut().insert(name.clone(), value.clone());
        }
    }
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(
        json!({"error": {"message": message, "type": "gateway_error"}}).to_string(),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn request_body_with_usage(body: Bytes, path: &str) -> Bytes {
    if path != "/v1/chat/completions" {
        return body;
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    if payload.get("stream").and_then(Value::as_bool) != Some(true) {
        return body;
    }
    let Some(object) = payload.as_object_mut() else {
        return body;
    };
    let options = object
        .entry("stream_options")
        .or_insert_with(|| Value::Object(Default::default()));
    if let Some(options) = options.as_object_mut() {
        options.insert("include_usage".to_string(), Value::Bool(true));
    }
    serde_json::to_vec(&payload)
        .map(Bytes::from)
        .unwrap_or(body)
}

fn usage_from_json(bytes: &[u8]) -> Option<TokenUsage> {
    let payload: Value = serde_json::from_slice(bytes).ok()?;
    token_usage(&payload)
}

fn token_usage(payload: &Value) -> Option<TokenUsage> {
    let usage = payload.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))?
        .as_u64()?;
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))?
        .as_u64()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

fn usage_from_sse_buffer(buffer: &[u8]) -> Option<TokenUsage> {
    // This runs on every streamed chunk until usage is found, but OpenAI only
    // emits the `usage` block in the final event (with `stream_options`). Gate
    // the UTF-8 + per-line JSON parsing on a cheap byte scan so every earlier
    // chunk — the overwhelming majority — costs one memcmp sweep and no
    // allocation.
    if !buffer.windows(7).any(|w| w == b"\"usage\"") {
        return None;
    }
    let text = std::str::from_utf8(buffer).ok()?;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<Value>(data) {
            if let Some(usage) = token_usage(&payload) {
                return Some(usage);
            }
        }
    }
    None
}

fn trim_sse_buffer(buffer: &mut Vec<u8>) {
    const KEEP: usize = 64 * 1024;
    if buffer.len() > KEEP {
        buffer.drain(..buffer.len() - KEEP);
    }
}

async fn run_gateway(state: GatewayState, listener: tokio::net::TcpListener) {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/session-connect", post(connect_consumer_session))
        .route("/v1/session-disconnect", post(disconnect_consumer_session))
        .route("/v1/{*path}", any(proxy))
        .layer(cors)
        .with_state(state);

    log::info!("local metering gateway listening at http://{GATEWAY_ADDRESS}");
    if let Err(err) = axum::serve(listener, app).await {
        log::error!("local metering gateway stopped: {err}");
    }
}

#[derive(Parser)]
#[command(
    name = "doodleiq",
    version,
    about = "Rent out your local LLM on the DoodleIQ marketplace",
    long_about = "\
Rent out your local LLM on the DoodleIQ marketplace.

Run a model locally with any OpenAI-compatible runtime (Ollama, LM Studio, oMLX,
vLLM, llama.cpp, ...), and this agent meters it, exposes it through a managed
tunnel, and pays you out for the compute consumers use.

GETTING STARTED
  1. doodleiq configure   Point the agent at your local model runtime.
                          e.g. Ollama:  doodleiq configure --url http://127.0.0.1:11434/v1/models
                          A running model is required before pairing.

  2. doodleiq pair        Link this machine to your DoodleIQ account. Prints a URL
                          and a code; approve it in the browser. Done once per machine.

  3. doodleiq run         Start serving. Keeps the tunnel and heartbeat alive and
                          streams consumer requests to your local model until you
                          press Ctrl+C. Run this under a supervisor (launchd/systemd)
                          so it restarts on reboot.

  doodleiq status         Check configuration and that every dependency is ready.

Run `doodleiq <command> --help` for details on any step."
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Step 1: point the agent at your local model runtime
    #[command(long_about = "\
Step 1 of 3. Save where your local OpenAI-compatible model runtime is listening.

Works with any runtime that speaks the OpenAI API — Ollama, LM Studio, oMLX,
vLLM, llama.cpp, and others. The agent reads the model list from --url to learn
which model you are serving, and proxies consumer requests to the same host.

  Ollama      doodleiq configure --url http://127.0.0.1:11434/v1/models
  LM Studio   doodleiq configure --url http://127.0.0.1:1234/v1/models
  oMLX        doodleiq configure            (this is the default)

Pass --api-key only if your runtime requires a bearer token (Ollama and LM Studio
do not). The value is also read from DOODLEIQ_MODEL_API_KEY.

Settings are written to the config file (see `doodleiq status`) and reused by
every later command; re-run this whenever your runtime moves.")]
    Configure {
        /// URL of your runtime's model-list endpoint (its host is also used to proxy requests)
        #[arg(long, default_value = DEFAULT_RUNTIME_URL)]
        url: String,
        /// Bearer token for the runtime, if it requires one (Ollama/LM Studio do not)
        #[arg(long, env = "DOODLEIQ_MODEL_API_KEY")]
        api_key: Option<String>,
    },
    /// Step 2: link this machine to your DoodleIQ account
    #[command(long_about = "\
Step 2 of 3. Pair this machine with your DoodleIQ provider account.

Prints a verification URL and a short code — open the URL on any device, sign in,
and enter the code to approve this machine. The command waits until you approve
or the timeout elapses, then stores the pairing so you never need to repeat it.

Requires a model already loaded in your runtime (from step 1).")]
    Pair {
        /// How long to wait for browser approval before giving up
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    /// Step 3: start serving — runs the gateway, tunnel, and heartbeat until Ctrl+C
    #[command(long_about = "\
Step 3 of 3. Start earning.

Opens the local metering gateway, brings up the managed Cloudflare tunnel, and
begins the heartbeat loop that keeps your model listed on the marketplace. Every
consumer request is streamed to your local runtime, metered, and billed. Runs in
the foreground until Ctrl+C, which cleanly delists the model.

Run this under launchd or systemd so it comes back after a crash or reboot.")]
    Run,
    /// Show configuration and check every dependency is ready
    #[command(long_about = "\
Print the current configuration (secrets redacted), the model your runtime
reports as loaded, whether this machine is paired, and whether the tunnel binary
is installed. Use this to diagnose a failed `configure`, `pair`, or `run`.")]
    Status,
    /// Clear pairing state (add --all to also wipe the runtime configuration)
    #[command(long_about = "\
Unlink this machine. By default only the pairing is cleared, so you can re-pair
without reconfiguring your runtime. Pass --all to also erase the runtime URL/key
and the device signing key — a full reset.")]
    Reset {
        /// Also erase the runtime configuration and device key, not just the pairing
        #[arg(long)]
        all: bool,
    },
}

fn gateway_state(config: RuntimeConfig) -> Result<GatewayState, String> {
    // The upstream client must never impose a total-request timeout: an SSE
    // chat completion can legitimately stream tokens for many minutes. Only
    // cap how long we'll wait to establish the connection to the model runtime.
    let upstream = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|err| format!("Unable to initialize the upstream client: {err}"))?;
    Ok(GatewayState {
        config: Arc::new(RwLock::new(config)),
        meter: Arc::new(UsageMeter::default()),
        client: provider_http_client()?,
        upstream,
        heartbeat_health: Arc::new(HeartbeatHealth::default()),
        signing_key: Arc::new(device_signing_key()?),
    })
}

pub async fn run_cli() -> Result<(), String> {
    load_environment_files();
    // Route diagnostic INFO logs to a file so they do not interleave with the
    // live terminal counter, which repaints the current line with `\r`.
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(log_file) = log_file_path() {
        if let Ok(file) = fs::File::create(&log_file) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();
    let cli = Cli::parse();
    let config = load_config()?;
    let state = gateway_state(config)?;

    match cli.command {
        CliCommand::Configure { url, api_key } => {
            save_runtime_config(url, api_key, &state).await?;
            println!("Saved configuration to {}", config_path()?.display());
        }
        CliCommand::Pair { timeout_seconds } => {
            let pairing = begin_provider_pairing().await?;
            println!("Open this URL on any device:\n{}", pairing.verification_url);
            println!("Pairing code: {}", pairing.code);
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
            loop {
                if tokio::time::Instant::now() >= deadline {
                    return Err("pairing timed out; run `doodleiq pair` again".to_string());
                }
                if let Some(identity) =
                    poll_provider_pairing(pairing.id.clone(), pairing.pairing_token.clone(), &state)
                        .await?
                {
                    println!("Paired provider {}", identity.provider_id);
                    println!("Public hostname: {}", identity.hostname);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        CliCommand::Run => run_agent(state).await?,
        CliCommand::Status => print_status(&state).await?,
        CliCommand::Reset { all } => reset_config(all)?,
    }
    Ok(())
}

async fn run_agent(state: GatewayState) -> Result<(), String> {
    let config = state.config.read().await.clone();
    if config.url.trim().is_empty() {
        return Err("run `doodleiq configure` first".to_string());
    }
    if config.device_id.is_empty() || config.pairing_token.is_empty() {
        return Err("run `doodleiq pair` first".to_string());
    }
    let model_id = discover_loaded_model(&config, &state.client).await?;

    let listener = tokio::net::TcpListener::bind(GATEWAY_ADDRESS)
        .await
        .map_err(|err| format!("unable to bind local gateway at {GATEWAY_ADDRESS}: {err}"))?;
    let cloudflared = CloudflaredState::default();
    if !resume_provider_tunnel(&state, &cloudflared).await? {
        return Err("the control plane did not return a Cloudflare tunnel token".to_string());
    }
    let heartbeat = ProviderHeartbeatState::default();
    let hostname =
        send_provider_heartbeat(model_id.clone(), true, &state, &cloudflared, &heartbeat).await?;
    println!("DoodleIQ provider online at https://{hostname}/v1");
    println!("Serving model: {model_id}");
    println!("Waiting for consumer connection... (press Ctrl+C to stop)");

    let mut gateway_task = tokio::spawn(run_gateway(state.clone(), listener));
    let mut heartbeat_task = tokio::spawn(run_provider_heartbeat(
        state.clone(),
        cloudflared.clone(),
        heartbeat,
    ));
    let terminal_task = tokio::spawn(report_terminal_usage(state.meter.clone()));
    let heartbeat_watch_task =
        tokio::spawn(warn_on_heartbeat_health(state.heartbeat_health.clone()));

    // Wait for Ctrl+C or for a critical task to terminate unexpectedly. If the
    // gateway or the heartbeat loop dies, return an error so an external
    // supervisor (systemd/launchd) can restart the agent rather than leaving a
    // "phantom online" provider with no live tunnel or heartbeats.
    let shutdown_reason = tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.map_err(|err| format!("unable to wait for Ctrl+C: {err}"))?;
            gateway_task.abort();
            heartbeat_task.abort();
            terminal_task.abort();
            heartbeat_watch_task.abort();
            None
        }
        result = &mut heartbeat_task => {
            gateway_task.abort();
            terminal_task.abort();
            heartbeat_watch_task.abort();
            match result {
                Ok(()) => Some("heartbeat loop ended unexpectedly".to_string()),
                Err(join) => Some(format!("heartbeat loop panicked: {join}")),
            }
        }
        result = &mut gateway_task => {
            heartbeat_task.abort();
            terminal_task.abort();
            heartbeat_watch_task.abort();
            match result {
                Ok(()) => Some("local gateway ended unexpectedly".to_string()),
                Err(join) => Some(format!("local gateway panicked: {join}")),
            }
        }
    };

    if let Err(error) = post_provider_heartbeat(
        &model_id,
        false,
        false,
        Some("shutdown"),
        &state,
        &cloudflared,
    )
    .await
    {
        log::warn!("unable to mark provider offline during shutdown: {error}");
    }
    stop_cloudflared(&cloudflared)?;
    println!("DoodleIQ provider stopped");

    if let Some(reason) = shutdown_reason {
        log::error!("{reason}");
        Err(format!(
            "{reason}: the provider agent is stopping so a supervisor can restart it"
        ))
    } else {
        Ok(())
    }
}

async fn print_status(state: &GatewayState) -> Result<(), String> {
    let config = state.config.read().await.clone();
    let cloudflared = CloudflaredState::default();
    let tunnel = cloudflared_status(&cloudflared);
    println!("control_plane: {}", control_plane_url());
    println!("config: {}", config_path()?.display());
    println!("runtime_url: {}", config.url);
    let model = if config.url.trim().is_empty() {
        "unavailable (run `doodleiq configure`)".to_string()
    } else {
        discover_loaded_model(&config, &state.client)
            .await
            .unwrap_or_else(|error| format!("unavailable ({error})"))
    };
    println!("loaded_model: {model}");
    println!(
        "runtime_key: {}",
        if config.api_key.is_empty() {
            "none (unauthenticated runtime)"
        } else {
            "configured"
        }
    );
    println!(
        "paired: {}",
        !config.device_id.is_empty() && !config.pairing_token.is_empty()
    );
    println!(
        "cloudflared: {}",
        if tunnel.installed {
            tunnel.version.as_deref().unwrap_or("installed")
        } else {
            "missing"
        }
    );
    Ok(())
}

fn reset_config(all: bool) -> Result<(), String> {
    let mut config = load_config()?;
    config.provider_id.clear();
    config.pairing_id.clear();
    config.device_id.clear();
    config.pairing_token.clear();
    if all {
        config.url = DEFAULT_RUNTIME_URL.to_string();
        config.api_key.clear();
        let key = device_key_path()?;
        if key.exists() {
            fs::remove_file(key).map_err(|err| format!("unable to remove device key: {err}"))?;
        }
    }
    persist_config(&config)?;
    println!("Reset {} state", if all { "all local" } else { "pairing" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enables_stream_usage() {
        let body = request_body_with_usage(
            Bytes::from_static(br#"{"stream":true}"#),
            "/v1/chat/completions",
        );
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["stream_options"]["include_usage"], true);
    }

    #[test]
    fn leaves_other_streaming_endpoints_unchanged() {
        let original = Bytes::from_static(br#"{"stream":true}"#);
        let body = request_body_with_usage(original.clone(), "/v1/responses");
        assert_eq!(body, original);
    }

    #[test]
    fn reads_openai_usage() {
        let usage = usage_from_json(
            br#"{"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}"#,
        )
        .unwrap();
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 12,
                output_tokens: 5,
                total_tokens: 17
            }
        );
    }

    #[test]
    fn reads_stream_usage() {
        let event = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4,\"total_tokens\":13}}\n\n";
        assert_eq!(usage_from_sse_buffer(event).unwrap().total_tokens, 13);
    }

    #[test]
    fn sse_buffer_without_usage_returns_none() {
        // Every delta chunk before the final event — the byte gate must skip it.
        let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        assert!(usage_from_sse_buffer(event).is_none());
    }

    #[test]
    fn selects_first_loaded_model_from_status() {
        let payload = json!({"models": [
            {"id": "not-loaded", "loaded": false},
            {"id": "loaded-model", "loaded": true}
        ]});
        assert_eq!(loaded_model_id(&payload).as_deref(), Some("loaded-model"));
    }

    #[test]
    fn falls_back_to_first_model_when_runtime_omits_load_state() {
        // Ollama / LM Studio `/v1/models`: OpenAI list shape, no `loaded` flag.
        let payload = json!({"object": "list", "data": [
            {"id": "llama3.2", "object": "model"},
            {"id": "qwen2.5", "object": "model"}
        ]});
        assert_eq!(loaded_model_id(&payload).as_deref(), Some("llama3.2"));
    }

    #[test]
    fn disconnects_only_the_matching_consumer_session() {
        let meter = UsageMeter::default();
        meter.note_consumer_activity("current-grant", unix_timestamp() + 300, unix_timestamp());
        meter.record(
            TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
            },
            100,
        );

        assert!(!meter.disconnect("different-grant"));
        let connected = meter.snapshot();
        assert!(connected.consumer_connected);
        assert_eq!(connected.total_tokens, 15);
        assert!(meter.disconnect("current-grant"));
        let disconnected = meter.snapshot();
        assert!(!disconnected.consumer_connected);
        assert_eq!(disconnected.requests, 1);
        assert_eq!(disconnected.input_tokens, 10);
        assert_eq!(disconnected.output_tokens, 5);
        assert_eq!(disconnected.total_tokens, 15);
        assert_eq!(disconnected.last_consumer_activity, 0);
        assert_eq!(disconnected.tokens_per_second, 50.0);

        meter.note_consumer_activity("next-grant", unix_timestamp() + 300, unix_timestamp());
        let next_session = meter.snapshot();
        assert!(next_session.consumer_connected);
        assert_eq!(next_session.requests, 0);
        assert_eq!(next_session.total_tokens, 0);
    }
}
