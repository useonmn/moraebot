//! Where a memory write's words came from: claims, the approval set and the provenance stamp.

use super::frontmatter::{self, render_frontmatter};
use super::home::MemoryError;
use super::{APPROVALS, E_MEMORY_FRONTMATTER, E_MEMORY_QUOTE_MISSING, PROVENANCE_KEY};

/// The stamp of a file whose words come only partly from the owner.
const MIXED: &str = "mixed: only parts come from the owner's own words";

/// The kind words a stamp value can start with.
const STAMP_KINDS: [&str; 5] = ["user", "quoted", "agent", "inferred", "mixed"];

/// What the lead says a write is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    Agent,
    Inferred,
}

impl Source {
    /// The source named by a tool argument, if it is one.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "inferred" => Some(Self::Inferred),
            _ => None,
        }
    }

    /// The tool argument value of this source.
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Inferred => "inferred",
        }
    }
}

/// What moraebot stored after checking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Quoted,
    Agent,
    Inferred,
}

impl Kind {
    /// The stored name of this kind.
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Quoted => "quoted",
            Self::Agent => "agent",
            Self::Inferred => "inferred",
        }
    }
}

/// The claim arguments of one write tool call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimArgs {
    pub source: Option<Source>,
    pub user_quote: Option<String>,
    pub quote_message: Option<i64>,
}

/// A checked claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub kind: Kind,
    pub requested: Source,
    pub code: Option<&'static str>,
    /// The trimmed quote, at most MAX_QUOTE_CHARS characters.
    pub quote: Option<String>,
    /// The quoted row id as given.
    pub message: Option<i64>,
    /// `(platform, message_id)` of the quoted row; only for `User` and `Quoted`.
    pub receipt: Option<(String, String)>,
    /// `(row id, proposal_hash)`; only for `User`.
    pub proposal: Option<(i64, String)>,
}

impl Claim {
    /// `requested == User && kind != User`.
    pub fn downgraded(&self) -> bool {
        self.requested == Source::User && self.kind != Kind::User
    }

    /// A claim that needs no check: `agent` or `inferred` as asked. `Source::User` cannot be
    /// taken unchecked, so it gives `Kind::Agent` with `E_MEMORY_QUOTE_MISSING`.
    pub fn plain(source: Source) -> Claim {
        let (kind, code) = match source {
            Source::User => (Kind::Agent, Some(E_MEMORY_QUOTE_MISSING)),
            Source::Agent => (Kind::Agent, None),
            Source::Inferred => (Kind::Inferred, None),
        };
        Claim {
            kind,
            requested: source,
            code,
            quote: None,
            message: None,
            receipt: None,
            proposal: None,
        }
    }

    /// The `claim_json` of the commit: every key present, `null` when absent.
    pub fn to_json(&self) -> String {
        let (platform, message_id) = match &self.receipt {
            Some((p, m)) => (Some(p.as_str()), Some(m.as_str())),
            None => (None, None),
        };
        let (proposal_row, proposal_hash) = match &self.proposal {
            Some((row, hash)) => (Some(*row), Some(hash.as_str())),
            None => (None, None),
        };
        serde_json::json!({
            "v": 1,
            "source": self.kind.name(),
            "requested": self.requested.name(),
            "downgraded": self.downgraded(),
            "code": self.code,
            "quote": self.quote,
            "message": self.message,
            "platform": platform,
            "message_id": message_id,
            "proposal_row": proposal_row,
            "proposal_hash": proposal_hash,
        })
        .to_string()
    }
}

/// Whether a whole owner message approves the lead's last proposal: after trimming and dropping
/// one trailing `.` or `!`, it is one of `APPROVALS` (ASCII case ignored).
pub fn is_approval(text: &str) -> bool {
    let t = text.trim();
    let t = t
        .strip_suffix('.')
        .or_else(|| t.strip_suffix('!'))
        .unwrap_or(t);
    let t = t.to_ascii_lowercase();
    APPROVALS.contains(&t.as_str())
}

