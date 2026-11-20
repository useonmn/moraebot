//! Frames of the bridge link and their line codec (`docs/bridge.md`).
//!
//! One frame is one UTF-8 JSON object on one line ended by `\n`, tagged by its string `t`.
//! Unknown fields are ignored. A frame with an unknown `t` decodes to
//! [`Decoded::Unknown`], so the receiver can answer `E_BRIDGE_UNSUPPORTED` when it
//! carries an `id`. Every core request carries an `id`; every `ok`, `err` and `progress`
//! frame names the request it belongs to in `re`. Line order is never used to pair them.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::adapter::{
    E_ADAPTER_DM, E_ADAPTER_EDIT, E_ADAPTER_FILES, E_ADAPTER_SEND, E_ADAPTER_THREAD,
    E_ADAPTER_TYPING, E_ADAPTER_UPLOAD, E_ADAPTER_UPLOAD_DENIED,
};
use crate::files::{
    E_FILE_DENIED, E_FILE_FETCH, E_FILE_HOST, E_FILE_INCOMPLETE, E_FILE_PATH, E_FILE_REMOTE,
    E_FILE_TOO_LARGE, E_FILE_WRITE,
};

/// The contract version both sides send in `hello` and `hello_ok`.
pub const PROTO_VERSION: u32 = 1;
/// Longest line in bytes, without its `\n`. A longer line is a protocol error.
pub const MAX_LINE: usize = 1_048_576;

/// A string that may hold message text or a signed URL; never shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Redacted(pub String);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} bytes>", self.0.len())
    }
}

impl From<&str> for Redacted {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// How the bridge's messenger makes threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadsCap {
    /// The root message id is the thread id; no `thread` request is needed.
    Implicit,
    /// A thread is made with a `thread` request.
    Create,
    /// No threads; an unknown value counts as this too.
    #[default]
    #[serde(other)]
    None,
}

/// What the bridge can do. Missing keys mean "no"; unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    pub threads: ThreadsCap,
    pub dm: bool,
    pub edit: bool,
    pub files: bool,
    pub status: bool,
    pub reactions: bool,
    pub agent_mode: bool,
}

/// One attachment of an inbound message, and the file a `fetch` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireAttachment {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub mime: Option<String>,
    /// Opaque to the core: sent back unchanged in `fetch`, never logged.
    #[serde(rename = "ref")]
    pub reference: Redacted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    Start,
    Stop,
}

/// Every frame of version 1. The variant name in snake case is the `t` value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    // Handshake and link control.
    Hello {
        proto: u32,
        bridge: String,
        version: String,
        platform: String,
        message_limit: usize,
        hints: String,
        #[serde(default)]
        home: Option<String>,
        #[serde(default)]
        caps: Caps,
        #[serde(default)]
        spooled: u64,
    },
    HelloOk {
        proto: u32,
        core: String,
        owner_id: String,
        #[serde(default)]
        allow: Vec<String>,
        window: u32,
    },
    Bye {
        code: String,
        #[serde(default)]
        message: String,
    },
    Ping {
        n: u64,
    },
    Pong {
        n: u64,
    },
    Event {
        name: String,
    },

    // Inbound messages.
    Message {
        seq: u64,
        message_id: String,
        author_id: String,
        #[serde(default)]
        author_is_bot: bool,
        channel_id: String,
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        is_dm: bool,
        text: Redacted,
        #[serde(default)]
        attachments: Vec<WireAttachment>,
        #[serde(default)]
        redelivery: bool,
    },
    Ack {
        seq: u64,
    },

    // Core requests.
    Send {
        id: u64,
        target: String,
        text: Redacted,
    },
    Edit {
        id: u64,
        target: String,
        message_id: String,
        text: Redacted,
    },
    Thread {
        id: u64,
        channel_id: String,
        from_message_id: String,
        name: Redacted,
    },
    Dm {
        id: u64,
        user_id: String,
    },
    Status {
        id: u64,
        target: String,
        state: StatusState,
    },
    File {
        id: u64,
        target: String,
        path: PathBuf,
        name: String,
    },
    Fetch {
        id: u64,
        attachment: WireAttachment,
        dest: PathBuf,
        max_bytes: Option<u64>,
    },
    React {
        id: u64,
        target: String,
        message_id: String,
        emoji: String,
    },
    Cancel {
        id: u64,
    },

    // Bridge answers. `re` is the `id` of the request.
    Ok {
        re: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
    },
    Err {
        re: u64,
        code: String,
        #[serde(default)]
        message: String,
    },
    Progress {
        re: u64,
        bytes: u64,
    },
}

