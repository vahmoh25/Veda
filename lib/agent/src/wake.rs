//! Recognising that someone is talking to the agent.
//!
//! While asleep the agent hears transcripts of what is said near the
//! computer. It wakes when it is addressed by name — "Vera, play some
//! music", "Hey Vera", "what time is it, Vera?" — but not when it is only
//! mentioned ("I was telling Anna about Vera yesterday").
//!
//! A call starts or ends a sentence (or a clause: the recogniser
//! punctuates), so a transcript that also holds other things — a song's
//! words, someone else talking — still wakes it when the name starts or
//! ends one of its sentences. The recogniser does not always spell the name
//! the same way: one letter off counts too ("Vira"), except for common
//! words ("very").

use alloc::string::String;
use alloc::vec::Vec;

/// Words people put before a name when calling someone.
const OPENERS: &[&str] =
    &["hey", "hi", "hello", "ok", "okay", "yo", "oh", "so", "um", "uh", "well", "and", "excuse", "me", "dear"];

/// Common words never taken for a name one letter off ("very" is not
/// "Vera", "now" is not "Nova").
const COMMON: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "an", "and", "any", "are", "area", "as", "ask", "at", "away",
    "back", "bad", "be", "bear", "been", "best", "big", "both", "but", "by", "call", "came", "can", "car", "care",
    "come", "could", "day", "dear", "did", "do", "does", "done", "down", "each", "else", "era", "even", "ever",
    "every", "eye", "far", "fear", "feel", "few", "fine", "for", "from", "game", "get", "give", "go", "good", "got",
    "had", "has", "have", "he", "hear", "her", "here", "hey", "him", "his", "home", "how", "if", "in", "into", "is",
    "it", "its", "just", "keep", "kind", "know", "last", "let", "like", "line", "look", "lot", "made", "make", "man",
    "many", "may", "me", "mean", "more", "most", "move", "much", "must", "my", "name", "near", "need", "never", "new",
    "next", "nice", "no", "nor", "not", "now", "of", "off", "oh", "ok", "old", "on", "once", "one", "only", "open",
    "or", "our", "out", "over", "own", "part", "play", "put", "real", "right", "said", "same", "say", "see", "seem",
    "she", "show", "so", "some", "song", "stop", "such", "sure", "take", "tell", "than", "that", "the", "them", "then",
    "there", "these", "they", "this", "time", "to", "too", "turn", "two", "up", "us", "use", "vary", "verb", "very",
    "want", "was", "way", "we", "wear", "well", "went", "were", "what", "when", "where", "who", "why", "will", "with",
    "work", "would", "year", "yes", "yet", "you", "your",
];

