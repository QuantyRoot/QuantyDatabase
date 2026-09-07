//! Turning text into terms.
//!
//! A word is a run of letters and digits, lowercased. Letters are
//! whatever Unicode says they are, and lowercasing is whatever Unicode
//! says it is, both of which the standard library already knows, so this
//! needs no table and no dependency (ADR-036).
//!
//! What it still does not do: stemming, stop words, folding accents away,
//! or segmenting languages that do not put spaces between words. Those
//! are tables or algorithms, and none has earned one yet.
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
/// A character is part of a term if Unicode calls it a letter or a digit;
/// everything else separates. Lowercasing is Unicode's too, so `STRASSE`
/// and `Straße` do not become the same word but `ÜBER` and `über` do.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut position = 0u32;

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // The cut counts bytes and never splits a character, so a
            // term may end a few bytes over rather than a few under.
            if current.len() + ch.len_utf8() <= MAX_TERM_LEN {
                current.extend(ch.to_lowercase());
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

/// Whether `haystack` contains every word of `needle`.
///
/// Both sides go through the same tokenizer, so a query matches on words
/// rather than on substrings: `cat` does not find `category`. An empty
/// query matches everything, which is what asking for nothing means.
pub fn contains_all(haystack: &str, needle: &str) -> bool {
    let wanted = query_terms(needle);
    if wanted.is_empty() {
        return true;
    }
    let have = terms_of(haystack);
    wanted.iter().all(|w| {
        if w.prefix {
            have.iter().any(|h| h.starts_with(&w.term))
        } else {
            have.contains(&w.term)
        }
    })
}

/// The distinct terms of `text`, sorted, without their positions.
pub fn terms_of(text: &str) -> Vec<String> {
    postings(text).into_iter().map(|(t, _)| t).collect()
}

/// One word of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub term: String,
    /// Whether a trailing star asked for every word starting with it.
    pub prefix: bool,
}

/// Split a query into words, honouring a trailing star.
///
/// The star has to be read before tokenizing, because the tokenizer sees
/// it as a separator like any other punctuation. So the query is cut on
/// whitespace first, a chunk ending in one or more stars marks its last
/// word as a prefix, and everything else is tokenized as usual. A star
/// anywhere but at the end of a chunk stays what it was: a separator.
/// `qu*ick` is two words, not one prefix.
pub fn query_terms(query: &str) -> Vec<QueryTerm> {
    let mut out = Vec::new();
    for chunk in query.split_whitespace() {
        let starred = chunk.ends_with('*');
        let body = chunk.trim_end_matches('*');
        let words: Vec<String> = tokenize(body).into_iter().map(|t| t.term).collect();
        let last = words.len().saturating_sub(1);
        for (i, term) in words.into_iter().enumerate() {
            out.push(QueryTerm {
                term,
                prefix: starred && i == last,
            });
        }
    }
    out
}

/// Whether a query asks for a prefix anywhere.
pub fn has_prefix(query: &str) -> bool {
    query_terms(query).iter().any(|t| t.prefix)
}

/// How often the words of `needle` appear in `haystack` back to back.
///
/// The brute force half of a phrase query, and the definition the index
/// has to agree with. Both sides tokenize, so a phrase matches words in
/// sequence rather than a substring, and an empty needle is a phrase of
/// nothing, which is everywhere: it answers zero here and the caller
/// treats that shape as matching, exactly as `contains_all` does.
pub fn phrase_occurrences(haystack: &str, needle: &str) -> u32 {
    let wanted: Vec<String> = tokenize(needle).into_iter().map(|t| t.term).collect();
    if wanted.is_empty() {
        return 0;
    }
    let have: Vec<String> = tokenize(haystack).into_iter().map(|t| t.term).collect();
    if have.len() < wanted.len() {
        return 0;
    }
    have.windows(wanted.len())
        .filter(|window| *window == wanted.as_slice())
        .count() as u32
}

/// Whether `haystack` contains the words of `needle` back to back.
pub fn contains_phrase(haystack: &str, needle: &str) -> bool {
    tokenize(needle).is_empty() || phrase_occurrences(haystack, needle) > 0
}

/// How often a phrase occurs, given one position list per word in order.
///
/// The index half, and it has to answer the same as
/// [`phrase_occurrences`]. A phrase sits at position `p` when the first
/// word is at `p`, the second at `p + 1`, and so on, which is why
/// positions count tokens rather than bytes.
pub fn phrase_hits(positions: &[Vec<u32>]) -> u32 {
    let Some(first) = positions.first() else {
        return 0;
    };
    if positions.len() == 1 {
        return first.len() as u32;
    }
    first
        .iter()
        .filter(|&&start| {
            positions
                .iter()
                .enumerate()
                .skip(1)
                .all(|(offset, list)| list.binary_search(&(start + offset as u32)).is_ok())
        })
        .count() as u32
}

