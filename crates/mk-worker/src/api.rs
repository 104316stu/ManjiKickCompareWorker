//! The site's worker API:
//!   POST /hello, POST /lease {"want": n}, GET <clip_url> (X-Lease),
//!   POST /result, POST /release {"jobs": [{job_id, lease}]}.
//! 401/403 = the token is bad or revoked; 409 = the lease went stale;
//! 429 = slow down.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub const PREFIX: &str = "/api/worker/v1";
pub const MAX_BATCH: usize = 8;
const MAX_CLIP_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct Job {
    pub job_id: i64,
    pub lease: String,
    #[serde(default)]
    pub audio_id: Value,
    pub video_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub video_seconds: Option<f64>,
    pub clip_url: String,
    /// The server's clock; only informational (see `leased_at`).
    #[serde(default)]
    pub expires_at: f64,
    /// When the lease reply arrived, by this machine's clock.
    #[serde(skip)]
    pub leased_at: Option<std::time::Instant>,
}

#[derive(Debug)]
pub enum ApiError {
    /// 401/403: token bad or revoked.
    Auth(u16),
    /// 409: the lease is gone.
    Stale,
    /// 429, with the server's Retry-After in seconds when it sent one.
    Slow(Option<f64>),
    Http(u16),
    Net(String),
    Bad(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth(c) => write!(f, "token refused (HTTP {c})"),
            ApiError::Stale => write!(f, "lease no longer valid (HTTP 409)"),
            ApiError::Slow(_) => write!(f, "server asked to slow down (HTTP 429)"),
            ApiError::Http(c) => write!(f, "HTTP {c}"),
            ApiError::Net(s) => write!(f, "network: {s}"),
            ApiError::Bad(s) => write!(f, "bad reply: {s}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Hello {
    pub name: String,
    pub trust: String,
    pub algo: String,
    pub max_video_seconds: Option<u32>,
    pub video_max_bytes: Option<u64>,
    pub lease_seconds: Option<f64>,
}

pub fn agent(read_timeout: Duration) -> ureq::Agent {
    let b = ureq::AgentBuilder::new()
        .user_agent(&crate::user_agent())
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(read_timeout)
        .timeout_write(Duration::from_secs(60))
        .redirects(5);
    #[cfg(windows)]
    let b = match native_tls::TlsConnector::new() {
        Ok(c) => b.tls_connector(std::sync::Arc::new(c)),
        Err(_) => b,
    };
    b.build()
}

pub struct Api {
    agent: ureq::Agent,
    server: String,
    token: String,
}

fn map_err(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::Status(c, _) if c == 401 || c == 403 => ApiError::Auth(c),
        ureq::Error::Status(409, _) => ApiError::Stale,
        ureq::Error::Status(429, resp) => ApiError::Slow(
            resp.header("Retry-After").and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0),
        ),
        ureq::Error::Status(c, _) => ApiError::Http(c),
        ureq::Error::Transport(t) => ApiError::Net(t.to_string()),
    }
}

impl Api {
    pub fn new(server: &str, token: &str) -> Api {
        Api {
            agent: agent(Duration::from_secs(60)),
            server: server.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        let resp = self
            .agent
            .post(&format!("{}{}{}", self.server, PREFIX, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(map_err)?;
        let text = resp.into_string().map_err(|e| ApiError::Net(e.to_string()))?;
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| ApiError::Bad(e.to_string()))
    }

    pub fn hello(&self, threads: usize, algo: &str) -> Result<Hello, ApiError> {
        let platform = format!("{} {} rust", crate::os_name(), std::env::consts::ARCH);
        let v = self.post("/hello", &json!({"version": crate::VERSION, "platform": platform,
                                             "threads": threads, "algo": algo}))?;
        let limits = &v["limits"];
        Ok(Hello {
            name: v["name"].as_str().unwrap_or("?").to_string(),
            trust: v["trust"].as_str().unwrap_or("?").to_string(),
            algo: v["algo"].as_str().unwrap_or("").to_string(),
            max_video_seconds: limits["max_video_seconds"].as_u64().map(|n| n as u32),
            video_max_bytes: limits["video_max_bytes"].as_u64(),
            lease_seconds: v["lease_seconds"].as_f64(),
        })
    }

    /// Up to `want` jobs, and how long the server suggests waiting when none.
    pub fn lease(&self, want: usize) -> Result<(Vec<Job>, f64), ApiError> {
        let v = self.post("/lease", &json!({"want": want.clamp(1, MAX_BATCH)}))?;
        let at = Instant::now();
        let mut jobs = Vec::new();
        let mut unreadable = Vec::new();
        for item in v["jobs"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            match serde_json::from_value::<Job>(item.clone()) {
                Ok(mut job) => {
                    job.leased_at = Some(at);
                    jobs.push(job);
                }
                // Hand back what can't be read, if it has an id and lease at all.
                Err(e) => {
                    crate::log!("unreadable job in the lease reply ({e}); handing it back");
                    if let (Some(id), Some(lease)) = (item["job_id"].as_i64(), item["lease"].as_str()) {
                        unreadable.push(json!({"job_id": id, "lease": lease}));
                    }
                }
            }
        }
        if !unreadable.is_empty() {
            let _ = self.post("/release", &json!({ "jobs": unreadable }));
        }
        let poll = v["poll_after_seconds"].as_f64().unwrap_or(60.0);
        Ok((jobs, poll))
    }

    pub fn clip(&self, job: &Job) -> Result<Vec<u8>, ApiError> {
        // Only ever send the token to our own server.
        let url = if job.clip_url.starts_with('/') {
            format!("{}{}", self.server, job.clip_url)
        } else if job.clip_url.starts_with(&format!("{}/", self.server)) {
            job.clip_url.clone()
        } else {
            return Err(ApiError::Bad(format!("clip_url outside the server: {}", job.clip_url)));
        };
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("X-Lease", &job.lease)
            .call()
            .map_err(map_err)?;
        let expected = resp.header("X-Clip-Sha256").map(|h| h.trim().to_ascii_lowercase());
        let length: Option<usize> = resp.header("Content-Length").and_then(|h| h.trim().parse().ok());
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(MAX_CLIP_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ApiError::Net(e.to_string()))?;
        if bytes.len() as u64 > MAX_CLIP_BYTES || length.is_some_and(|n| n != bytes.len()) {
            return Err(ApiError::Bad("clip truncated or oversized".into()));
        }
        if let Some(expected) = expected {
            let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            if got != expected {
                return Err(ApiError::Bad("clip checksum mismatch".into()));
            }
        }
        Ok(bytes)
    }

    pub fn result(&self, body: &Value) -> Result<Value, ApiError> {
        self.post("/result", body)
    }

    pub fn release(&self, jobs: &[Job]) {
        if jobs.is_empty() {
            return;
        }
        let list: Vec<Value> = jobs.iter().map(|j| json!({"job_id": j.job_id, "lease": j.lease})).collect();
        // The lease runs out by itself anyway; a failure here costs nothing.
        let _ = self.post("/release", &json!({ "jobs": list }));
    }
}
