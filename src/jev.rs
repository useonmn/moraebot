//! The jev client: one `POST /v1/systemone` per call to TypeSafe System One.
//!
//! The client never sleeps and never retries. A failed call closes a gate until a retry
//! time, kept with an injected clock; the caller (the observer) simply tries again later.
//! After `breaker_failures` transient failures in a row the breaker opens: one probe per
//! `breaker_probe_seconds` is let through until a call succeeds. Auth and request errors
//! pause the client until restart, because every later call would fail the same way.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{JevConfig, Secret};

/// No key, or the key was refused (401, 403). Permanent.
pub const E_JEV_AUTH: &str = "E_JEV_AUTH";
/// Any other 4xx except 408 and 429, or a redirect: the request shape, the model or the URL
/// is wrong. Permanent.
pub const E_JEV_REQUEST: &str = "E_JEV_REQUEST";
/// 408, 429 or 5xx. Transient.
pub const W_JEV_BUSY: &str = "W_JEV_BUSY";
/// Connect error, timeout or body read error. Transient.
pub const W_JEV_NETWORK: &str = "W_JEV_NETWORK";
/// A success status whose body does not decode or misses an answer. Transient.
pub const E_JEV_RESPONSE: &str = "E_JEV_RESPONSE";

const USER_AGENT: &str = concat!("moraebot/", env!("CARGO_PKG_VERSION"));
const PATH: &str = "/v1/systemone";

/// The clock, in unix seconds. Tests pass a manual one.
pub type Now = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_now() -> Now {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// One System One request. `questions` and each choice's options keep their order on the
/// wire (choice answers can depend on option order).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub state: Value,
    pub questions: Vec<(String, Question)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// A yes/no question; `yes` and `no` become the `true` and `false` criteria.
    Noul {
        instructions: String,
        yes: String,
        no: String,
    },
    /// A pick among named options; each option's value is its description.
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
}

/// Serializes `(key, value)` pairs as a JSON object in vector order.
struct Ordered<'a, T>(&'a [(String, T)]);

impl<T: Serialize> Serialize for Ordered<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(key, value)| (key, value)))
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("state", &self.state)?;
        map.serialize_entry("questions", &Ordered(&self.questions))?;
        map.end()
    }
}

impl Serialize for Question {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        match self {
            Question::Noul {
                instructions,
                yes,
                no,
            } => {
                map.serialize_entry("type", "noul")?;
                map.serialize_entry("instructions", instructions)?;
                let criteria = [("true".to_owned(), yes), ("false".to_owned(), no)];
                map.serialize_entry("criteria", &Ordered(&criteria))?;
            }
            Question::Choice {
                instructions,
                options,
            } => {
                map.serialize_entry("type", "choice")?;
                map.serialize_entry("instructions", instructions)?;
                map.serialize_entry("criteria", &Ordered(options))?;
            }
        }
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Response {
    /// The versioned model id that answered.
    pub model: String,
    pub answers: HashMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        probabilities: HashMap<String, f64>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevError {
    /// `memory.jev.enabled` resolved to false.
    Off,
    /// Paused by an earlier permanent error; nothing was sent.
    Paused { code: &'static str },
    /// The gate is closed, or another call is the breaker's probe; nothing was sent.
    NotYet { retry_at: i64 },
    /// This call caused the pause.
    Permanent {
        code: &'static str,
        status: Option<u16>,
    },
    Transient {
        code: &'static str,
        status: Option<u16>,
        retry_at: i64,
    },
}

impl fmt::Display for JevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JevError::Off => f.write_str("jev is off"),
            JevError::Paused { code } => write!(f, "{code}: jev is paused until restart"),
            JevError::NotYet { retry_at } => write!(f, "jev waits until {retry_at}"),
            JevError::Permanent { code, status } => {
                write!(f, "{code}: jev refused the call (status {status:?})")
            }
            JevError::Transient {
                code,
                status,
                retry_at,
            } => write!(
                f,
                "{code}: jev call failed (status {status:?}), retry at {retry_at}"
            ),
        }
    }
}

impl std::error::Error for JevError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevStatus {
    Off,
    Ready,
    /// Backing off after a transient failure.
    Waiting {
        until: i64,
    },
    /// The breaker is open; the next probe is allowed at `until`.
    Open {
        until: i64,
    },
    Paused {
        code: &'static str,
    },
}