impl Frame {
    /// The `t` value, the only part of a frame that logs may show.
    pub fn kind(&self) -> &'static str {
        match self {
            Frame::Hello { .. } => "hello",
            Frame::HelloOk { .. } => "hello_ok",
            Frame::Bye { .. } => "bye",
            Frame::Ping { .. } => "ping",
            Frame::Pong { .. } => "pong",
            Frame::Event { .. } => "event",
            Frame::Message { .. } => "message",
            Frame::Ack { .. } => "ack",
            Frame::Send { .. } => "send",
            Frame::Edit { .. } => "edit",
            Frame::Thread { .. } => "thread",
            Frame::Dm { .. } => "dm",
            Frame::Status { .. } => "status",
            Frame::File { .. } => "file",
            Frame::Fetch { .. } => "fetch",
            Frame::React { .. } => "react",
            Frame::Cancel { .. } => "cancel",
            Frame::Ok { .. } => "ok",
            Frame::Err { .. } => "err",
            Frame::Progress { .. } => "progress",
        }
    }

    /// The `id` of a core request (`cancel` included), `None` for every other frame.
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Frame::Send { id, .. }
            | Frame::Edit { id, .. }
            | Frame::Thread { id, .. }
            | Frame::Dm { id, .. }
            | Frame::Status { id, .. }
            | Frame::File { id, .. }
            | Frame::Fetch { id, .. }
            | Frame::React { id, .. }
            | Frame::Cancel { id } => Some(*id),
            _ => None,
        }
    }

    /// The request id an answer (`ok`, `err`, `progress`) belongs to.
    pub fn reply_to(&self) -> Option<u64> {
        match self {
            Frame::Ok { re, .. } | Frame::Err { re, .. } | Frame::Progress { re, .. } => Some(*re),
            _ => None,
        }
    }

    /// The frame as one line, `\n` included.
    pub fn encode(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("frames always serialize");
        line.push(b'\n');
        line
    }
}

/// `t` values this version knows; a known `t` that fails to parse is a bad frame,
/// an unknown one is [`Decoded::Unknown`].
const KNOWN_TYPES: &[&str] = &[
    "hello", "hello_ok", "bye", "ping", "pong", "event", "message", "ack", "send", "edit",
    "thread", "dm", "status", "file", "fetch", "react", "cancel", "ok", "err", "progress",
];

/// One decoded line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Frame(Frame),
    /// A frame type this version does not know. With an `id` the receiver answers
    /// `E_BRIDGE_UNSUPPORTED`; without one it ignores the frame.
    Unknown {
        t: String,
        id: Option<u64>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("line longer than {MAX_LINE} bytes")]
    LineTooLong,
    #[error("line is not a JSON object with a string `t`: {0}")]
    NotAFrame(String),
    /// A known frame type with a missing or invalid field. `id` is the request id when
    /// the line had one, so the receiver can answer `E_BRIDGE_BAD_REQUEST`.
    #[error("bad `{t}` frame: {reason}")]
    BadFrame {
        t: String,
        id: Option<u64>,
        reason: String,
    },
    #[error("link i/o: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Deserialize)]
struct Envelope {
    t: String,
    #[serde(default)]
    id: Option<u64>,
}

