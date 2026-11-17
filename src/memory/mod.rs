//! Owner memory: markdown files with frontmatter under `<home>/memory`, rendered into the lead's
//! appended system prompt at each lead start. Design follows omo/senpi memory; no omo code or text
//! is used. Messenger-agnostic.
//!
//! Writes go through the memory host tools; each one is recorded in the inbox database
//! (`memory_commits`, `memory_snapshots`) before the working files change, and the block is
//! rendered from the recorded text.
//!
//! Guards (`read_only`, `expires`, the secret table) live in `guard`.
//!
//! Where a write's words came from (claims, the provenance stamp) lives in `provenance`.
//!
//! What changed since the lead's block was pinned, and the notice lines that say so, live in
//! `notice`.
//!
//! The standing agreements ledger (stored in the inbox database) and its tools live in
//! `agreements`.
//!
//! Person cards and observation files under `people/` live in `people`; multi-file patches
//! live in `patch`.
//!
//! The first meeting with the owner (the request file, the state in the inbox database and the
//! lead's finish tool) lives in `onboarding`.
//!
//! The size estimate of the system files and its line in the block live in `render`; the
//! over-budget notice lives in `notice`.

pub mod agreements;
pub mod frontmatter;
pub mod guard;
pub mod history;
pub mod home;
pub mod notice;
pub mod onboarding;
pub mod patch;
pub mod people;
pub mod provenance;
pub mod render;
pub mod tools;
pub mod write;

/// The memory folder under the moraebot home.
pub const MEMORY_DIR: &str = "memory";
/// The folder of always-rendered system blocks inside the memory folder.
pub const SYSTEM_DIR: &str = "system";
/// The top-level folders a memory path may start with.
pub const HOMES: [&str; 5] = ["system", "reference", "notes", "people", "skills"];
/// Who the lead is: character and way of working.
pub const PERSONA: &str = "system/persona.md";
/// Optional particulars of the lead, such as its name.
pub const IDENTITY: &str = "system/identity.md";
/// What the lead knows about the owner.
pub const HUMAN: &str = "system/human.md";
/// The only frontmatter keys a memory file may use.
pub const KNOWN_KEYS: [&str; 7] = [
    "description",
    "read_only",
    "limit",
    "kind",
    "aliases",
    "provenance",
    "expires",
];
/// The largest memory file that is read or rendered, in bytes.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
/// The largest rendered memory block, in bytes.
pub const MAX_RENDER_BYTES: usize = 64 * 1024;
/// The longest frontmatter `description`, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// The deepest memory path, in components.
pub const MAX_DEPTH: usize = 8;
/// The line that opens the rendered memory block.
pub const BEGIN: &str = "<!-- moraebot-memory:begin -->";
/// The line that closes the rendered memory block.
pub const END: &str = "<!-- moraebot-memory:end -->";
/// The lead's host tool that lists memory files.
pub const MEMORY_LIST_TOOL: &str = "memory_list";
/// The lead's host tool that reads one memory file.
pub const MEMORY_READ_TOOL: &str = "memory_read";

/// The lead's host tool that creates one memory file.
pub const MEMORY_CREATE_TOOL: &str = "memory_create";
/// The lead's host tool that replaces one exact string in a memory file.
pub const MEMORY_STR_REPLACE_TOOL: &str = "memory_str_replace";
/// The lead's host tool that inserts text at a line of a memory file's body.
pub const MEMORY_INSERT_TOOL: &str = "memory_insert";
/// The lead's host tool that deletes a memory file or folder.
pub const MEMORY_DELETE_TOOL: &str = "memory_delete";
/// The lead's host tool that renames a memory file.
pub const MEMORY_RENAME_TOOL: &str = "memory_rename";
/// The lead's host tool that changes a memory file's `description`.
pub const MEMORY_UPDATE_DESCRIPTION_TOOL: &str = "memory_update_description";
/// The longest `reason` a write may carry, in characters.
pub const MAX_REASON_CHARS: usize = 200;
/// The most dirty paths named in one refusal or report.
pub const MAX_DIRTY_LISTED: usize = 20;
/// Writer of a commit made by the lead's tools.
pub const WRITER_LEAD: &str = "lead";
/// Writer of a commit made by `moraebot memory accept`.
pub const WRITER_HUMAN: &str = "human";
/// Writer of the first commit that imports the existing files.
pub const WRITER_IMPORT: &str = "import";
/// Files that cannot be deleted or renamed.
pub const PROTECTED: [&str; 2] = [PERSONA, HUMAN];

