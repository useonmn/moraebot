//! Who may talk to moraebot: the owner, and with `access_mode = "allowlist"` the ids in
//! the active messenger's `allow` list. Decided from stable ids only, never from names.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::adapter::InboundEvent;
use crate::config::{AllowEntry, BOT_REPLY_WINDOW_MINUTES, Config, Platform, Role, SenderKind};

pub const E_ACCESS_BOT: &str = "E_ACCESS_BOT";
pub const E_ACCESS_NOT_OWNER: &str = "E_ACCESS_NOT_OWNER";
/// A listed bot reached `max_bot_replies` in its thread; the message gets no answer.
pub const E_ACCESS_BOT_LIMIT: &str = "E_ACCESS_BOT_LIMIT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Bot,
    NotOwner,
    BotLimit,
}

impl DropReason {
    pub fn code(self) -> &'static str {
        match self {
            DropReason::Bot => E_ACCESS_BOT,
            DropReason::NotOwner => E_ACCESS_NOT_OWNER,
            DropReason::BotLimit => E_ACCESS_BOT_LIMIT,
        }
    }
}

/// Who sent an accepted message: the owner, or a listed id with its role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    Owner,
    Listed(Role),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow(Sender),
    Drop(DropReason),
}

/// The owner id and the allow list of the active messenger, ids as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    owner_id: String,
    allow: Vec<AllowEntry<String>>,
}

impl Policy {
    pub fn new(owner_id: impl Into<String>, allow: Vec<AllowEntry<String>>) -> Self {
        Self {
            owner_id: owner_id.into(),
            allow,
        }
    }

    /// The active messenger's owner and allow list (empty with `owner_only`).
    pub fn from_config(config: &Config) -> Self {
        let allow = match config.messenger {
            Platform::Discord => config.discord.as_ref().map(|d| {
                d.access
                    .allow
                    .iter()
                    .map(|entry| AllowEntry {
                        id: entry.id.to_string(),
                        kind: entry.kind,
                        role: entry.role,
                    })
                    .collect()
            }),
            Platform::Bridge => config.bridge.as_ref().map(|b| b.access.allow.clone()),
        };
        Self::new(config.owner_id(), allow.unwrap_or_default())
    }

    /// The owner (never as a bot), or a listed id whose `kind` matches the author's bot
    /// flag. Everything else is dropped: bots as [`DropReason::Bot`], people as
    /// [`DropReason::NotOwner`].
    pub fn allow(&self, event: &InboundEvent) -> Decision {
        if !event.author_is_bot && event.author_id == self.owner_id {
            return Decision::Allow(Sender::Owner);
        }
        let listed = self.allow.iter().find(|entry| {
            entry.id == event.author_id && (entry.kind == SenderKind::Bot) == event.author_is_bot
        });
        match listed {
            Some(entry) => Decision::Allow(Sender::Listed(entry.role)),
            None if event.author_is_bot => Decision::Drop(DropReason::Bot),
            None => Decision::Drop(DropReason::NotOwner),
        }
    }
}

/// Counts replies to bot messages per conversation in a sliding
/// [`BOT_REPLY_WINDOW_MINUTES`] window. Without a limit every reply is allowed and nothing
/// is kept.
pub struct BotReplies {
    max: Option<u32>,
    window: Duration,
    sent: HashMap<String, VecDeque<Instant>>,
}

impl BotReplies {
    pub fn new(max: Option<u32>) -> Self {
        Self {
            max,
            window: Duration::from_secs(BOT_REPLY_WINDOW_MINUTES * 60),
            sent: HashMap::new(),
        }
    }

    /// The active messenger's `max_bot_replies`.
    pub fn from_config(config: &Config) -> Self {
        let max = match config.messenger {
            Platform::Discord => config
                .discord
                .as_ref()
                .and_then(|d| d.access.max_bot_replies),
            Platform::Bridge => config
                .bridge
                .as_ref()
                .and_then(|b| b.access.max_bot_replies),
        };
        Self::new(max)
    }

