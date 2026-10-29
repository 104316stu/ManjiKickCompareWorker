//! Pairing: connect this machine to the site without copying a token.
//! The worker asks for a code, the user approves it on the site (logged in),
//! and the worker picks up its token once.
//!
//!   POST /api/worker/v1/pair/start {name, platform}
//!        -> {code, poll_id, approve_url, expires_in, poll_seconds}
//!   POST /api/worker/v1/pair/poll {poll_id}
//!        -> {status: pending|approved|denied|expired, token?}

use crate::api::PREFIX;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Started {
    pub code: String,
    pub poll_id: String,
    pub approve_url: String,
    pub expires_in: f64,
    pub poll_seconds: f64,
}

pub enum Outcome {
    Approved(String),
    Denied,
    Expired,
    Cancelled,
}

fn post(server: &str, path: &str, body: &Value) -> Result<Value, String> {
    let agent = crate::api::agent(Duration::from_secs(30));
    let resp = agent
        .post(&format!("{}{}{}", server.trim_end_matches('/'), PREFIX, path))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| match e {
            ureq::Error::Status(429, _) => "the site asked to slow down; try again in a minute".to_string(),
            ureq::Error::Status(400 | 404 | 405, _) if path == "/pair/start" => {
                "the site doesn't support pairing yet; try again later".to_string()
            }
            ureq::Error::Status(c, _) => format!("the site answered HTTP {c}"),
            ureq::Error::Transport(t) => format!("can't reach the site: {t}"),
        })?;
    let text = resp.into_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("bad reply: {e}"))
}

/// This computer's name, the default worker name.
pub fn hostname() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default();
    let name = raw.trim();
    if name.is_empty() { "worker".into() } else { name.chars().take(60).collect() }
}

pub fn start(server: &str, name: &str) -> Result<Started, String> {
    let platform = format!("{} {}", crate::os_name(), std::env::consts::ARCH);
    let v = post(server, "/pair/start", &json!({"name": name, "platform": platform}))?;
    let field = |k: &str| v[k].as_str().map(str::to_string).ok_or_else(|| format!("reply has no {k}"));
    let approve_url = field("approve_url")?;
    // The approve page has to be on our own site.
    let approve_url = if approve_url.starts_with('/') {
        format!("{}{}", server.trim_end_matches('/'), approve_url)
    } else if approve_url.starts_with(&format!("{}/", server.trim_end_matches('/'))) {
        approve_url
    } else {
        return Err("the approve link points outside the site".into());
    };
    Ok(Started {
        code: field("code")?,
        poll_id: field("poll_id")?,
        approve_url,
        expires_in: v["expires_in"].as_f64().unwrap_or(600.0),
        poll_seconds: v["poll_seconds"].as_f64().unwrap_or(3.0).clamp(1.0, 30.0),
    })
}

/// Polls until approved, denied, expired or `cancel()`.
pub fn wait(server: &str, started: &Started, cancel: &dyn Fn() -> bool) -> Result<Outcome, String> {
    let until = Instant::now() + Duration::from_secs_f64(started.expires_in + 5.0);
    let mut failures = 0;
    while Instant::now() < until {
        let next = Instant::now() + Duration::from_secs_f64(started.poll_seconds);
        while Instant::now() < next {
            if cancel() {
                return Ok(Outcome::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        match post(server, "/pair/poll", &json!({"poll_id": started.poll_id})) {
            Ok(v) => {
                failures = 0;
                match v["status"].as_str().unwrap_or("") {
                    "approved" => {
                        let token = v["token"].as_str().unwrap_or("").trim().to_string();
                        if token.starts_with("mkw_") {
                            return Ok(Outcome::Approved(token));
                        }
                        return Err("approved, but the site sent no token".into());
                    }
                    "denied" => return Ok(Outcome::Denied),
                    "expired" => return Ok(Outcome::Expired),
                    _ => {}
                }
            }
            Err(e) => {
                failures += 1;
                if failures >= 10 {
                    return Err(e);
                }
            }
        }
    }
    Ok(Outcome::Expired)
}