/// The first 16 lowercase hex digits of the SHA-256 of `reply`.
pub fn proposal_hash(reply: &str) -> String {
    sha256(reply.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The one-line stamp value of a claim.
pub fn stamp_value(claim: &Claim) -> String {
    match (claim.kind, claim.code) {
        (Kind::User, _) => format!(
            "user: owner message {} said {} after proposal {}; kept as a record",
            message_text(claim),
            quote_text(claim),
            claim
                .proposal
                .as_ref()
                .map_or("", |(_, hash)| hash.as_str()),
        ),
        (Kind::Quoted, _) => format!(
            "quoted: owner message {} contains {}; not an approval",
            message_text(claim),
            quote_text(claim),
        ),
        (Kind::Agent, None) => "agent: written by the lead on its own".to_string(),
        (Kind::Agent, Some(code)) => {
            format!("agent: unverified owner claim ({code}), kept as the lead's own")
        }
        (Kind::Inferred, _) => "inferred: concluded by the lead".to_string(),
    }
}

fn message_text(claim: &Claim) -> String {
    claim.message.map(|m| m.to_string()).unwrap_or_default()
}

fn quote_text(claim: &Claim) -> String {
    serde_json::to_string(claim.quote.as_deref().unwrap_or("")).unwrap_or_default()
}

/// The kind of a stored `provenance` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OldKind<'a> {
    None,
    Named(&'a str),
    Other,
}

fn old_kind(value: Option<&str>) -> OldKind<'_> {
    match value {
        None => OldKind::None,
        Some(v) => match v.split_once(':') {
            Some((word, _)) if STAMP_KINDS.contains(&word) => OldKind::Named(word),
            _ => OldKind::Other,
        },
    }
}

fn parse_err(e: frontmatter::FrontmatterError) -> MemoryError {
    MemoryError {
        code: E_MEMORY_FRONTMATTER,
        detail: e.to_string(),
    }
}

/// For a write without a claim: a file whose committed words came from the owner (`user` or
/// `quoted`) becomes `mixed`; every other text comes back byte for byte.
pub fn stamp_unclaimed(text: &str) -> Result<String, MemoryError> {
    let parsed = frontmatter::parse(text).map_err(parse_err)?;
    match old_kind(parsed.frontmatter.get(PROVENANCE_KEY)) {
        OldKind::Named("user" | "quoted") => {
            let mut fm = parsed.frontmatter;
            fm.set(PROVENANCE_KEY, MIXED);
            Ok(render_frontmatter(&fm) + parsed.body)
        }
        _ => Ok(text.to_string()),
    }
}

/// Set the `provenance` of `text` for `claim`, mixing with the old value by the stamp rules.
pub fn stamp(text: &str, claim: &Claim) -> Result<String, MemoryError> {
    let parsed = frontmatter::parse(text).map_err(parse_err)?;
    let new = claim.kind.name();
    let value = match old_kind(parsed.frontmatter.get(PROVENANCE_KEY)) {
        OldKind::None => stamp_value(claim),
        OldKind::Named(old) if old == new => stamp_value(claim),
        OldKind::Named("agent" | "inferred")
            if matches!(claim.kind, Kind::Agent | Kind::Inferred) =>
        {
            stamp_value(claim)
        }
        _ => MIXED.to_string(),
    };
    let mut fm = parsed.frontmatter;
    fm.set(PROVENANCE_KEY, &value);
    Ok(render_frontmatter(&fm) + parsed.body)
}

/// SHA-256 round constants (FIPS 180-4 section 4.2.2).
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 initial hash value (FIPS 180-4 section 5.3.3).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 of `data` (FIPS 180-4 sections 5.1.1 and 6.2).
fn sha256(data: &[u8]) -> [u8; 32] {
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut h = H0;
    let mut w = [0u32; 64];
    for block in message.as_chunks::<64>().0 {
        for (t, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[t] = u32::from_be_bytes(*word);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for t in 0..64 {
            let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(big_s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = big_s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }

    let mut out = [0u8; 32];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *chunk = word.to_be_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::E_MEMORY_QUOTE_APPROVAL;
    use serde_json::Value;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn provenance_of(text: &str) -> Option<String> {
        frontmatter::parse(text)
            .unwrap()
            .frontmatter
            .get(PROVENANCE_KEY)
            .map(str::to_string)
    }

    fn body_of(text: &str) -> String {
        frontmatter::parse(text).unwrap().body.to_string()
    }

    fn user_claim(quote: &str) -> Claim {
        Claim {
            kind: Kind::User,
            requested: Source::User,
            code: None,
            quote: Some(quote.to_string()),
            message: Some(7),
            receipt: Some(("test".to_string(), "m1".to_string())),
            proposal: Some((5, proposal_hash("proposal-text"))),
        }
    }

    fn quoted_claim() -> Claim {
        Claim {
            kind: Kind::Quoted,
            requested: Source::User,
            code: Some(E_MEMORY_QUOTE_APPROVAL),
            quote: Some("green tea".to_string()),
            message: Some(8),
            receipt: Some(("test".to_string(), "p1".to_string())),
            proposal: None,
        }
    }

    fn downgraded_claim() -> Claim {
        Claim {
            kind: Kind::Agent,
            requested: Source::User,
            code: Some(E_MEMORY_QUOTE_MISSING),
            quote: None,
            message: Some(9),
            receipt: None,
            proposal: None,
        }
    }

    fn claim_of(kind: Kind) -> Claim {
        match kind {
            Kind::User => user_claim("응"),
            Kind::Quoted => quoted_claim(),
            Kind::Agent => Claim::plain(Source::Agent),
            Kind::Inferred => Claim::plain(Source::Inferred),
        }
    }

    #[test]
    fn sha256_known_vectors() {
        let cases: [(Vec<u8>, &str); 7] = [
            (
                b"".to_vec(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".to_vec(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".to_vec(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                vec![b'a'; 1_000_000],
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
            (
                vec![b'a'; 55],
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                vec![b'a'; 56],
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                vec![b'a'; 64],
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
        ];
        for (data, expected) in cases {
            assert_eq!(hex(&sha256(&data)), expected, "length {}", data.len());
        }
    }

    #[test]
    fn proposal_hash_is_sixteen_hex() {
        assert_eq!(proposal_hash("abc"), "ba7816bf8f01cfea");
        assert_eq!(proposal_hash("proposal-text"), "369ee34e9531978a");
    }

    #[test]
    fn approval_forms() {
        for word in APPROVALS {
            for form in [
                word.to_string(),
                format!("{word}."),
                format!("{word}!"),
                format!("  {word} \n"),
            ] {
                assert!(is_approval(&form), "{form:?}");
            }
        }
        for yes in ["OK", "Yes!", "GO AHEAD."] {
            assert!(is_approval(yes), "{yes:?}");
        }
        for no in [
            "응!!", "yes..", "yess", "ok then", "네네", "do  it", "응?", "", " ",
        ] {
            assert!(!is_approval(no), "{no:?}");
        }
    }

    #[test]
    fn claim_json_has_every_field() {
        let keys = [
            "v",
            "source",
            "requested",
            "downgraded",
            "code",
            "quote",
            "message",
            "platform",
            "message_id",
            "proposal_row",
            "proposal_hash",
        ];
        let parse = |claim: &Claim| -> Value { serde_json::from_str(&claim.to_json()).unwrap() };
        let check_keys = |json: &Value| {
            let object = json.as_object().unwrap();
            assert_eq!(object.len(), keys.len());
            for key in keys {
                assert!(object.contains_key(key), "{key}");
            }
            assert_eq!(json["v"], 1);
        };

        let user = parse(&user_claim("응"));
        check_keys(&user);
        assert_eq!(user["source"], "user");
        assert_eq!(user["requested"], "user");
        assert_eq!(user["downgraded"], false);
        assert_eq!(user["code"], Value::Null);
        assert_eq!(user["quote"], "응");
        assert_eq!(user["message"], 7);
        assert_eq!(user["platform"], "test");
        assert_eq!(user["message_id"], "m1");
        assert_eq!(user["proposal_row"], 5);
        assert_eq!(user["proposal_hash"], "369ee34e9531978a");

        let quoted = parse(&quoted_claim());
        check_keys(&quoted);
        assert_eq!(quoted["source"], "quoted");
        assert_eq!(quoted["requested"], "user");
        assert_eq!(quoted["downgraded"], true);
        assert_eq!(quoted["code"], E_MEMORY_QUOTE_APPROVAL);
        assert_eq!(quoted["quote"], "green tea");
        assert_eq!(quoted["message"], 8);
        assert_eq!(quoted["platform"], "test");
        assert_eq!(quoted["message_id"], "p1");
        assert_eq!(quoted["proposal_row"], Value::Null);
        assert_eq!(quoted["proposal_hash"], Value::Null);

        let down = parse(&downgraded_claim());
        check_keys(&down);
        assert_eq!(down["source"], "agent");
        assert_eq!(down["requested"], "user");
        assert_eq!(down["downgraded"], true);
        assert_eq!(down["code"], E_MEMORY_QUOTE_MISSING);
        assert_eq!(down["quote"], Value::Null);
        assert_eq!(down["message"], 9);
        for key in ["platform", "message_id", "proposal_row", "proposal_hash"] {
            assert_eq!(down[key], Value::Null, "{key}");
        }

        for (source, name) in [(Source::Agent, "agent"), (Source::Inferred, "inferred")] {
            let plain = parse(&Claim::plain(source));
            check_keys(&plain);
            assert_eq!(plain["source"], name);
            assert_eq!(plain["requested"], name);
            assert_eq!(plain["downgraded"], false);
            for key in [
                "code",
                "quote",
                "message",
                "platform",
                "message_id",
                "proposal_row",
                "proposal_hash",
            ] {
                assert_eq!(plain[key], Value::Null, "{name} {key}");
            }
        }
    }

    #[test]
    fn stamp_kinds_and_mixing() {
        let olds: [Option<&str>; 7] = [
            None,
            Some("user: x"),
            Some("quoted: x"),
            Some("agent: x"),
            Some("inferred: x"),
            Some("mixed: x"),
            Some("owner said so"),
        ];
        let news = [Kind::User, Kind::Quoted, Kind::Agent, Kind::Inferred];
        for old in olds {
            let text = match old {
                None => "---\ndescription: d\n---\nbody\n".to_string(),
                Some(p) => format!("---\ndescription: d\nprovenance: {p}\n---\nbody\n"),
            };
            for new in news {
                let expected = match (old.and_then(|v| v.split_once(':')).map(|(w, _)| w), new) {
                    (None, _) if old.is_none() => new.name(),
                    (Some(o), n) if o == n.name() => n.name(),
                    (Some("agent" | "inferred"), Kind::Agent | Kind::Inferred) => new.name(),
                    _ => "mixed",
                };
                let stamped = stamp(&text, &claim_of(new)).unwrap();
                let value = provenance_of(&stamped).unwrap();
                assert!(
                    value.starts_with(&format!("{expected}:")),
                    "old {old:?} new {new:?} gave {value:?}"
                );
                assert_eq!(body_of(&stamped), "body\n");
            }
        }
        let fresh = "---\ndescription: d\n---\n";
        let agent_text = "---\ndescription: d\nprovenance: agent: x\n---\n";
        let user_text = "---\ndescription: d\nprovenance: user: x\n---\n";
        let other_text = "---\ndescription: d\nprovenance: owner said so\n---\n";
        let kind_of = |text: &str, kind: Kind| {
            let value = provenance_of(&stamp(text, &claim_of(kind)).unwrap()).unwrap();
            value.split_once(':').unwrap().0.to_string()
        };
        assert_eq!(kind_of(fresh, Kind::User), "user");
        assert_eq!(kind_of(agent_text, Kind::Inferred), "inferred");
        assert_eq!(kind_of(agent_text, Kind::User), "mixed");
        assert_eq!(kind_of(user_text, Kind::User), "user");
        assert_eq!(kind_of(user_text, Kind::Quoted), "mixed");
        assert_eq!(kind_of(user_text, Kind::Agent), "mixed");
        assert_eq!(kind_of(other_text, Kind::Agent), "mixed");
        let downgraded = provenance_of(&stamp(fresh, &downgraded_claim()).unwrap()).unwrap();
        assert!(downgraded.starts_with("agent:"));
        assert!(downgraded.contains(E_MEMORY_QUOTE_MISSING));
    }

    #[test]
    fn stamp_keeps_body_and_other_keys() {
        let text = "---\ndescription: d\nread_only: \"true\"\nexpires: 2999-01-01\n---\nline one\nline two";
        let stamped = stamp(text, &Claim::plain(Source::Agent)).unwrap();
        assert_eq!(body_of(&stamped), "line one\nline two");
        assert!(stamped.ends_with("---\nline one\nline two"));
        let parsed = frontmatter::parse(&stamped).unwrap();
        let keys: Vec<&str> = parsed
            .frontmatter
            .entries()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["description", "read_only", "expires", "provenance"]);
        assert_eq!(parsed.frontmatter.get("description"), Some("d"));
        assert_eq!(parsed.frontmatter.get("read_only"), Some("true"));
        assert_eq!(parsed.frontmatter.get("expires"), Some("2999-01-01"));

        let existing = "---\nprovenance: agent: x\ndescription: d\n---\nb\n";
        let restamped = stamp(existing, &Claim::plain(Source::Inferred)).unwrap();
        let parsed = frontmatter::parse(&restamped).unwrap();
        let keys: Vec<&str> = parsed
            .frontmatter
            .entries()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["provenance", "description"]);
        assert!(
            parsed
                .frontmatter
                .get(PROVENANCE_KEY)
                .unwrap()
                .starts_with("inferred:")
        );
        assert_eq!(parsed.body, "b\n");
    }

    #[test]
    fn stamp_quote_stays_one_line() {
        let quote = "say \"hi\"\nthen go";
        let claim = user_claim(quote);
        let stamped = stamp("---\ndescription: d\n---\nbody\n", &claim).unwrap();
        let value = provenance_of(&stamped).unwrap();
        assert!(!value.contains('\n'));
        assert!(value.starts_with("user:"));
        assert!(value.contains(&serde_json::to_string(quote).unwrap()));
        assert!(value.contains("7"));
        assert!(value.contains(&proposal_hash("proposal-text")));
        assert_eq!(body_of(&stamped), "body\n");
    }

    #[test]
    fn plain_claims_need_no_check() {
        for (source, kind) in [
            (Source::Agent, Kind::Agent),
            (Source::Inferred, Kind::Inferred),
        ] {
            let claim = Claim::plain(source);
            assert_eq!(claim.kind, kind);
            assert_eq!(claim.requested, source);
            assert!(!claim.downgraded());
            assert_eq!(claim.code, None);
            assert_eq!(claim.quote, None);
            assert_eq!(claim.message, None);
            assert_eq!(claim.receipt, None);
            assert_eq!(claim.proposal, None);
        }
        let user = Claim::plain(Source::User);
        assert_eq!(user.kind, Kind::Agent);
        assert_eq!(user.code, Some(E_MEMORY_QUOTE_MISSING));
        assert!(user.downgraded());
    }

    #[test]
    fn stamp_unclaimed_only_touches_owner_kinds() {
        for p in ["user: x", "quoted: x"] {
            let text = format!("---\ndescription: d\nprovenance: {p}\n---\nbody\n");
            let out = stamp_unclaimed(&text).unwrap();
            assert!(provenance_of(&out).unwrap().starts_with("mixed:"), "{p}");
            assert_eq!(body_of(&out), "body\n");
        }
        let untouched = [
            "---\ndescription: d\n---\nbody\n".to_string(),
            "---\ndescription: d\nprovenance: agent: x\n---\nbody\n".to_string(),
            "---\ndescription: d\nprovenance: inferred: x\n---\nbody\n".to_string(),
            "---\ndescription: d\nprovenance: mixed: x\n---\nbody\n".to_string(),
            "---\ndescription: d\nprovenance: owner said so\n---\nbody\n".to_string(),
            "---\ndescription: d\nread_only: \"false\"\nprovenance: agent: x\n---\nbody"
                .to_string(),
        ];
        for text in untouched {
            assert_eq!(stamp_unclaimed(&text).unwrap().as_bytes(), text.as_bytes());
        }
    }

    #[test]
    fn source_and_kind_names_round_trip() {
        for source in [Source::User, Source::Agent, Source::Inferred] {
            assert_eq!(Source::parse(source.name()), Some(source));
        }
        assert_eq!(Source::parse("quoted"), None);
        assert_eq!(Source::parse("User"), None);
        let names: Vec<&str> = [Kind::User, Kind::Quoted, Kind::Agent, Kind::Inferred]
            .iter()
            .map(|k| k.name())
            .collect();
        assert_eq!(names, ["user", "quoted", "agent", "inferred"]);
    }

    #[test]
    fn stamp_refuses_bad_frontmatter() {
        let err = stamp("no fence", &Claim::plain(Source::Agent)).unwrap_err();
        assert_eq!(err.code, E_MEMORY_FRONTMATTER);
        let err = stamp_unclaimed("no fence").unwrap_err();
        assert_eq!(err.code, E_MEMORY_FRONTMATTER);
    }
}