#[derive(Debug, Default)]
struct Gate {
    failures: u32,
    retry_at: i64,
    probing: bool,
    paused: Option<&'static str>,
}

pub struct Jev {
    config: JevConfig,
    enabled: bool,
    key: Option<Secret>,
    now: Now,
    client: reqwest::Client,
    gate: Mutex<Gate>,
}

impl fmt::Debug for Jev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jev")
            .field("base_url", &self.config.base_url)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// How the outcome of one sent call changes the gate.
enum Outcome {
    Success(Response),
    Permanent(&'static str, Option<u16>),
    /// Code, status, and the server's requested wait in seconds.
    Transient(&'static str, Option<u16>, Option<i64>),
    /// The call's own timeout ran out (the status when the head had arrived).
    TimedOut(Option<u16>),
}

impl Jev {
    /// Reads the key once. Enabled without a key starts paused with `E_JEV_AUTH`.
    pub fn new(config: JevConfig, now: Now) -> Self {
        let key = config.api_key();
        let enabled = config.enabled.unwrap_or(key.is_some());
        let gate = Gate {
            paused: (enabled && key.is_none()).then_some(E_JEV_AUTH),
            ..Gate::default()
        };
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the TLS backend initializes");
        Self {
            config,
            enabled,
            key,
            now,
            client,
            gate: Mutex::new(gate),
        }
    }

    pub fn config(&self) -> &JevConfig {
        &self.config
    }

    pub fn status(&self) -> JevStatus {
        if !self.enabled {
            return JevStatus::Off;
        }
        let gate = self.lock();
        if let Some(code) = gate.paused {
            return JevStatus::Paused { code };
        }
        if gate.failures >= self.config.breaker_failures {
            return JevStatus::Open {
                until: gate.retry_at,
            };
        }
        if (self.now)() < gate.retry_at {
            return JevStatus::Waiting {
                until: gate.retry_at,
            };
        }
        JevStatus::Ready
    }

    /// Sends one request when the gate allows it. Never sleeps and never retries.
    pub async fn ask(&self, request: &Request, timeout: Duration) -> Result<Response, JevError> {
        self.ask_counting(request, timeout, true).await
    }

    /// Like [`Jev::ask`], for a caller that gives up early (the memory request classifier):
    /// when `timeout` runs out the call counts as no answer and leaves the gate as it was, so
    /// a jev that answers after the short timeout but within the observer's never closes the
    /// gate for the observer. Every other failure settles the gate as in [`Jev::ask`].
    pub async fn ask_quick(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<Response, JevError> {
        self.ask_counting(request, timeout, false).await
    }

    async fn ask_counting(
        &self,
        request: &Request,
        timeout: Duration,
        count_timeout: bool,
    ) -> Result<Response, JevError> {
        if !self.enabled {
            return Err(JevError::Off);
        }
        self.enter()?;
        let Some(key) = &self.key else {
            return Err(JevError::Paused { code: E_JEV_AUTH });
        };
        let outcome = self.send(request, key, timeout).await;
        match outcome {
            Outcome::Success(response) => {
                *self.lock() = Gate::default();
                Ok(response)
            }
            Outcome::TimedOut(status) if !count_timeout => {
                let mut gate = self.lock();
                gate.probing = false;
                Err(JevError::Transient {
                    code: W_JEV_NETWORK,
                    status,
                    retry_at: gate.retry_at,
                })
            }
            failure => Err(self.settle(failure)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Gate> {
        self.gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Checks the gate and, when the breaker is open, claims the single probe.
    fn enter(&self) -> Result<(), JevError> {
        let now = (self.now)();
        let mut gate = self.lock();
        if let Some(code) = gate.paused {
            return Err(JevError::Paused { code });
        }
        if now < gate.retry_at {
            return Err(JevError::NotYet {
                retry_at: gate.retry_at,
            });
        }
        if gate.failures >= self.config.breaker_failures {
            if gate.probing {
                return Err(JevError::NotYet {
                    retry_at: gate.retry_at.max(now),
                });
            }
            gate.probing = true;
        }
        Ok(())
    }

    fn settle(&self, outcome: Outcome) -> JevError {
        let now = (self.now)();
        let mut gate = self.lock();
        gate.probing = false;
        match outcome {
            Outcome::Success(_) => unreachable!("successes never settle as failures"),
            Outcome::Permanent(code, status) => {
                gate.paused = Some(code);
                JevError::Permanent { code, status }
            }
            Outcome::TimedOut(status) => {
                self.transient(&mut gate, now, W_JEV_NETWORK, status, None)
            }
            Outcome::Transient(code, status, asked) => {
                self.transient(&mut gate, now, code, status, asked)
            }
        }
    }

    /// Counts one transient failure and sets the next retry time.
    fn transient(
        &self,
        gate: &mut Gate,
        now: i64,
        code: &'static str,
        status: Option<u16>,
        asked: Option<i64>,
    ) -> JevError {
        gate.failures = gate.failures.saturating_add(1);
        let delay = self.delay(gate.failures, asked);
        gate.retry_at = now.saturating_add(delay);
        JevError::Transient {
            code,
            status,
            retry_at: gate.retry_at,
        }
    }

    /// The wait after the `failures`-th transient failure in a row, in seconds.
    fn delay(&self, failures: u32, asked: Option<i64>) -> i64 {
        let config = &self.config;
        let seconds = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        let probe = seconds(config.breaker_probe_seconds);
        if failures >= config.breaker_failures {
            return probe;
        }
        let doubling = 2_i64.saturating_pow(failures.saturating_sub(1));
        let backoff = seconds(config.backoff_initial_seconds)
            .saturating_mul(doubling)
            .min(seconds(config.backoff_max_seconds));
        match asked {
            Some(asked) if asked > backoff => asked.min(probe),
            _ => backoff,
        }
    }

    async fn send(&self, request: &Request, key: &Secret, timeout: Duration) -> Outcome {
        let url = format!("{}{PATH}", self.config.base_url.trim_end_matches('/'));
        let sent = self
            .client
            .post(url)
            .bearer_auth(key.expose())
            .json(request)
            .timeout(timeout)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(err) if err.is_timeout() => return Outcome::TimedOut(None),
            Err(_) => return Outcome::Transient(W_JEV_NETWORK, None, None),
        };
        let status = response.status().as_u16();
        match status {
            200..=299 => {}
            401 | 403 => return Outcome::Permanent(E_JEV_AUTH, Some(status)),
            408 | 429 | 500..=599 => {
                let asked = retry_after(response.headers());
                return Outcome::Transient(W_JEV_BUSY, Some(status), asked);
            }
            _ => return Outcome::Permanent(E_JEV_REQUEST, Some(status)),
        }
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(err) if err.is_timeout() => return Outcome::TimedOut(Some(status)),
            Err(_) => return Outcome::Transient(W_JEV_NETWORK, Some(status), None),
        };
        match serde_json::from_slice::<Response>(&body) {
            Ok(decoded) if answers_fit(request, &decoded) => Outcome::Success(decoded),
            _ => Outcome::Transient(E_JEV_RESPONSE, Some(status), None),
        }
    }
}

/// Every question has an answer of its own type; a `noul` is within `0..=1`; a `choice`
/// names one of the options that were sent.
fn answers_fit(request: &Request, response: &Response) -> bool {
    request.questions.iter().all(
        |(name, question)| match (question, response.answers.get(name)) {
            (Question::Noul { .. }, Some(Answer::Noul { noul })) => (0.0..=1.0).contains(noul),
            (Question::Choice { options, .. }, Some(Answer::Choice { choice, .. })) => {
                options.iter().any(|(option, _)| option == choice)
            }
            _ => false,
        },
    )
}

/// `retry-after-ms`, else `Retry-After` in seconds, rounded up to whole seconds. An HTTP
/// date is ignored.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    let text = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    if let Some(ms) = text("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some((ms / 1000.0).ceil() as i64);
    }
    text("retry-after")?.parse::<i64>().ok().filter(|s| *s >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_questions_and_options_in_order() {
        let request = Request {
            model: "m".into(),
            state: serde_json::json!({"k": 1}),
            questions: vec![
                (
                    "z".into(),
                    Question::Noul {
                        instructions: "i".into(),
                        yes: "y".into(),
                        no: "n".into(),
                    },
                ),
                (
                    "a".into(),
                    Question::Choice {
                        instructions: "j".into(),
                        options: vec![("b".into(), "B".into()), ("a".into(), "A".into())],
                    },
                ),
            ],
        };
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(
            text,
            r#"{"model":"m","state":{"k":1},"questions":{"z":{"type":"noul","instructions":"i","criteria":{"true":"y","false":"n"}},"a":{"type":"choice","instructions":"j","criteria":{"b":"B","a":"A"}}}}"#
        );
    }
}
