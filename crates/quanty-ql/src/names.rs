//! The words that cannot be table or column names, in either front end.
//!
//! QQL has no reserved words by design: context decides, so a column may
//! still be called `limit` or `order`. Two classes are the exception, and
//! the fuzzer found both rather than the design anticipating either
//! (ADR-017). A name spelled like something the expression grammar reads
//! as a token parses fine, and then the canonical form prints it as that
//! token, so `parse(pretty(ast)) == ast` no longer holds.
//!
//! One list, used by both front ends. It was two copies until the second
//! class arrived, at which point a word added on one side would have gone
//! missing on the other.

/// Operators in an expression. `h = not * x` reads `not` as a name, and
/// the canonical parentheses put it back in operator position.
const OPERATOR_WORDS: [&str; 3] = ["not", "and", "or"];

/// Literals in an expression. `false *= limit` desugars to
/// `false = false * limit`, where the left side is a column and the right
/// side is the boolean, and the printed form cannot tell them apart.
const LITERAL_WORDS: [&str; 3] = ["true", "false", "null"];

/// Why this word cannot name a table or column, if it cannot.
///
/// Case is significant, as ADR-017 says: QQL keywords are lowercase, so
/// `NOT` and `False` remain legal names.
pub(crate) fn refuse_as_name(word: &str) -> Option<String> {
    if OPERATOR_WORDS.contains(&word) {
        return Some(format!(
            "'{word}' is an operator and cannot name a table or column"
        ));
    }
    if LITERAL_WORDS.contains(&word) {
        return Some(format!(
            "'{word}' is a literal value and cannot name a table or column"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_classes_are_refused_and_nothing_else_is() {
        for word in ["not", "and", "or"] {
            let message = refuse_as_name(word).expect("operator refused");
            assert!(message.contains("is an operator"), "{message}");
        }
        for word in ["true", "false", "null"] {
            let message = refuse_as_name(word).expect("literal refused");
            assert!(message.contains("is a literal value"), "{message}");
        }
        for word in ["limit", "order", "get", "set", "index", "score"] {
            assert!(
                refuse_as_name(word).is_none(),
                "{word} is not reserved and must stay a legal name"
            );
        }
    }

    /// The rule is lowercase, which is what makes it small enough to live
    /// with.
    #[test]
    fn case_carries_the_word_back_out_of_the_rule() {
        assert!(refuse_as_name("NOT").is_none());
        assert!(refuse_as_name("False").is_none());
        assert!(refuse_as_name("Null").is_none());
    }
}
