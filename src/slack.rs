//! Slack adapter (Socket Mode): pure protocol core. Frame parsing, event mapping, id
//! encoding, text escaping and Web API response parsing. No I/O lives here.
//!
//! Id encoding: a Slack `ts` is unique only per channel, so message ids are
//! `<channel>:<ts>` and thread ids are `<channel>:<thread_ts>`.
use serde_json::{Value, json};

use crate::adapter::{AdapterError, InboundEvent};

pub const PLATFORM: &str = "slack";
/// Slack advises keeping `text` under 4,000 characters.
pub const MESSAGE_LIMIT: usize = 3900;

pub const E_SLACK_AUTH: &str = "E_SLACK_AUTH";
pub const E_SLACK_APP_TOKEN: &str = "E_SLACK_APP_TOKEN";
pub const E_SLACK_API: &str = "E_SLACK_API";
pub const E_SLACK_RATE: &str = "E_SLACK_RATE";
pub const E_SLACK_ID: &str = "E_SLACK_ID";
pub const E_SLACK_PROTOCOL: &str = "E_SLACK_PROTOCOL";

pub const DEFAULT_HINTS: &str = "Your replies are posted to Slack. Use Slack mrkdwn, not Markdown: *bold*, _italic_, ~strike~, `code`, fenced code blocks, > quotes, - lists. Double-asterisk bold, # headings and tables do not render. Write links as plain URLs. One message holds about 3900 characters; longer replies are split.";

/// One Socket Mode message received from Slack.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello,
    Disconnect { reason: String },
    Event { envelope_id: String, payload: Value },
    Other { envelope_id: Option<String> },
}

/// Who may reach the bot and where it listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub owner_id: String,
    pub channel_id: String,
    pub bot_user_id: String,
}

fn protocol_error(message: impl Into<String>) -> AdapterError {
    AdapterError {
        code: E_SLACK_PROTOCOL,
        message: message.into(),
    }
}

pub fn parse_frame(text: &str) -> Result<Frame, AdapterError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| protocol_error(format!("bad frame: {e}")))?;
    let envelope_id = value
        .get("envelope_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    match value.get("type").and_then(Value::as_str) {
        Some("hello") => Ok(Frame::Hello),
        Some("disconnect") => Ok(Frame::Disconnect {
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("events_api") => {
            let envelope_id =
                envelope_id.ok_or_else(|| protocol_error("events_api without envelope_id"))?;
            let payload = value
                .get("payload")
                .cloned()
                .ok_or_else(|| protocol_error("events_api without payload"))?;
            Ok(Frame::Event {
                envelope_id,
                payload,
            })
        }
        _ => Ok(Frame::Other { envelope_id }),
    }
}

pub fn ack_frame(envelope_id: &str) -> String {
    json!({ "envelope_id": envelope_id }).to_string()
}

/// Maps an `events_api` payload to an inbound event, or `None` when the message must be
/// dropped: wrong type, bot or own messages, edits and other subtypes, other users in a
/// DM, other channels. Every field is optional on the wire; a missing one drops the event.
pub fn map_event(payload: &Value, scope: &Scope) -> Option<InboundEvent> {
    if payload.get("type")?.as_str()? != "event_callback" {
        return None;
    }
    let event = payload.get("event")?;
    if event.get("type")?.as_str()? != "message" {
        return None;
    }
    if let Some(subtype) = event.get("subtype") {
        if subtype.as_str() != Some("thread_broadcast") {
            return None;
        }
    }
    if event.get("bot_id").is_some() {
        return None;
    }
    let user = event.get("user")?.as_str()?;
    if user == scope.bot_user_id {
        return None;
    }
    let channel = event.get("channel")?.as_str()?;
    let ts = event.get("ts")?.as_str()?;
    let is_dm = event.get("channel_type").and_then(Value::as_str) == Some("im");
    let thread_id = if is_dm {
        if user != scope.owner_id {
            return None;
        }
        None
    } else {
        if channel != scope.channel_id {
            return None;
        }
        event
            .get("thread_ts")
            .and_then(Value::as_str)
            .filter(|thread_ts| *thread_ts != ts)
            .map(|thread_ts| format!("{channel}:{thread_ts}"))
    };
    let raw_text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mention = format!("<@{}>", scope.bot_user_id);
    let text = match raw_text.strip_prefix(&mention) {
        Some(rest) => rest.trim_start(),
        None => raw_text,
    };
    Some(InboundEvent {
        message_id: format!("{channel}:{ts}"),
        author_id: user.to_string(),
        author_is_bot: false,
        channel_id: channel.to_string(),
        thread_id,
        is_dm,
        text: text.to_string(),
    })
}

/// Splits a send target: `<channel>:<ts>` posts into that thread, a plain channel or DM
/// id posts to the channel itself.
pub fn parse_target(target: &str) -> Result<(String, Option<String>), AdapterError> {
    let bad = || AdapterError {
        code: E_SLACK_ID,
        message: "send target must be <channel> or <channel>:<ts>".to_string(),
    };
    match target.split_once(':') {
        None if target.is_empty() => Err(bad()),
        None => Ok((target.to_string(), None)),
        Some((channel, ts)) if !channel.is_empty() && !ts.is_empty() => {
            Ok((channel.to_string(), Some(ts.to_string())))
        }
        Some(_) => Err(bad()),
    }
}

/// Slack text escapes only these three characters.
pub fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn post_body(channel: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut body = json!({ "channel": channel, "text": text });
    if let Some(thread_ts) = thread_ts {
        body["thread_ts"] = json!(thread_ts);
    }
    body
}

/// Checks `ok` and returns the field `key`. On `ok: false` the error carries Slack's
/// `error` string with `code`; a missing field on an ok response is a protocol error.
fn ok_field(json: &Value, key: &str, code: &'static str) -> Result<String, AdapterError> {
    if json.get("ok").and_then(Value::as_bool) != Some(true) {
        let reason = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(AdapterError {
            code,
            message: reason.to_string(),
        });
    }
    json.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| protocol_error(format!("response without {key}")))
}

