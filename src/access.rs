use crate::adapter::InboundEvent;

pub const E_ACCESS_BOT: &str = "E_ACCESS_BOT";
pub const E_ACCESS_NOT_OWNER: &str = "E_ACCESS_NOT_OWNER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Bot,
    NotOwner,
}

impl DropReason {
    pub fn code(self) -> &'static str {
        match self {
            DropReason::Bot => E_ACCESS_BOT,
            DropReason::NotOwner => E_ACCESS_NOT_OWNER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Drop(DropReason),
}

pub fn allow(event: &InboundEvent, owner_id: &str) -> Decision {
    if event.author_is_bot {
        return Decision::Drop(DropReason::Bot);
    }
    if event.author_id != owner_id {
        return Decision::Drop(DropReason::NotOwner);
    }
    Decision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_ID: &str = "111";

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

    #[test]
    fn owner_is_allowed() {
        assert_eq!(allow(&event(OWNER_ID, false), OWNER_ID), Decision::Allow);
    }

    #[test]
    fn other_user_is_dropped_as_not_owner() {
        let decision = allow(&event("222", false), OWNER_ID);
        assert_eq!(decision, Decision::Drop(DropReason::NotOwner));
        assert_eq!(DropReason::NotOwner.code(), E_ACCESS_NOT_OWNER);
    }

    #[test]
    fn bot_with_owner_id_is_dropped_as_bot() {
        let decision = allow(&event(OWNER_ID, true), OWNER_ID);
        assert_eq!(decision, Decision::Drop(DropReason::Bot));
        assert_eq!(DropReason::Bot.code(), E_ACCESS_BOT);
    }

    #[test]
    fn empty_author_id_is_dropped_as_not_owner() {
        assert_eq!(
            allow(&event("", false), OWNER_ID),
            Decision::Drop(DropReason::NotOwner)
        );
    }
}
