//! Turning text into terms.
//!
//! ASCII, and deliberately so (ADR-036): lowercase ASCII letters, split on
//! anything that is not a letter or a digit, do nothing else. No stemming,
//! no stop words, no Unicode case folding, no segmentation for languages
//! that do not put spaces between words.
//!
//! Everything upstream goes through [`tokenize`], so replacing it is a
//! change to this file and a reindex.

/// One term and where it sat in the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The term, lowercased.
    pub term: String,
    /// Which token this was, counting from zero. Positions are ordinal
    /// rather than byte offsets: phrase search asks whether two terms are
    /// adjacent, and byte distance does not answer that.
    pub position: u32,
}

/// The longest term this will produce.
///
/// A term is a key, and a key has a limit. Rather than let a run of ten
/// thousand letters fail the insert, a term is cut here, which makes it
/// findable by its first bytes and nothing worse.
pub const MAX_TERM_LEN: usize = 128;

/// Split `text` into terms.
///
/// A byte is part of a term if it is an ASCII letter or digit; everything
/// else separates. Non-ASCII bytes separate too, which is the limitation
/// ADR-036 names rather than hides.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut position = 0u32;

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if current.len() < MAX_TERM_LEN {
                current.push(ch.to_ascii_lowercase());
            }
        } else if !current.is_empty() {
            out.push(Token {
                term: std::mem::take(&mut current),
                position,
            });
            position += 1;
        }
    }
    if !current.is_empty() {
        out.push(Token {
            term: current,
            position,
        });
    }
    out
}

/// The distinct terms of `text`, each with its positions in order.
///
/// This is the shape the index stores: one entry per term per document,
/// carrying every place it appeared, so term frequency is the length of
/// that list and no separate counter can drift from it.
pub fn postings(text: &str) -> Vec<(String, Vec<u32>)> {
    let mut terms: Vec<(String, Vec<u32>)> = Vec::new();
    for token in tokenize(text) {
        match terms.iter_mut().find(|(t, _)| *t == token.term) {
            Some((_, positions)) => positions.push(token.position),
            None => terms.push((token.term, vec![token.position])),
        }
    }
    terms.sort_by(|a, b| a.0.cmp(&b.0));
    terms
}

/// How many tokens `text` has, which is the document length BM25 wants.
pub fn length(text: &str) -> u32 {
    tokenize(text).len() as u32
}