/// `apps.connections.open` response -> the `wss://` URL.
pub fn parse_open_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "url", E_SLACK_APP_TOKEN)
}

/// `auth.test` response -> the bot's user id.
pub fn parse_auth_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "user_id", E_SLACK_AUTH)
}

/// `chat.postMessage` response -> the new message's `ts`.
pub fn parse_post_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "ts", E_SLACK_API)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            owner_id: "U111".into(),
            channel_id: "C222".into(),
            bot_user_id: "UBOT".into(),
        }
    }

    fn payload(event: Value) -> Value {
        json!({ "type": "event_callback", "event": event })
    }

    fn msg(user: &str, channel: &str, ts: &str) -> Value {
        json!({ "type": "message", "user": user, "channel": channel, "ts": ts, "text": "hi",
                "channel_type": "channel" })
    }

    #[test]
    fn hello_and_disconnect_parse() {
        assert_eq!(
            parse_frame(r#"{"type":"hello","num_connections":1}"#).unwrap(),
            Frame::Hello
        );
        assert_eq!(
            parse_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#).unwrap(),
            Frame::Disconnect {
                reason: "refresh_requested".into()
            }
        );
        assert_eq!(
            parse_frame(r#"{"type":"slash_commands","envelope_id":"e9"}"#).unwrap(),
            Frame::Other {
                envelope_id: Some("e9".into())
            }
        );
    }

    #[test]
    fn events_api_envelope_parses_and_acks() {
        let frame = parse_frame(
            r#"{"type":"events_api","envelope_id":"e1","payload":{"type":"event_callback"}}"#,
        )
        .unwrap();
        let Frame::Event {
            envelope_id,
            payload,
        } = frame
        else {
            panic!("expected event frame");
        };
        assert_eq!(envelope_id, "e1");
        assert_eq!(payload["type"], "event_callback");
        let ack: Value = serde_json::from_str(&ack_frame(&envelope_id)).unwrap();
        assert_eq!(ack, json!({ "envelope_id": "e1" }));
    }

    #[test]
    fn bad_json_is_protocol_error() {
        assert_eq!(parse_frame("not json").unwrap_err().code, E_SLACK_PROTOCOL);
    }

    #[test]
    fn owner_channel_message_maps_without_thread() {
        let event = map_event(&payload(msg("U111", "C222", "1.1")), &scope()).unwrap();
        assert_eq!(event.message_id, "C222:1.1");
        assert_eq!(event.author_id, "U111");
        assert!(!event.author_is_bot);
        assert_eq!(event.channel_id, "C222");
        assert_eq!(event.thread_id, None);
        assert!(!event.is_dm);
        assert_eq!(event.text, "hi");
    }

    #[test]
    fn thread_reply_maps_thread_id() {
        let mut m = msg("U111", "C222", "2.2");
        m["thread_ts"] = json!("1.1");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.thread_id.as_deref(), Some("C222:1.1"));
    }

    #[test]
    fn thread_root_with_equal_thread_ts_has_no_thread() {
        let mut m = msg("U111", "C222", "1.1");
        m["thread_ts"] = json!("1.1");
        assert_eq!(map_event(&payload(m), &scope()).unwrap().thread_id, None);
    }

    #[test]
    fn owner_dm_maps_is_dm() {
        let mut m = msg("U111", "D333", "3.3");
        m["channel_type"] = json!("im");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert!(event.is_dm);
        assert_eq!(event.thread_id, None);
        assert_eq!(event.channel_id, "D333");
    }

    #[test]
    fn other_user_dm_dropped() {
        let mut m = msg("U999", "D333", "3.3");
        m["channel_type"] = json!("im");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn other_channel_dropped() {
        assert!(map_event(&payload(msg("U111", "C777", "1.1")), &scope()).is_none());
    }

    #[test]
    fn missing_channel_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m.as_object_mut().unwrap().remove("channel");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn bot_id_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m["bot_id"] = json!("B1");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn own_bot_user_dropped() {
        assert!(map_event(&payload(msg("UBOT", "C222", "1.1")), &scope()).is_none());
    }

    #[test]
    fn subtype_message_changed_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m["subtype"] = json!("message_changed");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn thread_broadcast_kept() {
        let mut m = msg("U111", "C222", "2.2");
        m["subtype"] = json!("thread_broadcast");
        m["thread_ts"] = json!("1.1");
        assert!(map_event(&payload(m), &scope()).is_some());
    }

    #[test]
    fn leading_bot_mention_stripped() {
        let mut m = msg("U111", "C222", "1.1");
        m["text"] = json!("<@UBOT>  run it <@UBOT>");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.text, "run it <@UBOT>");
    }

    #[test]
    fn non_message_payloads_dropped() {
        let reaction = json!({ "type": "reaction_added", "user": "U111" });
        assert!(map_event(&payload(reaction), &scope()).is_none());
        assert!(map_event(&json!({ "type": "url_verification" }), &scope()).is_none());
    }

    #[test]
    fn parse_target_thread_and_plain() {
        assert_eq!(
            parse_target("C222:1.1").unwrap(),
            ("C222".to_string(), Some("1.1".to_string()))
        );
        assert_eq!(parse_target("D333").unwrap(), ("D333".to_string(), None));
        for bad in ["", ":1.1", "C222:"] {
            assert_eq!(parse_target(bad).unwrap_err().code, E_SLACK_ID);
        }
    }

    #[test]
    fn escape_only_three_chars() {
        assert_eq!(
            escape_text("a & <b> \"c\" 'd'"),
            "a &amp; &lt;b&gt; \"c\" 'd'"
        );
    }

    #[test]
    fn post_body_has_thread_ts_only_when_given() {
        assert_eq!(
            post_body("C222", Some("1.1"), "x"),
            json!({ "channel": "C222", "text": "x", "thread_ts": "1.1" })
        );
        assert_eq!(
            post_body("D333", None, "x"),
            json!({ "channel": "D333", "text": "x" })
        );
    }

    #[test]
    fn open_response_ok_and_error() {
        assert_eq!(
            parse_open_response(&json!({ "ok": true, "url": "wss://example.test/link" })).unwrap(),
            "wss://example.test/link"
        );
        let err =
            parse_open_response(&json!({ "ok": false, "error": "invalid_auth" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_APP_TOKEN);
        assert_eq!(err.message, "invalid_auth");
        let err = parse_open_response(&json!({ "ok": true })).unwrap_err();
        assert_eq!(err.code, E_SLACK_PROTOCOL);
    }

    #[test]
    fn auth_response_error_code() {
        assert_eq!(
            parse_auth_response(&json!({ "ok": true, "user_id": "UBOT" })).unwrap(),
            "UBOT"
        );
        let err =
            parse_auth_response(&json!({ "ok": false, "error": "invalid_auth" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_AUTH);
    }

    #[test]
    fn post_response_ts_and_error_code() {
        assert_eq!(
            parse_post_response(&json!({ "ok": true, "channel": "C222", "ts": "9.9" })).unwrap(),
            "9.9"
        );
        let err =
            parse_post_response(&json!({ "ok": false, "error": "channel_not_found" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_API);
    }
}
