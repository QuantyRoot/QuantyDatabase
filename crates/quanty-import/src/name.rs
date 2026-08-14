//! Turning a SQLite name into one our language accepts.
//!
//! SQLite lets a name be almost anything: spaces, punctuation, emoji, a
//! leading digit, or one of our operator words. QQL identifiers are ASCII
//! letters, digits and underscores, not starting with a digit, and `not`,
//! `and` and `or` are refused outright (ADR-017).
//!
//! So some names have to change, and the rule for changing them is chosen
//! to be boring and predictable rather than clever: a developer reading the
//! import report should be able to guess what a name became without
//! consulting anything. Every rename is reported, because a silently
//! renamed column is a query that silently returns nothing.

/// The three words that cannot name a table or a column.
const OPERATOR_WORDS: [&str; 3] = ["not", "and", "or"];

/// Map one name into the identifier grammar, without regard to what else
/// exists. Collisions are the caller's problem; see `Names`.
fn rewrite(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for c in source.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            // one underscore per unusable character, so `first name` and
            // `first-name` do not collapse onto the same thing silently
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if OPERATOR_WORDS.contains(&out.to_ascii_lowercase().as_str()) {
        out.push('_');
    }
    out
}

/// Assigns each source name a legal identifier, keeping them distinct.
///
/// Two different SQLite names can rewrite to the same identifier, and our
/// identifiers are matched without regard to case, so `Name` and `name` are
/// a collision too. The second one to arrive gets a numeric suffix.
#[derive(Default)]
pub struct Names {
    taken: Vec<String>,
}

impl Names {
    pub fn new() -> Names {
        Names { taken: Vec::new() }
    }

    /// The identifier for `source`, and whether it had to change.
    pub fn assign(&mut self, source: &str) -> (String, bool) {
        let base = rewrite(source);
        let mut candidate = base.clone();
        let mut suffix = 2u32;
        while self
            .taken
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&candidate))
        {
            candidate = format!("{base}_{suffix}");
            suffix += 1;
        }
        self.taken.push(candidate.clone());
        let changed = candidate != source;
        (candidate, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_left_alone() {
        let mut names = Names::new();
        for name in ["users", "AlbumId", "_private", "x2", "TrackId"] {
            let (mapped, changed) = names.assign(name);
            assert_eq!(mapped, name);
            assert!(!changed, "{name} should not have changed");
        }
    }

    #[test]
    fn unusable_characters_become_underscores() {
        let mut names = Names::new();
        assert_eq!(names.assign("first name").0, "first_name");
        assert_eq!(names.assign("first-name").0, "first_name_2");
        assert_eq!(names.assign("caf\u{e9}").0, "caf_");
        assert_eq!(names.assign("a.b").0, "a_b");
    }

    #[test]
    fn a_leading_digit_gets_an_underscore() {
        let mut names = Names::new();
        assert_eq!(names.assign("2fast").0, "_2fast");
        assert_eq!(names.assign("").0, "_");
    }

    #[test]
    fn the_operator_words_are_refused_and_suffixed() {
        let mut names = Names::new();
        assert_eq!(names.assign("not").0, "not_");
        assert_eq!(names.assign("and").0, "and_");
        assert_eq!(names.assign("or").0, "or_");
        // only exactly those three, and only as whole words
        assert_eq!(names.assign("nothing").0, "nothing");
        assert_eq!(names.assign("android").0, "android");
    }

    #[test]
    fn collisions_are_numbered_in_arrival_order() {
        let mut names = Names::new();
        assert_eq!(names.assign("name").0, "name");
        assert_eq!(names.assign("Name").0, "Name_2");
        assert_eq!(names.assign("NAME").0, "NAME_3");
    }

    #[test]
    fn a_rename_is_always_reported() {
        let mut names = Names::new();
        assert!(!names.assign("plain").1);
        assert!(names.assign("with space").1);
        assert!(names.assign("plain").1, "the suffixed one changed too");
    }
}