/// Decodes one line without its `\n`. Error messages never quote the line, which may
/// hold message text.
pub fn decode_line(line: &[u8]) -> Result<Decoded, ProtoError> {
    if line.len() > MAX_LINE {
        return Err(ProtoError::LineTooLong);
    }
    let envelope: Envelope =
        serde_json::from_slice(line).map_err(|err| ProtoError::NotAFrame(describe(&err)))?;
    if !KNOWN_TYPES.contains(&envelope.t.as_str()) {
        return Ok(Decoded::Unknown {
            t: envelope.t,
            id: envelope.id,
        });
    }
    serde_json::from_slice::<Frame>(line)
        .map(Decoded::Frame)
        .map_err(|err| ProtoError::BadFrame {
            t: envelope.t,
            id: envelope.id,
            reason: describe(&err),
        })
}

/// A reason that never quotes the input: serde_json's "invalid type" and "unknown
/// variant" messages carry the offending value, which may be message text. Only
/// "missing field `name`" is passed through.
fn describe(err: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let text = err.to_string();
    match err.classify() {
        Category::Data if text.starts_with("missing field") => {
            text.split(" at line ").next().unwrap_or(&text).to_owned()
        }
        Category::Data => "a field has an invalid type or value".to_owned(),
        Category::Syntax => "invalid JSON".to_owned(),
        Category::Eof => "truncated JSON".to_owned(),
        Category::Io => "i/o error".to_owned(),
    }
}

/// Reads frames from a stream, one per line, holding at most [`MAX_LINE`] bytes of a
/// line in memory.
pub struct FrameReader<R> {
    inner: R,
    line: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            line: Vec::new(),
        }
    }

    /// The next frame, or `None` at a clean end of stream. A partial last line is
    /// dropped. A [`ProtoError::LineTooLong`] or I/O error ends the link; a bad frame
    /// does not, the reader is ready for the next line.
    pub async fn next(&mut self) -> Result<Option<Decoded>, ProtoError> {
        self.line.clear();
        loop {
            let buf = self.inner.fill_buf().await?;
            if buf.is_empty() {
                return Ok(None);
            }
            let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(end) => (&buf[..end], Some(end + 1)),
                None => (buf, None),
            };
            if self.line.len() + chunk.len() > MAX_LINE {
                return Err(ProtoError::LineTooLong);
            }
            self.line.extend_from_slice(chunk);
            let used = done.unwrap_or(buf.len());
            self.inner.consume(used);
            if done.is_some() {
                return decode_line(&self.line).map(Some);
            }
        }
    }
}

/// Writes one frame as one line and flushes it.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> std::io::Result<()> {
    writer.write_all(&frame.encode()).await?;
    writer.flush().await
}

/// The core request a bridge `err` answers, which decides the code the core reports
/// when the bridge sent one the core does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Send,
    Edit,
    Thread,
    Dm,
    Status,
    Fetch,
    File,
    React,
}

impl RequestKind {
    /// The code of a plain failure of this request.
    pub fn default_code(self) -> &'static str {
        match self {
            RequestKind::Send | RequestKind::React => E_ADAPTER_SEND,
            RequestKind::Edit => E_ADAPTER_EDIT,
            RequestKind::Thread => E_ADAPTER_THREAD,
            RequestKind::Dm => E_ADAPTER_DM,
            RequestKind::Status => E_ADAPTER_TYPING,
            RequestKind::Fetch => E_FILE_FETCH,
            RequestKind::File => E_ADAPTER_UPLOAD,
        }
    }
}