/// Lower-case words without punctuation.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || is_break(c) || c == '"')
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Punctuation that ends a sentence or a clause.
fn is_break(c: char) -> bool {
    matches!(c, ',' | '.' | '!' | '?' | ';' | ':')
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

/// Whether `word` is the name: exactly, with a possessive "'s", or one
/// letter off for names of four letters or more (unless it is a common
/// word).
fn is_name(word: &str, name: &str) -> bool {
    let word = word.strip_suffix("'s").unwrap_or(word);
    word == name || (name.chars().count() >= 4 && !COMMON.contains(&word) && distance(word, name) <= 1)
}

/// Whether `word` sounds a little like the name (two letters off at most):
/// for the log, when a transcript came close but did not wake the agent.
pub fn resembles(transcript: &str, name: &str) -> bool {
    let Some(name) = words(name).into_iter().next() else { return false };
    words(transcript).iter().any(|w| distance(w, &name) <= 2 && !COMMON.contains(&w.as_str()))
}

/// If `transcript` addresses the agent called `name`, what was asked
/// (possibly nothing: "Hey Vera" just wakes it).
pub fn addressed(transcript: &str, name: &str) -> Option<String> {
    find_call(transcript, name).map(|(_, request)| request)
}

/// If `transcript` addresses the agent called `name`, the transcript from
/// the call on: what comes before it (a song's words, someone else) is
/// left out ("... moonlight. Vera, turn it down." is "Vera, turn it down.").
pub fn call(transcript: &str, name: &str) -> Option<String> {
    find_call(transcript, name).map(|(at, _)| String::from(transcript[at..].trim()))
}

/// Where the call starts in `transcript` (a byte offset) and what was
/// asked.
fn find_call(transcript: &str, name: &str) -> Option<(usize, String)> {
    let name_words = words(name);
    let name = name_words.first()?;
    // Words before each clause, over the whole transcript, and the clause
    // before if a comma joined it to this one (the same sentence).
    let mut before = 0;
    let mut at = 0;
    let mut joined: Option<(usize, Vec<String>)> = None;
    for piece in transcript.split_inclusive(is_break) {
        let w = words(piece);
        // Called at the start, after an opener or two: "Hey Vera, ..."
        let start = w.iter().take_while(|x| OPENERS.contains(&x.as_str())).count();
        if start <= 2 && w.get(start).is_some_and(|x| is_name(x, name)) {
            let rest = after_words(transcript, before + start + name_words.len());
            if !rest.chars().any(char::is_alphanumeric)
                && let Some((joined_at, joined)) = joined
            {
                // Only the name, after the request in the same sentence:
                // "What time is it, Vera?"
                return Some((joined_at, request_before(&joined)));
            }
            if !rest.chars().any(char::is_alphanumeric) {
                return Some((at, String::new()));
            }
            return Some((at, rest));
        }
        // Called at the end: "... what time is it Vera"
        if w.len() >= 2 && w.last().is_some_and(|x| is_name(x, name)) {
            return Some((at, request_before(&w[..w.len() - 1])));
        }
        before += w.len();
        joined = piece.trim_end().ends_with(',').then_some((at, w));
        at += piece.len();
    }
    None
}

/// What was asked before the name: the words, without openers at the end
/// ("play some music, hey" is "play some music").
fn request_before(w: &[String]) -> String {
    let mut end = w.len();
    while end > 0 && OPENERS.contains(&w[end - 1].as_str()) {
        end -= 1;
    }
    w[..end].join(" ")
}

/// The transcript after its first `skip` words, keeping its punctuation.
fn after_words(transcript: &str, skip: usize) -> String {
    let mut seen = 0;
    let mut rest = transcript;
    while seen < skip {
        let t = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
        let end = t.find(|c: char| c.is_whitespace() || is_break(c) || c == '"').unwrap_or(t.len());
        rest = &t[end..];
        seen += 1;
    }
    String::from(rest.trim_start_matches(|c: char| c.is_whitespace() || is_break(c)).trim_end())
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
        // The recogniser may be one letter off.
        assert_eq!(addressed("Hey Samanta, lights", "Samantha").as_deref(), Some("lights"));
        assert_eq!(addressed("Vira, stop the music.", "Vera").as_deref(), Some("stop the music."));
        assert_eq!(addressed("Hey, Bera.", "Vera").as_deref(), Some(""));
    }

    #[test]
    fn wakes_amid_other_words() {
        // A song or someone else before the call, in the same transcript.
        assert_eq!(
            addressed("Dancing in the moonlight. Vera, turn it down.", "Vera").as_deref(),
            Some("turn it down.")
        );
        assert_eq!(addressed("and then we went home. Hey Vera.", "Vera").as_deref(), Some(""));
        assert_eq!(addressed("Play some music, hey Vera?", "Vera").as_deref(), Some("play some music"));
        assert_eq!(addressed("I mean it. What's the time Vera", "Vera").as_deref(), Some("what's the time"));
    }

    #[test]
    fn the_call_without_what_came_before() {
        let call = |t| super::call(t, "Vera");
        assert_eq!(call("Dancing in the moonlight. Vera, turn it down.").as_deref(), Some("Vera, turn it down."));
        assert_eq!(call("Hey Vera, what time is it?").as_deref(), Some("Hey Vera, what time is it?"));
        assert_eq!(call("Oh well. What time is it, Vera?").as_deref(), Some("What time is it, Vera?"));
        assert_eq!(call("I mean it. What's the time Vera").as_deref(), Some("What's the time Vera"));
        assert_eq!(call("I was telling Anna about Vera yesterday"), None);
    }

    #[test]
    fn ignores_mentions() {
        assert_eq!(addressed("I was telling Anna about Vera yesterday", "Vera"), None);
        assert_eq!(addressed("Vera's voice is nice, isn't it", "Vera").as_deref(), Some("voice is nice, isn't it"));
        assert_eq!(addressed("", "Vera"), None);
        assert_eq!(addressed("very nice", "Vera"), None, "a common word is not the name");
        assert_eq!(addressed("Very good. Very nice.", "Vera"), None);
        assert_eq!(addressed("Now play it", "Nova"), None);
        assert_eq!(addressed("Vera", ""), None);
    }

    #[test]
    fn notices_near_misses() {
        assert!(resembles("Hi Mara, how are you", "Vera"));
        assert!(!resembles("very nice indeed", "Vera"));
        assert!(!resembles("play some music", "Vera"));
    }
}