    /// Records one reply in conversation `key` at `now` and returns true, or returns
    /// false when the conversation already had `max` replies in the window.
    pub fn take(&mut self, key: &str, now: Instant) -> bool {
        let Some(max) = self.max else {
            return true;
        };
        let window = self.window;
        self.sent.retain(|_, times| {
            while times
                .front()
                .is_some_and(|at| now.duration_since(*at) >= window)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = self.sent.entry(key.to_owned()).or_default();
        if times.len() >= max as usize {
            return false;
        }
        times.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_ID: &str = "111";
    const USER_ID: &str = "222";
    const BOT_ID: &str = "444";

    fn event(author_id: &str, author_is_bot: bool) -> InboundEvent {
        InboundEvent {
            message_id: "900".into(),
            author_id: author_id.into(),
            author_is_bot,
            channel_id: "333".into(),
            thread_id: None,
            is_dm: false,
            text: "hello".into(),
            attachments: Vec::new(),
        }
    }

    fn entry(id: &str, kind: SenderKind) -> AllowEntry<String> {
        AllowEntry {
            id: id.into(),
            kind,
            role: Role::Chat,
        }
    }

    fn owner_only() -> Policy {
        Policy::new(OWNER_ID, Vec::new())
    }

    fn listed() -> Policy {
        Policy::new(
            OWNER_ID,
            vec![
                entry(USER_ID, SenderKind::User),
                entry(BOT_ID, SenderKind::Bot),
            ],
        )
    }

    #[test]
    fn owner_is_allowed_as_owner() {
        for policy in [owner_only(), listed()] {
            assert_eq!(
                policy.allow(&event(OWNER_ID, false)),
                Decision::Allow(Sender::Owner)
            );
        }
    }

    #[test]
    fn other_user_is_dropped_as_not_owner() {
        let decision = owner_only().allow(&event(USER_ID, false));
        assert_eq!(decision, Decision::Drop(DropReason::NotOwner));
        assert_eq!(DropReason::NotOwner.code(), E_ACCESS_NOT_OWNER);
        assert_eq!(
            listed().allow(&event("999", false)),
            Decision::Drop(DropReason::NotOwner)
        );
    }

    #[test]
    fn bot_with_owner_id_is_dropped_as_bot() {
        for policy in [owner_only(), listed()] {
            assert_eq!(
                policy.allow(&event(OWNER_ID, true)),
                Decision::Drop(DropReason::Bot)
            );
        }
        assert_eq!(DropReason::Bot.code(), E_ACCESS_BOT);
    }

    #[test]
    fn empty_author_id_is_dropped_as_not_owner() {
        assert_eq!(
            listed().allow(&event("", false)),
            Decision::Drop(DropReason::NotOwner)
        );
    }

    #[test]
    fn listed_user_is_allowed_with_its_role() {
        assert_eq!(
            listed().allow(&event(USER_ID, false)),
            Decision::Allow(Sender::Listed(Role::Chat))
        );
    }

    #[test]
    fn listed_bot_is_allowed_unlisted_bot_dropped() {
        assert_eq!(
            listed().allow(&event(BOT_ID, true)),
            Decision::Allow(Sender::Listed(Role::Chat))
        );
        assert_eq!(
            listed().allow(&event("555", true)),
            Decision::Drop(DropReason::Bot)
        );
        assert_eq!(
            owner_only().allow(&event(BOT_ID, true)),
            Decision::Drop(DropReason::Bot)
        );
    }

    #[test]
    fn kind_must_match_the_author_bot_flag() {
        assert_eq!(
            listed().allow(&event(USER_ID, true)),
            Decision::Drop(DropReason::Bot)
        );
        assert_eq!(
            listed().allow(&event(BOT_ID, false)),
            Decision::Drop(DropReason::NotOwner)
        );
    }

    #[test]
    fn bot_replies_cap_per_conversation_within_the_window() {
        let mut replies = BotReplies::new(Some(2));
        let start = Instant::now();
        assert!(replies.take("a", start));
        assert!(replies.take("a", start));
        assert!(!replies.take("a", start + Duration::from_secs(60)));
        assert!(replies.take("b", start), "other conversations count apart");
        let later = start + Duration::from_secs(BOT_REPLY_WINDOW_MINUTES * 60);
        assert!(
            replies.take("a", later),
            "the window slid past both replies"
        );
    }

    #[test]
    fn bridge_config_feeds_the_policy_and_the_bot_limit() {
        let env = crate::config::Env {
            home_dir: Some("/h/u".into()),
            ..Default::default()
        };
        let config = Config::from_toml(
            "messenger = \"bridge\"\naccess_mode = \"allowlist\"\nomp_path = \"/bin/omp\"\n\
             [bridge]\nsocket = \"/run/b.sock\"\nowner_id = \"U111\"\nmax_bot_replies = 2\n\
             [[bridge.allow]]\nid = \"B444\"\nkind = \"bot\"\n",
            &env,
        )
        .expect("loads");
        let policy = Policy::from_config(&config);
        assert_eq!(
            policy,
            Policy::new("U111", vec![entry("B444", SenderKind::Bot)])
        );
        assert_eq!(
            policy.allow(&event("U111", false)),
            Decision::Allow(Sender::Owner)
        );
        assert_eq!(
            policy.allow(&event("B444", true)),
            Decision::Allow(Sender::Listed(Role::Chat))
        );
        assert_eq!(BotReplies::from_config(&config).max, Some(2));
    }

    #[test]
    fn bot_replies_without_a_limit_never_cap() {
        let mut replies = BotReplies::new(None);
        let now = Instant::now();
        assert!((0..1000).all(|_| replies.take("a", now)));
        assert!(replies.sent.is_empty());
    }
}
