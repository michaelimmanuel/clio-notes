//! Fixes misheard terms in transcript lines using what the screen spells out.
//!
//! Two sources of fixes, both limited to spellings that are on screen:
//! - local: a phrase that equals a screen term once case, spaces and
//!   punctuation are ignored ("power shell", "MCN 10") takes the screen's
//!   spelling. Free, and cannot change the meaning.
//! - from the section writer: pairs of "heard" and "should be" for real
//!   mishearings ("cooper netties"). Each is checked here before it is
//!   applied: the heard phrase must be in the transcript and the
//!   replacement must be on screen.
//!
//! Lines are never rephrased.

use serde::Deserialize;

/// A mishearing reported by the section writer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Correction {
    pub heard: String,
    pub should_be: String,
}

/// Longest phrase, in words, that is compared with a term.
const MAX_WORDS: usize = 4;
/// Terms shorter than this (ignoring spaces and punctuation) are left alone:
/// too easy to hit by accident.
const MIN_TERM_LEN: usize = 4;

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// True for a term like "Home" or "cargo": a plain word whose only
/// capital, if any, is the first letter. Recasing those adds nothing.
fn is_plain_word(term: &str) -> bool {
    term.chars().all(char::is_alphabetic) && term.chars().skip(1).all(char::is_lowercase)
}

/// Rewrites phrases in `text` that match a term apart from case, spacing
/// and punctuation. Returns `None` when nothing changes.
pub fn apply_terms(text: &str, terms: &[String]) -> Option<String> {
    let terms: Vec<(&str, String)> = terms
        .iter()
        .map(|t| (t.trim(), squash(t)))
        .filter(|(_, s)| s.chars().count() >= MIN_TERM_LEN)
        .collect();
    if terms.is_empty() {
        return None;
    }

    // Each word as (start, end) of its core, without surrounding punctuation.
    let mut words = Vec::new();
    let mut offset = 0;
    for piece in text.split_whitespace() {
        let at = offset + text[offset..].find(piece).unwrap();
        offset = at + piece.len();
        let core = piece.trim_matches(|c: char| !c.is_alphanumeric());
        if core.is_empty() {
            continue;
        }
        let start = at + piece.find(core).unwrap();
        words.push((start, start + core.len()));
    }

    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < words.len() {
        let mut matched = None;
        for n in (1..=MAX_WORDS.min(words.len() - i)).rev() {
            let (a, b) = (words[i].0, words[i + n - 1].1);
            let heard = &text[a..b];
            let key = squash(heard);
            let hit = terms.iter().find(|(term, squashed)| {
                *squashed == key && *term != heard && !(n == 1 && is_plain_word(term))
            });
            if let Some((term, _)) = hit {
                matched = Some((a, b, n, *term));
                break;
            }
        }
        match matched {
            Some((a, b, n, term)) => {
                out.push_str(&text[copied..a]);
                out.push_str(term);
                copied = b;
                i += n;
            }
            None => i += 1,
        }
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

/// Whether a correction may be applied: short, a real change, and its
/// replacement appears in `on_screen` (lowercased screen text and terms).
pub fn is_allowed(c: &Correction, on_screen: &str) -> bool {
    let (heard, should_be) = (c.heard.trim(), c.should_be.trim());
    !heard.is_empty()
        && !should_be.is_empty()
        && heard != should_be
        && heard.split_whitespace().count() <= 5
        && should_be.split_whitespace().count() <= 6
        && on_screen.contains(&should_be.to_lowercase())
}

/// Replaces `heard` with `should_be` in `text`, ignoring ASCII case and only
/// at word boundaries. Returns `None` when `heard` is not there.
pub fn replace_phrase(text: &str, heard: &str, should_be: &str) -> Option<String> {
    let (heard, should_be) = (heard.trim(), should_be.trim());
    if heard.is_empty() {
        return None;
    }
    // ASCII lowercasing keeps byte offsets the same as in `text`.
    let hay = text.to_ascii_lowercase();
    let needle = heard.to_ascii_lowercase();
    let is_word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);

    let mut out = String::new();
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = hay[from..].find(&needle) {
        let (a, b) = (from + found, from + found + needle.len());
        let inside_word =
            is_word(text[..a].chars().next_back()) || is_word(text[b..].chars().next());
        if !inside_word && &text[a..b] != should_be {
            out.push_str(&text[copied..a]);
            out.push_str(should_be);
            copied = b;
        }
        from = b;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spacing_case_and_punctuation_take_the_screen_spelling() {
        let t = terms(&["PowerShell", "MCN-10", "Visual Studio Code", "Voicemod"]);
        assert_eq!(
            apply_terms("Open power shell, then pick the MCN 10.", &t).unwrap(),
            "Open PowerShell, then pick the MCN-10."
        );
        assert_eq!(
            apply_terms("I use visual studio code and voice mod", &t).unwrap(),
            "I use Visual Studio Code and Voicemod"
        );
    }

    #[test]
    fn plain_words_short_terms_and_correct_text_are_left_alone() {
        let t = terms(&["Home", "cargo", "AI", "PowerShell"]);
        assert_eq!(
            apply_terms("I went home with the cargo, a I said", &t),
            None
        );
        assert_eq!(apply_terms("PowerShell is open", &t), None);
        assert_eq!(apply_terms("", &t), None);
    }

    #[test]
    fn writer_corrections_must_be_on_screen() {
        let on_screen = "kubernetes cluster overview\nhelm";
        let ok = Correction {
            heard: "cooper netties".into(),
            should_be: "Kubernetes".into(),
        };
        assert!(is_allowed(&ok, on_screen));
        let invented = Correction {
            heard: "the cluster".into(),
            should_be: "OpenShift".into(),
        };
        assert!(!is_allowed(&invented, on_screen));
        let rewrite = Correction {
            heard: "we should probably think about maybe moving".into(),
            should_be: "helm".into(),
        };
        assert!(!is_allowed(&rewrite, on_screen));
    }

    #[test]
    fn phrases_are_replaced_at_word_boundaries() {
        assert_eq!(
            replace_phrase(
                "We run Cooper Netties. cooper netties is great",
                "cooper netties",
                "Kubernetes"
            )
            .unwrap(),
            "We run Kubernetes. Kubernetes is great"
        );
        // "helm" inside "helmet" is not the term.
        assert_eq!(replace_phrase("wear a helmet", "helm", "Helm"), None);
        assert_eq!(replace_phrase("nothing here", "cooper", "Kubernetes"), None);
    }
}