/// The largest notes index element, in bytes (markers not counted).
pub const MAX_INDEX_BYTES: usize = 40 * 1024;
/// Top-level folders the notes index leaves out.
pub const NOT_INDEXED: [&str; 2] = [SYSTEM_DIR, "skills"];
/// The frontmatter key that marks a file the lead cannot change.
pub const READ_ONLY_KEY: &str = "read_only";
/// The frontmatter key that holds a file's expiry time.
pub const EXPIRES_KEY: &str = "expires";
/// The frontmatter key that records where a file's words came from.
pub const PROVENANCE_KEY: &str = "provenance";
/// The longest owner quote a claim may carry, in characters.
pub const MAX_QUOTE_CHARS: usize = 300;
/// Whole owner messages that approve the lead's last proposal.
pub const APPROVALS: [&str; 17] = [
    "응",
    "네",
    "예",
    "좋아",
    "그래",
    "지워",
    "지워줘",
    "삭제해",
    "삭제해줘",
    "진행해",
    "yes",
    "ok",
    "okay",
    "do it",
    "go ahead",
    "delete it",
    "yes delete it",
];

/// The line that opens the memory notice in an owner input.
pub const NOTICE_BEGIN: &str = "[moraebot memory-notice]";
/// The line that closes the memory notice in an owner input.
pub const NOTICE_END: &str = "[moraebot memory-notice end]";
/// The most changed paths named in one notice.
pub const MAX_NOTICE_PATHS: usize = 20;
/// Owner turns without a lead memory save between two nudges.
pub const NUDGE_EVERY: i64 = 10;
/// Files that shape who the lead is.
pub const SOUL_PATHS: [&str; 3] = [PERSONA, IDENTITY, BOUNDARIES];
/// The file under the memory root that requests a re-pin at the next owner turn.
pub const REFRESH_FILE: &str = ".refresh";

/// The owner's exact "don't" words.
pub const BOUNDARIES: &str = "system/boundaries.md";
/// Reserved: the agreements ledger lives in the database, and no memory file may use this path.
pub const AGREEMENTS_FILE: &str = "system/agreements.md";
/// The lead's host tool that records a standing agreement.
pub const MEMORY_AGREEMENT_RECORD_TOOL: &str = "memory_agreement_record";
/// The lead's host tool that reports a broken agreement.
pub const MEMORY_AGREEMENT_BREAK_TOOL: &str = "memory_agreement_break";
/// The scopes an agreement may have.
pub const AGREEMENT_SCOPES: [&str; 2] = ["global", "project"];
/// An agreement the owner approved.
pub const AUTHOR_HUMAN: &str = "human";
/// An agreement the lead recorded on its own.
pub const AUTHOR_AGENT: &str = "agent";
/// The most agreements the ledger holds.
pub const MAX_AGREEMENTS: usize = 64;
/// The longest agreement text, in characters.
pub const MAX_AGREEMENT_CHARS: usize = 240;
/// The sum of all stored agreement texts in UTF-8 bytes, before escaping.
pub const MAX_AGREEMENT_BYTES: usize = 12288;
/// The broken count at which an agreement needs the owner.
pub const NEEDS_HUMAN_AT: i64 = 3;