/// Positions as they sit in a posting: little endian u32, in order.
///
/// Term frequency is the length of this and is never stored beside it,
/// so the two cannot disagree (ADR-036).
pub fn encode_positions(positions: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(positions.len() * 4);
    for p in positions {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

/// Read a posting back, or None if it is not a whole number of positions.
pub fn decode_positions(bytes: &[u8]) -> Option<Vec<u32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(|t| t.term).collect()
    }

    #[test]
    fn positions_survive_a_round_trip_and_junk_is_refused() {
        for case in [vec![], vec![0u32], vec![0, 1, 7, 4_000_000_000]] {
            assert_eq!(decode_positions(&encode_positions(&case)), Some(case));
        }
        assert_eq!(decode_positions(&[1, 2, 3]), None);
        assert_eq!(decode_positions(&[1, 2, 3, 4, 5]), None);
    }

    #[test]
    fn words_come_out_lowercased_and_split_on_punctuation() {
        assert_eq!(
            terms("The quick brown fox"),
            ["the", "quick", "brown", "fox"]
        );
        assert_eq!(
            terms("don't stop-me now!"),
            ["don", "t", "stop", "me", "now"]
        );
        assert_eq!(terms("MiXeD CaSe"), ["mixed", "case"]);
    }

    #[test]
    fn digits_are_terms_and_so_are_words_with_them() {
        assert_eq!(terms("rfc 2119 and utf8"), ["rfc", "2119", "and", "utf8"]);
        assert_eq!(terms("v1.2.3"), ["v1", "2", "3"]);
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   \n\t  ").is_empty());
        assert!(tokenize("...!?---").is_empty());
        assert_eq!(length(""), 0);
    }

    #[test]
    fn positions_count_tokens_not_bytes() {
        // Two terms separated by a paragraph are still adjacent positions,
        // which is what a phrase query needs to know.
        let tokens = tokenize("alpha\n\n\n     beta");
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 1);
    }

    #[test]
    fn a_repeated_word_keeps_every_position() {
        let p = postings("the cat sat on the mat the end");
        let the = p.iter().find(|(t, _)| t == "the").expect("the");
        assert_eq!(the.1, vec![0, 4, 6]);
        assert_eq!(p.iter().find(|(t, _)| t == "cat").unwrap().1, vec![1]);
    }

    #[test]
    fn postings_come_out_sorted_and_distinct() {
        let p = postings("zebra apple zebra mango apple apple");
        let names: Vec<&str> = p.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            names,
            ["apple", "mango", "zebra"],
            "not sorted or not distinct"
        );
        assert_eq!(p[0].1.len(), 3, "apple appears three times");
    }

    #[test]
    fn term_frequency_is_the_length_of_the_position_list() {
        // No separate counter exists, so none can drift from the list.
        let text = "a b a c a b a";
        let p = postings(text);
        let total: usize = p.iter().map(|(_, ps)| ps.len()).sum();
        assert_eq!(total as u32, length(text));
    }

    #[test]
    fn a_very_long_run_of_letters_is_cut_rather_than_refused() {
        let long = "x".repeat(MAX_TERM_LEN * 3);
        let tokens = tokenize(&long);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].term.len(), MAX_TERM_LEN);

        // and the cut does not swallow what follows it
        let tokens = tokenize(&format!("{long} tail"));
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].term, "tail");
    }

    #[test]
    fn non_ascii_separates_which_is_the_documented_limitation() {
        // ADR-036 names this rather than hiding it: a word split by a byte
        // this tokenizer cannot fold becomes two terms.
        let terms = terms("caf\u{e9} au lait");
        assert_eq!(terms, ["caf", "au", "lait"]);

        // and text with no ASCII at all produces nothing, rather than one
        // enormous term or a panic
        assert!(tokenize("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}").is_empty());
    }

    /// Deterministic junk: bytes that are letters, digits, punctuation,
    /// whitespace and multibyte characters in whatever order.
    fn junk(seed: u64, len: usize) -> String {
        let mut state = seed;
        let alphabet: Vec<char> = "abZ9 _-.,\n\t/\u{e9}\u{3053}!".chars().collect();
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                alphabet[(state >> 33) as usize % alphabet.len()]
            })
            .collect()
    }

    #[test]
    fn arbitrary_text_tokenizes_without_surprises() {
        // The tokenizer is the one thing every posting depends on, so it
        // gets held to its invariants over junk rather than over three
        // sentences somebody chose.
        for seed in 0..200u64 {
            let mut text = junk(seed, 1 + (seed as usize % 90));
            // Every so often, a run long enough to meet the cut, which
            // junk on its own is too broken up to reach.
            if seed % 7 == 0 {
                text.push_str(&"q".repeat(MAX_TERM_LEN + seed as usize % 40));
                text.push_str(&junk(seed + 991, 20));
            }
            let text = text;
            let tokens = tokenize(&text);

            for (i, token) in tokens.iter().enumerate() {
                assert_eq!(token.position, i as u32, "positions skipped, seed {seed}");
                assert!(!token.term.is_empty(), "empty term, seed {seed}");
                assert!(
                    token.term.len() <= MAX_TERM_LEN,
                    "over the cut, seed {seed}"
                );
                assert!(
                    token.term.chars().all(|c| c.is_ascii_alphanumeric()),
                    "a separator got into a term, seed {seed}: {:?}",
                    token.term
                );
                assert_eq!(
                    token.term,
                    token.term.to_ascii_lowercase(),
                    "not folded, seed {seed}"
                );
            }

            assert_eq!(length(&text), tokens.len() as u32);
            let p = postings(&text);
            let total: usize = p.iter().map(|(_, ps)| ps.len()).sum();
            assert_eq!(total, tokens.len(), "postings lost a token, seed {seed}");
            let mut names: Vec<&String> = p.iter().map(|(t, _)| t).collect();
            let before = names.len();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), before, "a term appeared twice, seed {seed}");
        }
    }

    #[test]
    fn every_position_is_used_once_and_in_order() {
        let text = "one two three two one three one";
        let mut seen: Vec<u32> = postings(text).into_iter().flat_map(|(_, ps)| ps).collect();
        seen.sort_unstable();
        let expected: Vec<u32> = (0..length(text)).collect();
        assert_eq!(seen, expected, "a position went missing or was reused");
    }
}
