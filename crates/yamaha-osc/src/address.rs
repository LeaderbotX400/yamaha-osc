//! Resolving the channel selector in an OSC address to console indices.
//!
//! The selector is the segment between the strip type and the parameter — the `5` in
//! `/ch/5/fader`. It can be a channel number, a name from the config, or an OSC pattern, and any
//! of those may expand to several channels so one cue can move a group.

use std::collections::{BTreeSet, HashMap};

/// Why a selector could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectorError {
    #[error("channel {given} is below the configured channel_base of {base}")]
    BelowBase { given: u32, base: u32 },
    #[error("channel {given} does not exist (this strip has {count}, numbered {base}..{last})")]
    OutOfRange {
        given: u32,
        count: u32,
        base: u32,
        last: u32,
    },
    #[error("{0:?} is not a channel number, a configured name, or a pattern matching either")]
    Unknown(String),
    #[error("pattern {0:?} matched nothing")]
    NoMatch(String),
}

/// Resolves selectors against the configured names and channel numbering.
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    /// Configured names, already converted to 0-based console indices.
    names: HashMap<String, Vec<u32>>,
    /// The OSC channel number that means console index 0.
    channel_base: u32,
}

impl Resolver {
    pub fn new(names: HashMap<String, Vec<u32>>, channel_base: u32) -> Self {
        Self {
            names,
            channel_base,
        }
    }

    /// Resolve a selector to 0-based console indices.
    ///
    /// `count` is how many instances the strip actually has, used both to reject out-of-range
    /// numbers and to enumerate candidates when the selector is a pattern.
    ///
    /// Results are deduplicated and sorted, so `/ch/{1,1,2}/on 0` mutes two channels once each
    /// rather than sending channel 1 twice.
    pub fn resolve(&self, selector: &str, count: u32) -> Result<Vec<u32>, SelectorError> {
        // A plain number is by far the common case, and must not be shadowed by anything else.
        if let Ok(number) = selector.parse::<u32>() {
            return self.number(number, count).map(|i| vec![i]);
        }

        if let Some(indices) = self.names.get(selector) {
            let valid: Vec<u32> = indices.iter().copied().filter(|&i| i < count).collect();
            if valid.is_empty() {
                return Err(SelectorError::NoMatch(selector.to_owned()));
            }
            return Ok(valid);
        }

        if is_pattern(selector) {
            return self.pattern(selector, count);
        }

        Err(SelectorError::Unknown(selector.to_owned()))
    }

    /// Convert one OSC channel number to a console index.
    fn number(&self, number: u32, count: u32) -> Result<u32, SelectorError> {
        if number < self.channel_base {
            return Err(SelectorError::BelowBase {
                given: number,
                base: self.channel_base,
            });
        }
        let index = number - self.channel_base;
        if index >= count {
            return Err(SelectorError::OutOfRange {
                given: number,
                count,
                base: self.channel_base,
                last: self.channel_base + count - 1,
            });
        }
        Ok(index)
    }

    /// Match a pattern against every channel number and every configured name.
    fn pattern(&self, pattern: &str, count: u32) -> Result<Vec<u32>, SelectorError> {
        let mut matched = BTreeSet::new();

        for index in 0..count {
            let number = index + self.channel_base;
            if matches(pattern, &number.to_string()) {
                matched.insert(index);
            }
        }

        for (name, indices) in &self.names {
            if matches(pattern, name) {
                matched.extend(indices.iter().copied().filter(|&i| i < count));
            }
        }

        if matched.is_empty() {
            return Err(SelectorError::NoMatch(pattern.to_owned()));
        }
        Ok(matched.into_iter().collect())
    }
}