impl RequestKind {
    /// The codes a bridge `err` answering this request may carry as they are: the
    /// request's own adapter code, the upload codes for `file` and the per-download file
    /// codes for `fetch`. Link codes, codes the core makes itself and codes of other
    /// operations never pass, so a confused bridge cannot steer the dispatcher's
    /// branches (`E_ADAPTER_UPLOAD_DENIED`, `E_FILE_*`) from the wrong operation.
    fn passed_codes(self) -> &'static [&'static str] {
        match self {
            RequestKind::Send | RequestKind::React => &[E_ADAPTER_SEND],
            RequestKind::Edit => &[E_ADAPTER_EDIT],
            RequestKind::Thread => &[E_ADAPTER_THREAD],
            RequestKind::Dm => &[E_ADAPTER_DM],
            RequestKind::Status => &[E_ADAPTER_TYPING],
            RequestKind::File => &[E_ADAPTER_UPLOAD, E_ADAPTER_UPLOAD_DENIED, E_ADAPTER_FILES],
            RequestKind::Fetch => &[
                E_ADAPTER_FILES,
                E_FILE_FETCH,
                E_FILE_HOST,
                E_FILE_REMOTE,
                E_FILE_WRITE,
                E_FILE_PATH,
                E_FILE_INCOMPLETE,
                E_FILE_DENIED,
                E_FILE_TOO_LARGE,
            ],
        }
    }
}

