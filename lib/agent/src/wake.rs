//! Recognising that someone is talking to the agent.
//!
//! While asleep the agent hears transcripts of what is said near the
//! computer. It wakes when it is addressed by name — "Vera, play some
//! music", "Hey Vera", "what time is it, Vera?" — but not when it is only
//! mentioned ("I was telling Anna about Vera yesterday").

use alloc::string::String;
use alloc::vec::Vec;

/// Words people put before a name when calling someone.
const OPENERS: &[&str] =
    &["hey", "hi", "hello", "ok", "okay", "yo", "oh", "so", "um", "uh", "well", "and", "excuse", "me", "dear"];

/// Lower-case words without punctuation.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?' | ';' | ':' | '"'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Edit distance, for names the recogniser spells slightly differently.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = alloc::vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + (ca != cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Whether `word` is the name (allowing one letter off for names of five
/// letters or more, and a possessive "'s").
fn is_name(word: &str, name: &str) -> bool {
    let word = word.strip_suffix("'s").unwrap_or(word);
    word == name || (name.chars().count() >= 5 && distance(word, name) <= 1)
}

/// If `transcript` addresses the agent called `name`, what was asked
/// (possibly nothing: "Hey Vera" just wakes it).
pub fn addressed(transcript: &str, name: &str) -> Option<String> {
    let name_words = words(name);
    let name = name_words.first()?;
    let w = words(transcript);
    // Called at the start, after an opener or two: "Hey Vera, ..."
    let start = w.iter().take_while(|x| OPENERS.contains(&x.as_str())).count();
    if start <= 2 && w.get(start).is_some_and(|x| is_name(x, name)) {
        return Some(after_name(transcript, start + name_words.len()));
    }
    // Called at the end: "... what time is it, Vera?"
    if w.len() >= 2 && w.last().is_some_and(|x| is_name(x, name)) {
        return Some(w[..w.len() - 1].join(" "));
    }
    None
}

/// The transcript after its first `skip` words, keeping its punctuation.
fn after_name(transcript: &str, skip: usize) -> String {
    let mut seen = 0;
    let mut rest = transcript;
    while seen < skip {
        let t = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
        let end =
            t.find(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?' | ';' | ':')).unwrap_or(t.len());
        rest = &t[end..];
        seen += 1;
    }
    String::from(
        rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!' | ';' | ':')).trim_end(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_when_called() {
        assert_eq!(addressed("Vera, play some music.", "Vera").as_deref(), Some("play some music."));
        assert_eq!(addressed("Hey Vera!", "Vera").as_deref(), Some(""));
        assert_eq!(addressed("hey vera what's the weather like", "Vera").as_deref(), Some("what's the weather like"));
        assert_eq!(addressed("Okay so um Vera open my notes", "Vera"), None, "three openers is not calling");
        assert_eq!(addressed("OK Vera, open my notes", "Vera").as_deref(), Some("open my notes"));
        assert_eq!(addressed("What time is it, Vera?", "Vera").as_deref(), Some("what time is it"));
        assert_eq!(addressed("Hey Samantha, hi", "Samantha").as_deref(), Some("hi"));
        // The recogniser may be one letter off on a longer name.
        assert_eq!(addressed("Hey Samanta, lights", "Samantha").as_deref(), Some("lights"));
    }

    #[test]
    fn ignores_mentions() {
        assert_eq!(addressed("I was telling Anna about Vera yesterday", "Vera"), None);
        assert_eq!(addressed("Vera's voice is nice, isn't it", "Vera").as_deref(), Some("voice is nice, isn't it"));
        assert_eq!(addressed("", "Vera"), None);
        assert_eq!(addressed("very nice", "Vera"), None, "short names must match exactly");
        assert_eq!(addressed("Vera", ""), None);
    }
}
