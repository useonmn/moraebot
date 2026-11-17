//! Owner memory: markdown files with frontmatter under `<home>/memory`, rendered into the lead's
//! appended system prompt at each lead start. Design follows omo/senpi memory; no omo code or text
//! is used. Messenger-agnostic.
//!
//! Writes go through the memory host tools; each one is recorded in the inbox database
//! (`memory_commits`, `memory_snapshots`) before the working files change, and the block is
//! rendered from the recorded text.

pub mod frontmatter;
pub mod history;
pub mod home;
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