/// The code and message the core reports for an `err` answering a `kind` request.
/// A code of the request's family ([`RequestKind::passed_codes`]) passes through; any
/// other becomes the request's default code, with the original code kept at the front
/// of the message.
pub fn map_error(kind: RequestKind, code: &str, message: &str) -> (&'static str, String) {
    match kind.passed_codes().iter().find(|known| **known == code) {
        Some(known) => (known, message.to_owned()),
        None => (kind.default_code(), format!("{code}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example lines of `docs/bridge.md`, one per frame type.
    const EXAMPLES: &[&str] = &[
        r#"{"t":"hello","proto":1,"bridge":"moraebot-slack-bridge","version":"0.9.0","platform":"slack","message_limit":3900,"hints":"Your replies are posted to Slack. ...","home":"C222BBBB","caps":{"threads":"implicit","dm":true,"edit":true,"files":true,"status":true,"reactions":false,"agent_mode":true},"spooled":3}"#,
        r#"{"t":"hello_ok","proto":1,"core":"0.9.0","owner_id":"U111AAAA","allow":["U333CCCC","B444DDDD"],"window":32}"#,
        r#"{"t":"bye","code":"E_BRIDGE_BUSY","message":"another core is connected"}"#,
        r#"{"t":"message","seq":41,"message_id":"D111EEEE:1700000000.000100","author_id":"U111AAAA","author_is_bot":false,"channel_id":"D111EEEE","thread_id":"D111EEEE:1700000000.000100","is_dm":true,"text":"hello","attachments":[{"name":"report.pdf","size":48213,"mime":"application/pdf","ref":"https://files.slack.com/files-pri/T111-F111/download/report.pdf"}],"redelivery":false}"#,
        r#"{"t":"ack","seq":41}"#,
        r#"{"t":"send","id":7,"target":"D111EEEE:1700000000.000100","text":"done"}"#,
        r#"{"t":"ok","re":7,"message_id":"D111EEEE:1700000002.000200"}"#,
        r#"{"t":"edit","id":8,"target":"C222BBBB:1700000000.000100","message_id":"C222BBBB:1700000003.000300","text":"[lane 2] halfway"}"#,
        r#"{"t":"ok","re":8}"#,
        r#"{"t":"thread","id":9,"channel_id":"C222BBBB","from_message_id":"C222BBBB:1700000000.000100","name":"fix the build"}"#,
        r#"{"t":"ok","re":9,"thread_id":"C222BBBB:1700000000.000100"}"#,
        r#"{"t":"dm","id":10,"user_id":"U111AAAA"}"#,
        r#"{"t":"ok","re":10,"target":"U111AAAA"}"#,
        r#"{"t":"status","id":11,"target":"D111EEEE:1700000000.000100","state":"start"}"#,
        r#"{"t":"status","id":12,"target":"D111EEEE:1700000000.000100","state":"stop"}"#,
        r#"{"t":"file","id":14,"target":"C222BBBB:1700000000.000100","path":"/abs/work/out/report.pdf","name":"report.pdf"}"#,
        r#"{"t":"ok","re":14,"message_id":"F111"}"#,
        r#"{"t":"react","id":15,"target":"C222BBBB:1700000000.000100","message_id":"C222BBBB:1700000000.000100","emoji":"eyes"}"#,
        r#"{"t":"err","re":15,"code":"E_BRIDGE_UNSUPPORTED","message":"reactions are not supported"}"#,
        r#"{"t":"fetch","id":13,"attachment":{"name":"report.pdf","size":48213,"mime":"application/pdf","ref":"https://files.slack.com/files-pri/T111-F111/download/report.pdf"},"dest":"/abs/work/.moraebot-inbox/slack-th-D111EEEE-1700000000.000100/12/report.pdf","max_bytes":null}"#,
        r#"{"t":"progress","re":13,"bytes":32768}"#,
        r#"{"t":"ok","re":13,"bytes":48213}"#,
        r#"{"t":"cancel","id":13}"#,
        r#"{"t":"event","name":"reconnected"}"#,
        r#"{"t":"ping","n":1}"#,
        r#"{"t":"pong","n":1}"#,
    ];

    fn frame(line: &str) -> Frame {
        match decode_line(line.as_bytes()).expect("example decodes") {
            Decoded::Frame(frame) => frame,
            other => panic!("not a known frame: {other:?}"),
        }
    }

    #[test]
    fn every_example_round_trips_with_the_same_fields() {
        let mut seen = std::collections::BTreeSet::new();
        for line in EXAMPLES {
            let parsed = frame(line);
            seen.insert(parsed.kind());
            let written = parsed.encode();
            assert_eq!(written.last(), Some(&b'\n'));
            let back: serde_json::Value =
                serde_json::from_slice(&written[..written.len() - 1]).expect("valid JSON");
            let original: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(back, original, "{line}");
            assert_eq!(
                frame(std::str::from_utf8(&written).unwrap().trim_end()),
                parsed
            );
        }
        let all: std::collections::BTreeSet<&str> = KNOWN_TYPES.iter().copied().collect();
        assert_eq!(seen, all, "every frame type has an example");
    }

    #[test]
    fn decoded_fields_match_the_example() {
        let Frame::Hello { caps, home, .. } = frame(EXAMPLES[0]) else {
            panic!("hello");
        };
        assert_eq!(caps.threads, ThreadsCap::Implicit);
        assert!(caps.agent_mode && !caps.reactions);
        assert_eq!(home.as_deref(), Some("C222BBBB"));
        let fetch = frame(EXAMPLES[19]);
        assert_eq!(fetch.request_id(), Some(13));
        let Frame::Fetch {
            attachment,
            max_bytes,
            ..
        } = fetch
        else {
            panic!("fetch");
        };
        assert_eq!(max_bytes, None);
        assert!(
            attachment
                .reference
                .0
                .starts_with("https://files.slack.com/")
        );
        assert_eq!(frame(EXAMPLES[20]).reply_to(), Some(13));
    }

    #[test]
    fn unknown_fields_and_values_are_ignored() {
        let parsed = frame(
            r#"{"t":"hello","proto":1,"bridge":"b","version":"1","platform":"x","message_limit":10,"hints":"","caps":{"threads":"forum","polls":true},"later":{"a":1}}"#,
        );
        let Frame::Hello { caps, spooled, .. } = parsed else {
            panic!("hello");
        };
        assert_eq!(caps.threads, ThreadsCap::None);
        assert!(!caps.dm);
        assert_eq!(spooled, 0);
    }

    #[test]
    fn unknown_type_keeps_its_id() {
        assert_eq!(
            decode_line(br#"{"t":"poll","id":4,"q":"?"}"#).unwrap(),
            Decoded::Unknown {
                t: "poll".into(),
                id: Some(4)
            }
        );
        assert_eq!(
            decode_line(br#"{"t":"notice"}"#).unwrap(),
            Decoded::Unknown {
                t: "notice".into(),
                id: None
            }
        );
    }

    #[test]
    fn bad_frames_name_the_request_but_never_echo_the_line() {
        let err = decode_line(br#"{"t":"send","id":5,"text":"secret words"}"#).unwrap_err();
        let ProtoError::BadFrame { t, id, reason } = &err else {
            panic!("bad frame: {err:?}");
        };
        assert_eq!((t.as_str(), *id), ("send", Some(5)));
        assert!(reason.contains("target"), "{reason}");
        assert!(!err.to_string().contains("secret"));
        let err =
            decode_line(br#"{"t":"send","id":5,"target":["secret"],"text":"x"}"#).unwrap_err();
        assert!(matches!(err, ProtoError::BadFrame { .. }));
        assert!(!err.to_string().contains("secret"), "{err}");
        let err =
            decode_line(br#"{"t":"status","id":6,"target":"x","state":"secret"}"#).unwrap_err();
        assert!(!err.to_string().contains("secret"), "{err}");
        assert!(matches!(
            decode_line(b"not json secret").unwrap_err(),
            ProtoError::NotAFrame(reason) if !reason.contains("secret")
        ));
        assert!(matches!(
            decode_line(br#"{"id":1}"#).unwrap_err(),
            ProtoError::NotAFrame(_)
        ));
        assert!(matches!(
            decode_line(br#"{"t":"secret\u0000""#).unwrap_err(),
            ProtoError::NotAFrame(reason) if !reason.contains("secret")
        ));
    }

    #[test]
    fn debug_hides_text_and_ref() {
        let message = frame(EXAMPLES[3]);
        let shown = format!("{message:?}");
        assert!(!shown.contains("hello\""), "{shown}");
        assert!(!shown.contains("files.slack.com"), "{shown}");
        assert!(shown.contains("D111EEEE"), "{shown}");
        for line in [EXAMPLES[5], EXAMPLES[7], EXAMPLES[9]] {
            let shown = format!("{:?}", frame(line));
            assert!(
                !shown.contains("done") && !shown.contains("halfway"),
                "{shown}"
            );
            assert!(!shown.contains("fix the build"), "{shown}");
        }
        assert!(!format!("{:?}", frame(EXAMPLES[19])).contains("files.slack.com"));
    }

    #[test]
    fn codes_map_to_the_request_default_when_unknown() {
        assert_eq!(
            map_error(RequestKind::Fetch, E_FILE_TOO_LARGE, "too big"),
            (E_FILE_TOO_LARGE, "too big".to_owned())
        );
        assert_eq!(
            map_error(RequestKind::File, E_ADAPTER_UPLOAD_DENIED, "no scope"),
            (E_ADAPTER_UPLOAD_DENIED, "no scope".to_owned())
        );
        assert_eq!(
            map_error(RequestKind::Send, "E_SLACK_API", "channel_not_found"),
            (E_ADAPTER_SEND, "E_SLACK_API: channel_not_found".to_owned())
        );
        for (kind, code) in [
            (RequestKind::Edit, E_ADAPTER_EDIT),
            (RequestKind::Thread, E_ADAPTER_THREAD),
            (RequestKind::Dm, E_ADAPTER_DM),
            (RequestKind::Status, E_ADAPTER_TYPING),
            (RequestKind::Fetch, E_FILE_FETCH),
            (RequestKind::File, E_ADAPTER_UPLOAD),
        ] {
            assert_eq!(map_error(kind, "E_NEW", "m").0, code);
        }
    }

    #[test]
    fn known_codes_of_another_operation_or_the_link_do_not_pass() {
        use crate::adapter::E_ADAPTER_START;
        use crate::files::{E_FILE_TOO_MANY, E_FILE_UPLOAD_TIMEOUT};
        // Codes the dispatcher branches on, from the wrong operation.
        for (kind, code) in [
            (RequestKind::Send, E_ADAPTER_UPLOAD_DENIED),
            (RequestKind::Send, E_FILE_DENIED),
            (RequestKind::Send, E_ADAPTER_START),
            (RequestKind::Edit, E_ADAPTER_SEND),
            (RequestKind::Fetch, E_ADAPTER_UPLOAD_DENIED),
            (RequestKind::Fetch, E_FILE_UPLOAD_TIMEOUT),
            (RequestKind::Fetch, E_FILE_TOO_MANY),
            (RequestKind::File, E_FILE_TOO_LARGE),
            (RequestKind::File, E_FILE_DENIED),
        ] {
            assert_eq!(
                map_error(kind, code, "m"),
                (kind.default_code(), format!("{code}: m")),
                "{kind:?} {code}"
            );
        }
        // Link codes, the core-made ones included, never pass.
        for code in [
            super::super::E_BRIDGE_DOWN,
            super::super::E_BRIDGE_PLATFORM,
            super::super::E_BRIDGE_UNSUPPORTED,
            super::super::E_BRIDGE_BAD_REQUEST,
        ] {
            assert_eq!(map_error(RequestKind::Send, code, "m").0, E_ADAPTER_SEND);
        }
        assert_eq!(
            map_error(RequestKind::Fetch, E_FILE_DENIED, "login page"),
            (E_FILE_DENIED, "login page".to_owned())
        );
    }

    async fn read_all(input: Vec<u8>) -> Vec<Result<Decoded, String>> {
        let mut reader = FrameReader::new(tokio::io::BufReader::with_capacity(
            4096,
            std::io::Cursor::new(input),
        ));
        let mut out = Vec::new();
        loop {
            match reader.next().await {
                Ok(Some(decoded)) => out.push(Ok(decoded)),
                Ok(None) => return out,
                Err(err @ ProtoError::LineTooLong) => {
                    out.push(Err(err.to_string()));
                    return out;
                }
                Err(err) => out.push(Err(err.to_string())),
            }
        }
    }

    #[tokio::test]
    async fn reader_splits_lines_and_recovers_from_a_bad_frame() {
        let mut input = Vec::new();
        input.extend(frame(EXAMPLES[24]).encode());
        input.extend_from_slice(b"{\"t\":\"ack\"}\n");
        input.extend(frame(EXAMPLES[4]).encode());
        input.extend_from_slice(b"{\"t\":\"ping\",\"n\":9");
        let out = read_all(input).await;
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0], Ok(Decoded::Frame(frame(EXAMPLES[24]))));
        assert!(out[1].is_err());
        assert_eq!(out[2], Ok(Decoded::Frame(frame(EXAMPLES[4]))));
    }

    #[tokio::test]
    async fn a_line_over_the_limit_is_an_error() {
        let text = "x".repeat(MAX_LINE);
        let long = format!(r#"{{"t":"send","id":1,"target":"c","text":"{text}"}}"#);
        assert!(matches!(
            decode_line(long.as_bytes()),
            Err(ProtoError::LineTooLong)
        ));
        let mut input = long.into_bytes();
        input.push(b'\n');
        input.extend(frame(EXAMPLES[24]).encode());
        let out = read_all(input).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].as_ref().unwrap_err().contains("longer than"));

        let wrapper = r#"{"t":"send","id":1,"target":"c","text":""}"#.len();
        let fits = format!(
            r#"{{"t":"send","id":1,"target":"c","text":"{}"}}"#,
            "y".repeat(MAX_LINE - wrapper)
        );
        assert_eq!(fits.len(), MAX_LINE);
        let mut input = fits.into_bytes();
        input.push(b'\n');
        let out = read_all(input).await;
        assert!(matches!(
            out.as_slice(),
            [Ok(Decoded::Frame(Frame::Send { .. }))]
        ));
    }

    #[tokio::test]
    async fn write_frame_writes_one_line() {
        let mut out = Vec::new();
        write_frame(&mut out, &frame(EXAMPLES[4])).await.unwrap();
        assert_eq!(out, b"{\"t\":\"ack\",\"seq\":41}\n");
    }
}
