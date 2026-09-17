//! HTTP client for the TypeSafe.ai System One endpoint.
//!
//! See <https://docs.typesafe.ai/api> for the request and response contract.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use ureq::Agent;

use crate::config::{Criteria, Rule};

/// Where requests are sent. Override with `COMMENTLINT_API_URL` for testing.
pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

const MAX_ATTEMPTS: u32 = 5;

/// Locates the API key file per the XDG base directory convention.
pub fn key_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("commentlint").join("key.txt"))
}

/// Reads the API key from the key file. The `COMMENTLINT_API_KEY` environment
/// variable, when set, takes precedence so CI jobs need not write a file.
pub fn load_key() -> Result<String> {
    if let Ok(k) = std::env::var("COMMENTLINT_API_KEY")
        && !k.trim().is_empty()
    {
        return Ok(k.trim().to_string());
    }
    let path = key_path()?;
    let key = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "TypeSafe.ai API key not found at {}. Create the file with your key, or set COMMENTLINT_API_KEY.",
            path.display()
        )
    })?;
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("TypeSafe.ai API key file {} is empty", path.display());
    }
    Ok(key)
}

#[derive(Serialize)]
struct Request<'a> {
    state: &'a str,
    model: &'a str,
    questions: BTreeMap<&'a str, Question<'a>>,
}

#[derive(Serialize)]
struct Question<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    criteria: &'a Criteria,
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    noul: Option<f64>,
}

/// A client bound to one API key.
#[derive(Clone)]
pub struct Client {
    agent: Agent,
    endpoint: String,
    key: String,
}

impl Client {
    pub fn new(key: String) -> Self {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(concat!("commentlint/", env!("CARGO_PKG_VERSION")))
            .build();
        let endpoint = std::env::var("COMMENTLINT_API_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        Self {
            agent: Agent::new_with_config(config),
            endpoint,
            key,
        }
    }

    /// Evaluates every rule against one piece of text. Returns the probability
    /// of "yes" for each rule id.
    pub fn evaluate(&self, model: &str, text: &str, rules: &BTreeMap<String, Rule>) -> Result<BTreeMap<String, f64>> {
        let questions = rules
            .iter()
            .map(|(id, r)| {
                (
                    id.as_str(),
                    Question {
                        kind: "noul",
                        instructions: &r.instructions,
                        criteria: &r.criteria,
                    },
                )
            })
            .collect();
        let body = Request {
            state: text,
            model,
            questions,
        };

        let mut delay = Duration::from_millis(500);
        for attempt in 1..=MAX_ATTEMPTS {
            let result = self
                .agent
                .post(&self.endpoint)
                .header("Authorization", &format!("Bearer {}", self.key))
                .send_json(&body);
            let mut resp = match result {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    thread::sleep(delay);
                    delay *= 2;
                    let _ = e;
                    continue;
                }
                Err(e) => return Err(anyhow!(e)).context("request to TypeSafe.ai failed"),
            };
            let status = resp.status().as_u16();
            match status {
                200 => {
                    let parsed: Response = resp
                        .body_mut()
                        .read_json()
                        .context("malformed response from TypeSafe.ai")?;
                    let mut out = BTreeMap::new();
                    for id in rules.keys() {
                        let noul = parsed
                            .answers
                            .get(id)
                            .and_then(|a| a.noul)
                            .with_context(|| format!("TypeSafe.ai returned no noul answer for rule `{id}`"))?;
                        out.insert(id.clone(), noul);
                    }
                    return Ok(out);
                }
                429 | 529 | 500..=504 if attempt < MAX_ATTEMPTS => {
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(delay);
                    thread::sleep(wait);
                    delay *= 2;
                }
                401 => bail!("TypeSafe.ai rejected the API key (401 Unauthorized)"),
                _ => {
                    let text = resp.body_mut().read_to_string().unwrap_or_default();
                    bail!("TypeSafe.ai returned HTTP {status}: {}", text.trim());
                }
            }
        }
        bail!("TypeSafe.ai request gave up after {MAX_ATTEMPTS} attempts")
    }
}