/// The memory folders or seeds could not be made.
pub const E_MEMORY_LAYOUT: &str = "E_MEMORY_LAYOUT";
/// Missing or invalid frontmatter.
pub const E_MEMORY_FRONTMATTER: &str = "E_MEMORY_FRONTMATTER";
/// A file that cannot be used: not regular, symlink, too large, not UTF-8, NUL or sentinel inside.
pub const E_MEMORY_FILE: &str = "E_MEMORY_FILE";
/// Left out because the block reached `MAX_RENDER_BYTES`.
pub const E_MEMORY_BUDGET: &str = "E_MEMORY_BUDGET";
/// Bad tool arguments.
pub const E_MEMORY_ARGS: &str = "E_MEMORY_ARGS";
/// A path outside the allowed homes or not a plain `.md` path.
pub const E_MEMORY_PATH: &str = "E_MEMORY_PATH";
/// No such file.
pub const E_MEMORY_NOT_FOUND: &str = "E_MEMORY_NOT_FOUND";

/// No running owner turn (none, or a lane-result turn).
pub const E_MEMORY_ORIGIN: &str = "E_MEMORY_ORIGIN";
/// A memory file was changed outside the tools and not accepted yet.
pub const E_MEMORY_DIRTY: &str = "E_MEMORY_DIRTY";
/// The write would change nothing.
pub const E_MEMORY_NOOP: &str = "E_MEMORY_NOOP";
/// The target path is already a memory file.
pub const E_MEMORY_EXISTS: &str = "E_MEMORY_EXISTS";
/// `old_string` does not occur in the file.
pub const E_MEMORY_NO_MATCH: &str = "E_MEMORY_NO_MATCH";
/// The write adds, changes or removes `provenance`.
pub const E_MEMORY_PROVENANCE: &str = "E_MEMORY_PROVENANCE";
/// The persona and the owner card cannot be deleted or renamed.
pub const E_MEMORY_PROTECTED: &str = "E_MEMORY_PROTECTED";
/// The history tables could not be read or written.
pub const E_MEMORY_HISTORY: &str = "E_MEMORY_HISTORY";
/// A working file could not be written or removed.
pub const E_MEMORY_WRITE: &str = "E_MEMORY_WRITE";
/// The file is marked `read_only` and the lead cannot change it.
pub const E_MEMORY_READ_ONLY: &str = "E_MEMORY_READ_ONLY";
/// The text, path or reason looks like a secret.
pub const E_MEMORY_SECRET: &str = "E_MEMORY_SECRET";
/// The file's `expires` has passed, so it is not rendered.
pub const E_MEMORY_EXPIRED: &str = "E_MEMORY_EXPIRED";
/// `source` is `user` but `user_quote` or `quote_message` is missing, or the quote is blank.
pub const E_MEMORY_QUOTE_MISSING: &str = "E_MEMORY_QUOTE_MISSING";
/// The quote is longer than `MAX_QUOTE_CHARS`.
pub const E_MEMORY_QUOTE_LONG: &str = "E_MEMORY_QUOTE_LONG";
/// The quoted row does not exist, is not an owner message, is in another conversation or has
/// another author.
pub const E_MEMORY_QUOTE_ROW: &str = "E_MEMORY_QUOTE_ROW";
/// The quoted row is neither the turn's row nor steered into it.
pub const E_MEMORY_QUOTE_TURN: &str = "E_MEMORY_QUOTE_TURN";
/// The quote does not occur in the row's text.
pub const E_MEMORY_QUOTE_TEXT: &str = "E_MEMORY_QUOTE_TEXT";
/// The quote is from the row but is not the whole text or not an approval.
pub const E_MEMORY_QUOTE_APPROVAL: &str = "E_MEMORY_QUOTE_APPROVAL";
/// An approval with no lead reply right before it.
pub const E_MEMORY_QUOTE_PROPOSAL: &str = "E_MEMORY_QUOTE_PROPOSAL";
/// A change of `system/boundaries.md` without a verified owner approval.
pub const E_MEMORY_QUOTE: &str = "E_MEMORY_QUOTE";
/// The agreements guard: the reserved file path, an unverified `user` agreement, or the database
/// guard.
pub const E_MEMORY_AGREEMENT: &str = "E_MEMORY_AGREEMENT";
/// The same agreement (normalized text) is already recorded.
pub const E_MEMORY_AGREEMENT_EXISTS: &str = "E_MEMORY_AGREEMENT_EXISTS";
/// The ledger holds `MAX_AGREEMENTS` agreements.
pub const E_MEMORY_AGREEMENT_FULL: &str = "E_MEMORY_AGREEMENT_FULL";
/// No agreement has that id.
pub const E_MEMORY_AGREEMENT_UNKNOWN: &str = "E_MEMORY_AGREEMENT_UNKNOWN";
/// This agreement's break was already counted in this owner turn.
pub const E_MEMORY_AGREEMENT_REPEAT: &str = "E_MEMORY_AGREEMENT_REPEAT";
/// The file under the moraebot home that asks for a first meeting with the owner.
pub const ONBOARDING_REQUEST_FILE: &str = "onboarding.request";
/// The lead's host tool that ends the first meeting.
pub const ONBOARDING_FINISH_TOOL: &str = "onboarding_finish";
/// How a first meeting can end.
pub const ONBOARDING_OUTCOMES: [&str; 2] = ["done", "skipped"];
/// The most owner turns that carry the onboarding notice line.
pub const MAX_ONBOARDING_TURNS: i64 = 10;
/// No onboarding is running.
pub const E_ONBOARDING_STATE: &str = "E_ONBOARDING_STATE";
/// The onboarding request file could not be written or removed.
pub const E_ONBOARDING_REQUEST: &str = "E_ONBOARDING_REQUEST";
/// Bytes counted as one token in the size estimate.
pub const BYTES_PER_TOKEN: usize = 4;
/// The advisory size of the system files and agreements, in estimated tokens.
pub const PRESSURE_BUDGET_TOKENS: usize = 30000;
/// The share of the budget at which the block carries the pressure line.
pub const PRESSURE_WARN_PERCENT: usize = 80;
/// The system files and agreements reached the warning share of their size budget.
pub const E_MEMORY_PRESSURE: &str = "E_MEMORY_PRESSURE";
/// The folder of person records.
pub const PEOPLE_DIR: &str = "people";
/// Who a person is: one file per person folder.
pub const CARD_FILE: &str = "card.md";
/// Dated facts about a person: one file per person folder.
pub const OBSERVATIONS_FILE: &str = "observations.md";
/// The slug that means the owner, whose card is `HUMAN`.
pub const HUMAN_SLUG: &str = "human";
/// The heading of the facts the lead was told.
pub const EXPLICIT_SECTION: &str = "## Explicit";
/// The starts a card line may have.
pub const CARD_PREFIXES: [&str; 4] = [
    "IDENTITY: ",
    "ATTRIBUTE: ",
    "RELATIONSHIP: ",
    "INSTRUCTION: ",
];
/// The most card lines or observation entries in one file.
pub const MAX_PERSON_ENTRIES: usize = 40;
/// The longest card line or observation entry, in characters.
pub const MAX_PERSON_LINE_CHARS: usize = 200;
/// The longest person slug.
pub const MAX_SLUG_CHARS: usize = 40;
/// The lead's host tool that applies a multi-file patch in one commit.
pub const MEMORY_APPLY_PATCH_TOOL: &str = "memory_apply_patch";
/// The lead's host tool that records one dated fact about a person.
pub const MEMORY_OBSERVE_TOOL: &str = "memory_observe";
/// The largest patch text.
pub const MAX_PATCH_BYTES: usize = 64 * 1024;
/// The most files one patch may name.
pub const MAX_PATCH_FILES: usize = 20;
/// A person card or observation file breaks its format or limits, or a bad slug.
pub const E_MEMORY_PERSON: &str = "E_MEMORY_PERSON";
/// A patch that cannot be read or does not apply.
pub const E_MEMORY_PATCH: &str = "E_MEMORY_PATCH";
