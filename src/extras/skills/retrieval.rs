//! Pure retrieval metadata and bounded lexical normalization shared with learned skills.

use std::collections::HashSet;

const MAX_QUERY_TERMS: usize = 64;
const LEXICAL_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "for", "from", "get", "give",
    "how", "i", "in", "into", "is", "it", "me", "my", "of", "on", "or", "please", "print", "show",
    "that", "the", "this", "to", "use", "want", "with", "you",
];

/// Immutable metadata about an embedding model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelMetadata {
    pub model_id: String,
    pub model_revision: String,
    pub dimensions: usize,
    pub normalized: bool,
}

pub(crate) fn lexical_tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && !LEXICAL_STOP_WORDS.contains(&term.as_str()))
        .take(MAX_QUERY_TERMS)
        .collect()
}

pub(crate) fn lexical_query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for term in query
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && !LEXICAL_STOP_WORDS.contains(&term.as_str()))
    {
        if seen.insert(term.clone()) {
            terms.push(term);
            if terms.len() == MAX_QUERY_TERMS {
                break;
            }
        }
    }
    terms.sort();
    terms
}

/// The same bounded cache key is used by instruction and learned retrieval.
/// Collapse whitespace without allocating the unbounded original prompt.
pub(crate) fn normalize_query(prompt: &str) -> String {
    const LIMIT: usize = 8 * 1024;
    let mut query = String::with_capacity(prompt.len().min(LIMIT));
    for word in prompt.split_whitespace() {
        if !query.is_empty() {
            if query.len() == LIMIT {
                break;
            }
            query.push(' ');
        }
        let mut end = word.len().min(LIMIT - query.len());
        while !word.is_char_boundary(end) {
            end -= 1;
        }
        query.push_str(&word[..end]);
        if end != word.len() || query.len() == LIMIT {
            break;
        }
    }
    query
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_cache_key_collapses_unicode_whitespace_and_cuts_only_at_utf8_boundary() {
        assert_eq!(
            super::normalize_query(" code\n\t review\u{2003}雪 "),
            "code review 雪"
        );
        let query = super::normalize_query(&format!("{}雪", "x".repeat(8191)));
        assert_eq!(query, "x".repeat(8191));
        assert_eq!(super::normalize_query(&" ".repeat(100_000)), "");
    }
}