/// Whether a selector contains any OSC pattern metacharacter.
fn is_pattern(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// OSC 1.0 pattern matching: `?` one character, `*` any run, `[abc]` / `[a-z]` / `[!abc]` a
/// character class, `{foo,bar}` alternatives.
///
/// Applied to the selector segment only, not the whole address — the rest of an address is a fixed
/// vocabulary, and matching patterns against it would let `/ch/1/*` fire every parameter at once.
pub fn matches(pattern: &str, literal: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let l: Vec<char> = literal.chars().collect();
    match_at(&p, 0, &l, 0)
}

fn match_at(p: &[char], mut pi: usize, l: &[char], mut li: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                // Try every possible length for the wildcard, shortest first.
                for skip in li..=l.len() {
                    if match_at(p, pi + 1, l, skip) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if li >= l.len() {
                    return false;
                }
                pi += 1;
                li += 1;
            }
            '[' => {
                let Some(close) = find(p, pi + 1, ']') else {
                    // An unterminated class is a literal '[', not a parse error: an operator
                    // typo should mismatch, not take the bridge down.
                    if li >= l.len() || l[li] != '[' {
                        return false;
                    }
                    pi += 1;
                    li += 1;
                    continue;
                };
                if li >= l.len() || !in_class(&p[pi + 1..close], l[li]) {
                    return false;
                }
                pi = close + 1;
                li += 1;
            }
            '{' => {
                let Some(close) = find(p, pi + 1, '}') else {
                    if li >= l.len() || l[li] != '{' {
                        return false;
                    }
                    pi += 1;
                    li += 1;
                    continue;
                };
                let alternatives: String = p[pi + 1..close].iter().collect();
                for alt in alternatives.split(',') {
                    let alt: Vec<char> = alt.chars().collect();
                    if l[li..].starts_with(&alt) && match_at(p, close + 1, l, li + alt.len()) {
                        return true;
                    }
                }
                return false;
            }
            c => {
                if li >= l.len() || l[li] != c {
                    return false;
                }
                pi += 1;
                li += 1;
            }
        }
    }
    li == l.len()
}

fn find(p: &[char], from: usize, target: char) -> Option<usize> {
    p[from..]
        .iter()
        .position(|&c| c == target)
        .map(|i| i + from)
}

