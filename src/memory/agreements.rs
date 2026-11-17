//! The standing agreements ledger: agreements the owner and the lead keep, with broken counts.

/// The comparison form of an agreement text: trimmed, inner whitespace runs folded to one
/// space, lowercased (`str::to_lowercase`).
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_case_and_spaces() {
        assert_eq!(normalize("  Never  Push\tTO main "), "never push to main");
        assert_eq!(normalize("커밋 전에  묻기"), "커밋 전에 묻기");
    }
}