/// A posting: the document's length, then the positions, all little
/// endian u32 and the positions in order.
///
/// Term frequency is the length of the position list and is never stored
/// beside it, so the two cannot disagree. The document's length is
/// stored, and that is a deliberate second copy: scoring needs it for
/// every candidate, and reading it from an entry of its own cost one
/// point lookup per candidate, which is what made a query matching most
/// of the corpus slower than the scan it exists to beat. It is written
/// once per term of the document and `verify_indexes` rebuilds it from
/// the row, so it cannot drift unnoticed (ADR-036).
pub fn encode_posting(length: u32, positions: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + positions.len() * 4);
    out.extend_from_slice(&length.to_le_bytes());
    for p in positions {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

/// Read a posting back, or None if it is not a length and whole
/// positions.
pub fn decode_posting(bytes: &[u8]) -> Option<(u32, Vec<u32>)> {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    Some((words[0], words[1..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(|t| t.term).collect()
    }

    #[test]
    fn a_phrase_is_words_in_sequence() {
        assert_eq!(phrase_occurrences("the quick brown fox", "quick brown"), 1);
        assert_eq!(phrase_occurrences("the quick brown fox", "brown quick"), 0);
        assert_eq!(phrase_occurrences("a b a b a b", "a b"), 3);
        assert_eq!(phrase_occurrences("a b a b a b", "b a"), 2);
        // punctuation between the words does not break the sequence,
        // because positions count tokens
        assert_eq!(phrase_occurrences("quick, brown!", "quick brown"), 1);
        // and a phrase longer than the text cannot fit in it
        assert_eq!(phrase_occurrences("quick", "quick brown"), 0);
    }

    #[test]
    fn an_empty_phrase_is_everywhere_like_an_empty_query() {
        assert!(contains_phrase("anything at all", "   ...  "));
        assert!(contains_phrase("", ""));
        assert_eq!(phrase_occurrences("anything", "  "), 0);
    }

    #[test]
    fn the_index_half_answers_what_the_text_half_answers() {
        // The two are different code over different data and have to
        // agree, so they are checked against each other rather than each
        // against a hand written expectation.
        let texts = [
            "a b a b a b",
            "the quick brown fox jumps over the quick brown dog",
            "one two three",
            "x",
            "",
            "b a b a",
        ];
        let queries = ["a b", "b a", "quick brown", "the quick brown", "one", "z"];
        for text in texts {
            let table = postings(text);
            for query in queries {
                let terms: Vec<String> = tokenize(query).into_iter().map(|t| t.term).collect();
                let lists: Option<Vec<Vec<u32>>> = terms
                    .iter()
                    .map(|term| {
                        table
                            .iter()
                            .find(|(t, _)| t == term)
                            .map(|(_, p)| p.clone())
                    })
                    .collect();
                let from_index = lists.map_or(0, |l| phrase_hits(&l));
                assert_eq!(
                    from_index,
                    phrase_occurrences(text, query),
                    "text {text:?} query {query:?}"
                );
            }
        }
    }

    #[test]
    fn a_posting_survives_a_round_trip_and_junk_is_refused() {
        for case in [vec![], vec![0u32], vec![0, 1, 7, 4_000_000_000]] {
            assert_eq!(decode_posting(&encode_posting(42, &case)), Some((42, case)));
        }
        // too short to hold even a length
        assert_eq!(decode_posting(&[]), None);
        assert_eq!(decode_posting(&[1, 2, 3]), None);
        // a length and part of a position
        assert_eq!(decode_posting(&[1, 2, 3, 4, 5]), None);
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
    fn a_letter_is_whatever_unicode_says_it_is() {
        // Before this, "H\u{e4}user" came out as "h" and "user", and
        // "Stra\u{df}e" as "stra" and "e": a German corpus was shredded
        // rather than merely left unstemmed.
        assert_eq!(
            terms("H\u{e4}user am Flu\u{df}"),
            ["h\u{e4}user", "am", "flu\u{df}"]
        );
        assert_eq!(
            terms("caf\u{e9} r\u{e9}sum\u{e9}"),
            ["caf\u{e9}", "r\u{e9}sum\u{e9}"]
        );
        assert_eq!(terms("\u{c5}NGSTR\u{d6}M"), ["\u{e5}ngstr\u{f6}m"]);

        // A script without spaces still does not segment, which is a
        // different problem and an honest one: the run becomes one term
        // rather than nothing at all.
        assert_eq!(terms("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}").len(), 1);
    }

    #[test]
    fn accents_are_not_folded_away_and_the_record_says_so() {
        // ADR-036 draws the line here: case is Unicode's business and the
        // standard library knows it, while folding an accent away is a
        // table somebody has to write and keep.
        assert_ne!(terms("r\u{e9}sum\u{e9}"), terms("resume"));
        assert_eq!(terms("R\u{c9}SUM\u{c9}"), terms("r\u{e9}sum\u{e9}"));
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
                    token.term.chars().all(|c| c.is_alphanumeric()),
                    "a separator got into a term, seed {seed}: {:?}",
                    token.term
                );
                assert_eq!(
                    token.term,
                    token.term.to_lowercase(),
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