/// Whether `c` is in a `[...]` class body, honouring `a-z` ranges and a leading `!` negation.
fn in_class(body: &[char], c: char) -> bool {
    let (negated, body) = match body.first() {
        Some('!') => (true, &body[1..]),
        _ => (false, body),
    };

    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        // A '-' that has a character either side is a range; anywhere else it is a literal.
        if i + 2 < body.len() && body[i + 1] == '-' {
            if body[i] <= c && c <= body[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if body[i] == c {
                hit = true;
            }
            i += 1;
        }
    }

    hit != negated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> Resolver {
        let mut names = HashMap::new();
        names.insert("lead_vox".to_owned(), vec![4]);
        names.insert("band".to_owned(), vec![0, 1, 2, 3]);
        names.insert("band_spare".to_owned(), vec![9]);
        Resolver::new(names, 1)
    }

    #[test]
    fn plain_numbers_convert_from_one_based_to_zero_based() {
        let r = resolver();
        assert_eq!(r.resolve("1", 40).unwrap(), [0]);
        assert_eq!(r.resolve("40", 40).unwrap(), [39]);
    }

    #[test]
    fn channel_zero_is_rejected_when_numbering_is_one_based() {
        let r = resolver();
        assert!(matches!(
            r.resolve("0", 40),
            Err(SelectorError::BelowBase { .. })
        ));
    }

    #[test]
    fn a_channel_past_the_end_is_rejected_with_the_valid_range() {
        let r = resolver();
        let err = r.resolve("41", 40).unwrap_err();
        assert!(err.to_string().contains("1..40"), "{err}");
    }

    #[test]
    fn zero_based_numbering_shifts_the_whole_range() {
        let r = Resolver::new(HashMap::new(), 0);
        assert_eq!(r.resolve("0", 40).unwrap(), [0]);
        assert_eq!(r.resolve("39", 40).unwrap(), [39]);
        assert!(r.resolve("40", 40).is_err());
    }

    #[test]
    fn names_resolve_to_their_indices() {
        let r = resolver();
        assert_eq!(r.resolve("lead_vox", 40).unwrap(), [4]);
        assert_eq!(r.resolve("band", 40).unwrap(), [0, 1, 2, 3]);
    }

    /// A name pointing past a smaller strip's range should degrade to the channels that do exist,
    /// so `/dca/band/on` with band = 1-8 works on an 8-DCA console.
    #[test]
    fn names_are_filtered_to_the_strips_real_range() {
        let r = resolver();
        assert_eq!(r.resolve("band", 2).unwrap(), [0, 1]);
        assert!(matches!(
            r.resolve("band_spare", 2),
            Err(SelectorError::NoMatch(_))
        ));
    }

    #[test]
    fn unknown_selectors_say_so_rather_than_silently_doing_nothing() {
        let r = resolver();
        assert!(matches!(
            r.resolve("lead_vocals", 40),
            Err(SelectorError::Unknown(_))
        ));
    }

    #[test]
    fn star_selects_every_channel() {
        let r = resolver();
        assert_eq!(r.resolve("*", 4).unwrap(), [0, 1, 2, 3]);
    }

    #[test]
    fn brace_alternatives_select_a_list() {
        let r = resolver();
        assert_eq!(r.resolve("{1,3,5}", 40).unwrap(), [0, 2, 4]);
    }

    #[test]
    fn character_classes_select_a_range() {
        let r = resolver();
        assert_eq!(r.resolve("[1-4]", 40).unwrap(), [0, 1, 2, 3]);
    }

    #[test]
    fn patterns_also_match_configured_names() {
        let r = resolver();
        // "band*" matches both "band" (1-4) and "band_spare" (10).
        assert_eq!(r.resolve("band*", 40).unwrap(), [0, 1, 2, 3, 9]);
    }

    #[test]
    fn results_are_deduplicated_and_sorted() {
        let r = resolver();
        assert_eq!(r.resolve("{2,1,2}", 40).unwrap(), [0, 1]);
    }

    #[test]
    fn a_pattern_matching_nothing_is_an_error_not_a_silent_no_op() {
        let r = resolver();
        assert!(matches!(
            r.resolve("9[0-9]", 40),
            Err(SelectorError::NoMatch(_))
        ));
    }

    #[test]
    fn wildcard_basics() {
        assert!(matches("*", "anything"));
        assert!(matches("*", ""));
        assert!(matches("a*c", "abc"));
        assert!(matches("a*c", "ac"));
        assert!(matches("a*c", "abbbc"));
        assert!(!matches("a*c", "abd"));
    }

    #[test]
    fn single_character_wildcard_requires_exactly_one() {
        assert!(matches("?", "a"));
        assert!(!matches("?", ""));
        assert!(!matches("?", "ab"));
        assert!(matches("a?c", "abc"));
    }

    #[test]
    fn character_class_negation_and_ranges() {
        assert!(matches("[abc]", "b"));
        assert!(!matches("[abc]", "d"));
        assert!(matches("[a-z]", "q"));
        assert!(!matches("[a-z]", "Q"));
        assert!(matches("[!a-z]", "Q"));
        assert!(!matches("[!a-z]", "q"));
    }

    #[test]
    fn brace_alternatives_can_be_different_lengths() {
        assert!(matches("{1,10,100}", "10"));
        assert!(matches("{1,10,100}", "100"));
        assert!(!matches("{1,10,100}", "1000"));
    }

    #[test]
    fn combined_metacharacters() {
        assert!(matches("ch[1-3]_*", "ch2_left"));
        assert!(!matches("ch[1-3]_*", "ch4_left"));
    }

    /// A typo in a pattern should mismatch, not panic or take the process down.
    #[test]
    fn unterminated_metacharacters_are_treated_as_literals() {
        assert!(matches("[abc", "[abc"));
        assert!(matches("{a,b", "{a,b"));
        assert!(!matches("[abc", "a"));
    }

    #[test]
    fn a_lone_dash_in_a_class_is_a_literal() {
        assert!(matches("[-a]", "-"));
        assert!(matches("[a-]", "-"));
    }
}
