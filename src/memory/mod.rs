//! Owner memory: markdown files with frontmatter under `<home>/memory`, rendered into the lead's
//! appended system prompt at each lead start. Design follows omo/senpi memory; no omo code or text
//! is used. Messenger-agnostic.

pub mod frontmatter;
pub mod home;
pub mod render;
pub mod tools;

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
