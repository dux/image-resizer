// Process configuration, read once from the environment (and `.env`) at start.

use std::path::PathBuf;
use std::time::Duration;

pub const AUTHCOG_URL: &str = "https://auth.authcog.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEnv {
    Development,
    Test,
    Production,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub env: AppEnv,
    pub port: u16,
    /// Hosts `AuthCog` may send the browser back to; dev also accepts `lvh.me`, its subdomains and `localhost`.
    pub app_hosts: Vec<String>,
    pub admin_emails: Vec<String>,
    pub workers: usize,
    pub quality: i32,
    pub max_size: i32,
    pub max_age: u64,
    pub max_cache_bytes: u64,
    pub max_source_bytes: u64,
    pub allowed_domains: Vec<String>,
    pub allow_private_sources: bool,
    pub f3d_bin: String,
    pub step2glb_bin: String,
    pub data_dir: PathBuf,
    /// How long a resize request waits for its job before answering the spinner.
    pub wait_timeout: Duration,
    pub job_timeout: Duration,
    pub authcog_url: String,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        Some(v) => v.parse().map_err(|_| format!("{name}: cannot parse '{v}'")),
        None => Ok(default),
    }
}

fn ranged(name: &str, default: i64, min: i64, max: i64) -> Result<i64, String> {
    let value = parse(name, default)?;
    if value < min || value > max {
        return Err(format!("{name} must be {min}-{max}, got {value}"));
    }
    Ok(value)
}

pub fn list(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let env = match var("APP_ENV").as_deref() {
            None | Some("development") => AppEnv::Development,
            Some("test") => AppEnv::Test,
            Some("production") => AppEnv::Production,
            Some(other) => {
                return Err(format!(
                    "APP_ENV must be development, test or production, got {other}"
                ));
            }
        };
        let production = env == AppEnv::Production;
        let admin_emails = list(&var("ADMIN_EMAILS").unwrap_or_default());
        if admin_emails.is_empty() {
            return Err("ADMIN_EMAILS is required: comma-separated emails of the admins".into());
        }
        let app_hosts = list(&var("APP_HOSTS").unwrap_or_default());
        if production && app_hosts.is_empty() {
            return Err("APP_HOSTS is required in production".into());
        }
        Ok(Self {
            env,
            port: parse("PORT", 8080)?,
            app_hosts,
            admin_emails,
            workers: ranged("WORKERS", 5, 1, 256)? as usize,
            quality: ranged("QUALITY", 90, 10, 100)? as i32,
            max_size: ranged("MAX_SIZE", 1600, 100, 10_000)? as i32,
            max_age: ranged("MAX_AGE", 86_400, 0, i64::MAX)? as u64,
            max_cache_bytes: ranged("MAX_CACHE_SIZE", 1000, 1, 10_000_000)? as u64 * 1024 * 1024,
            max_source_bytes: ranged("MAX_SOURCE_MB", 50, 1, 10_000)? as u64 * 1024 * 1024,
            allowed_domains: list(&var("ALLOWED_DOMAINS").unwrap_or_default()),
            allow_private_sources: parse("ALLOW_PRIVATE_SOURCES", !production)?,
            f3d_bin: var("F3D_BIN").unwrap_or_else(|| "f3d".into()),
            step2glb_bin: absolute(
                var("STEP2GLB_BIN").unwrap_or_else(|| "scripts/step2glb".into()),
            ),
            data_dir: PathBuf::from(var("DATA_DIR").unwrap_or_else(|| "tmp".into())),
            wait_timeout: Duration::from_secs(10),
            job_timeout: Duration::from_secs(90),
            authcog_url: AUTHCOG_URL.into(),
        })
    }

    /// Defaults for tests: everything local, data under `dir`.
    pub fn test(dir: impl Into<PathBuf>) -> Self {
        Self {
            env: AppEnv::Test,
            port: 0,
            app_hosts: Vec::new(),
            admin_emails: vec!["rejotl@gmail.com".into()],
            workers: 5,
            quality: 90,
            max_size: 1600,
            max_age: 86_400,
            max_cache_bytes: 1000 * 1024 * 1024,
            max_source_bytes: 50 * 1024 * 1024,
            allowed_domains: Vec::new(),
            allow_private_sources: true,
            f3d_bin: "f3d".into(),
            step2glb_bin: absolute("scripts/step2glb".into()),
            data_dir: dir.into(),
            wait_timeout: Duration::from_secs(10),
            job_timeout: Duration::from_secs(90),
            authcog_url: AUTHCOG_URL.into(),
        }
    }

    pub fn is_admin(&self, email: &str) -> bool {
        self.admin_emails.iter().any(|admin| admin == email)
    }

    /// Whether `AuthCog` may send the browser back to `host` (lowercase, no port).
    pub fn own_host(&self, host: &str) -> bool {
        if self.app_hosts.iter().any(|h| h == host) {
            return true;
        }
        self.env != AppEnv::Production
            && (host == "localhost"
                || host == "127.0.0.1"
                || host == "lvh.me"
                || host.ends_with(".lvh.me"))
    }
}

/// Resolve a relative helper path against the start directory so later spawns do not depend on cwd.
fn absolute(path: String) -> String {
    if path.contains('/')
        && let Ok(abs) = std::path::absolute(&path)
    {
        return abs.to_string_lossy().into_owned();
    }
    path
}
