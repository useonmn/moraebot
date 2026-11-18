//! The persona file: what the owner chose at the first meeting, appended to the lead's system
//! prompt at every start.

/// The persona file name under the moraebot home.
pub const FILE: &str = "persona.md";
/// The largest persona file, in bytes.
pub const MAX_BYTES: usize = 2048;
/// The line put above the persona text in the lead's system prompt.
pub const HEADER: &str = "[moraebot persona] What the owner chose at the first meeting: your name, how you talk and a few basics about them. The owner can change it with moraebot onboarding or by editing persona.md in the moraebot home; a change applies the next time moraebot starts.";
/// An empty name or a line break in a field.
pub const E_PERSONA_FIELD: &str = "E_PERSONA_FIELD";
/// More than `MAX_BYTES`.
pub const E_PERSONA_SIZE: &str = "E_PERSONA_SIZE";
/// Secret-like text.
pub const E_PERSONA_SECRET: &str = "E_PERSONA_SECRET";
/// The file was not written.
pub const E_PERSONA_WRITE: &str = "E_PERSONA_WRITE";
/// The file is not readable as UTF-8 text.
pub const E_PERSONA_READ: &str = "E_PERSONA_READ";
