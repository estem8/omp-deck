//! omp's fuzzy matcher for `/tree` search: a port of `FuzzyQuery` / `FuzzyText` from
//! `packages/tui/src/fuzzy.ts` at v18.16.0, reduced to the match decision (the tree filters
//! rows by `matches`; the score only ranks other pickers).
//!
//! Matching is word-local: a query token matches a text when it is
//! * a word start in the text with its spaces removed ("statusline" ~ "status line"),
//! * an exact word, a word prefix, a word that is at most 2 characters shorter than the token and a
//!   prefix of it, or a substring of a word,
//! * the in-order characters of one word spanning at most `max(len + 2, ceil(1.8 * len))`,
//! * a 2–4 character acronym over words at most `len + 2` words apart,
//! * or any of those after swapping one adjacent letter/digit pair of the token ("3d" ~ "d3").
//!
//! JavaScript indexes strings by UTF-16 unit and omp's limits count units, so the normalized text
//! and tokens are `u16` sequences here as well.

/// Minimum needle length for which a later word-start occurrence may rescue a mid-word first hit.
const MIN_SHADOW_RESCAN_LENGTH: usize = 3;

/// `normalizeForSearch`: split camelCase, lowercase, every run of characters that are not letters,
/// marks or numbers becomes one space, trimmed.
fn normalize(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut split = String::with_capacity(value.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            let before = chars[i - 1];
            // `([a-z0-9])([A-Z])` and `([A-Z]+)([A-Z][a-z])`.
            let boundary = before.is_ascii_lowercase()
                || before.is_ascii_digit()
                || (before.is_ascii_uppercase()
                    && chars.get(i + 1).is_some_and(char::is_ascii_lowercase));
            if boundary {
                split.push(' ');
            }
        }
        split.push(c);
    }
    let lower = split.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut gap = false;
    for c in lower.chars() {
        if is_word_char(c) {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

/// `\p{Letter}\p{Mark}\p{Number}`. std has no general category, so marks are the combining blocks
/// plus whatever `is_alphabetic` already covers (the Indic vowel signs).
fn is_word_char(c: char) -> bool {
    c.is_alphabetic()
        || c.is_numeric()
        || matches!(
            c,
            '\u{0300}'..='\u{036F}'
                | '\u{0483}'..='\u{0489}'
                | '\u{0591}'..='\u{05BD}'
                | '\u{064B}'..='\u{065F}'
                | '\u{1AB0}'..='\u{1AFF}'
                | '\u{1DC0}'..='\u{1DFF}'
                | '\u{20D0}'..='\u{20FF}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FE20}'..='\u{FE2F}'
        )
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// A text prepared once for repeated matching (omp `FuzzyText`).
#[derive(Clone, Debug, Default)]
pub struct FuzzyText {
    /// The normalized text, words separated by one space.
    normalized: Vec<u16>,
    /// `normalized` ranges of the words.
    words: Vec<(u32, u32)>,
    /// The normalized text without its spaces.
    compact: Vec<u16>,
    /// Offsets in `compact` where a word starts (strictly increasing).
    compact_starts: Vec<u32>,
}

impl FuzzyText {
    pub fn new(text: &str) -> FuzzyText {
        let normalized = units(&normalize(text));
        let mut words = Vec::new();
        let mut compact = Vec::with_capacity(normalized.len());
        let mut compact_starts = Vec::new();
        if !normalized.is_empty() {
            let mut at = 0usize;
            for word in normalized.split(|&u| u == u16::from(b' ')) {
                words.push((at as u32, (at + word.len()) as u32));
                compact_starts.push(compact.len() as u32);
                compact.extend_from_slice(word);
                at += word.len() + 1;
            }
        }
        FuzzyText {
            normalized,
            words,
            compact,
            compact_starts,
        }
    }

    fn word(&self, i: usize) -> &[u16] {
        let (a, z) = self.words[i];
        &self.normalized[a as usize..z as usize]
    }

    /// `findCompactWordStart(index, needle) >= 0`: an occurrence that starts a word. Only a first
    /// occurrence is considered for needles shorter than [`MIN_SHADOW_RESCAN_LENGTH`].
    fn compact_word_start(&self, needle: &[u16]) -> bool {
        let Some(first) = find(&self.compact, needle, 0) else {
            return false;
        };
        let starts_word = |at: usize| self.compact_starts.binary_search(&(at as u32)).is_ok();
        if starts_word(first) {
            return true;
        }
        if needle.len() < MIN_SHADOW_RESCAN_LENGTH {
            return false;
        }
        let mut from = first + 1;
        while let Some(at) = find(&self.compact, needle, from) {
            if starts_word(at) {
                return true;
            }
            from = at + 1;
        }
        false
    }

    /// `scoreTokenAgainstWord(token, word) !== null`.
    fn word_matches(token: &[u16], word: &[u16]) -> bool {
        if word.starts_with(token) {
            return true; // exact word or prefix
        }
        if token.starts_with(word) && token.len() - word.len() <= 2 {
            return true;
        }
        if find(word, token, 0).is_some() {
            return true;
        }
        // In-order characters; the span is measured on the greedy (earliest) match.
        if token.len() > word.len() {
            return false;
        }
        let mut next = 0;
        let (mut first, mut last) = (usize::MAX, 0);
        for (i, &u) in word.iter().enumerate() {
            if next < token.len() && u == token[next] {
                if first == usize::MAX {
                    first = i;
                }
                last = i;
                next += 1;
            }
        }
        if next < token.len() {
            return false;
        }
        // ceil(len * 1.8) without floating point.
        let max_span = (token.len() + 2).max((token.len() * 9).div_ceil(5));
        last - first < max_span
    }

    /// `scoreAcronym(token, index) !== null`.
    fn acronym(&self, token: &[u16]) -> bool {
        if !(2..=4).contains(&token.len()) || self.words.is_empty() {
            return false;
        }
        let mut matched = 0;
        let (mut first, mut last) = (usize::MAX, 0);
        for ordinal in 0..self.words.len() {
            if self.word(ordinal)[0] != token[matched] {
                continue;
            }
            if first == usize::MAX {
                first = ordinal;
            }
            last = ordinal;
            matched += 1;
            if matched == token.len() {
                break;
            }
        }
        matched == token.len() && last - first < token.len() + 2
    }

    /// `scoreTokenDirect(token, index).matches`.
    fn direct(&self, token: &[u16]) -> bool {
        token.is_empty()
            || self.compact_word_start(token)
            || (0..self.words.len()).any(|i| Self::word_matches(token, self.word(i)))
            || self.acronym(token)
    }
}

/// One normalized query token with its letter/digit swap variants.
#[derive(Clone, Debug)]
struct Token {
    units: Vec<u16>,
    swaps: Vec<Vec<u16>>,
}

impl Token {
    fn new(units: Vec<u16>) -> Token {
        let is_letter = |u: u16| (u16::from(b'a')..=u16::from(b'z')).contains(&u);
        let is_digit = |u: u16| (u16::from(b'0')..=u16::from(b'9')).contains(&u);
        let mut swaps: Vec<Vec<u16>> = Vec::new();
        for i in 0..units.len().saturating_sub(1) {
            let (current, next) = (units[i], units[i + 1]);
            if (is_letter(current) && is_digit(next)) || (is_digit(current) && is_letter(next)) {
                let mut swapped = units.clone();
                swapped.swap(i, i + 1);
                if !swaps.contains(&swapped) {
                    swaps.push(swapped);
                }
            }
        }
        Token { units, swaps }
    }

    fn matches(&self, text: &FuzzyText) -> bool {
        text.direct(&self.units) || self.swaps.iter().any(|s| text.direct(s))
    }
}

/// One whitespace-separated piece of the search box (omp `FuzzyQuery`): normalized and split once.
/// A piece without a letter, mark or number matches every text.
#[derive(Clone, Debug)]
struct FuzzyQuery {
    tokens: Vec<Token>,
}

impl FuzzyQuery {
    fn new(piece: &str) -> FuzzyQuery {
        let normalized = normalize(piece);
        let tokens = normalized
            .split(' ')
            .filter(|t| !t.is_empty())
            .map(|t| Token::new(units(t)))
            .collect();
        FuzzyQuery { tokens }
    }

    fn matches(&self, text: &FuzzyText) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        if text.words.is_empty() {
            return false;
        }
        self.tokens.iter().all(|t| t.matches(text))
    }
}

/// The search box: its lowercased text split at whitespace, one [`FuzzyQuery`] per piece; a row
/// matches when every piece does.
#[derive(Clone, Debug, Default)]
pub struct FuzzySearch {
    pieces: Vec<FuzzyQuery>,
}

impl FuzzySearch {
    pub fn new(query: &str) -> FuzzySearch {
        FuzzySearch {
            pieces: query
                .to_lowercase()
                .split_whitespace()
                .map(FuzzyQuery::new)
                .collect(),
        }
    }

    /// No search words: everything matches.
    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    pub fn matches(&self, text: &FuzzyText) -> bool {
        self.pieces.iter().all(|p| p.matches(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(query: &str, text: &str) -> bool {
        FuzzySearch::new(query).matches(&FuzzyText::new(text))
    }

    /// omp `tui/test/fuzzy.test.ts`: scattered letters across unrelated words do not match.
    #[test]
    fn tree_fuzzy_long_tokens_stay_word_local() {
        let image_provider =
            "Image Provider providers.image openrouter Preferred provider for image generation";
        let block_images =
            "Block Images images.blockImages false Prevent images from being sent to LLM providers";
        let include_model = "Include Model in Prompt includeModelInPrompt true Surface the active model identifier in the system prompt so the agent knows which model it is";
        let service_tier =
            "Service Tier serviceTier openai-only Processing priority hint on supported providers";
        assert!(m("image provider", image_provider));
        assert!(m("image provider", block_images));
        assert!(!m("image provider", include_model));
        assert!(!m("image provider", service_tier));
    }

    /// omp: `fuzzyFilter(["Ollama", "Kagi", "OpenCode Go", "Tavily"], "og")` is only "OpenCode Go".
    #[test]
    fn tree_fuzzy_short_tokens_are_word_initial_acronyms() {
        let hits: Vec<&str> = ["Ollama", "Kagi", "OpenCode Go", "Tavily"]
            .into_iter()
            .filter(|t| m("og", t))
            .collect();
        assert_eq!(hits, ["OpenCode Go"]);
    }

    /// omp: a CJK query filters instead of matching everything.
    #[test]
    fn tree_fuzzy_cjk_queries_filter() {
        assert!(m("搜索", "文件搜索"));
        assert!(m("搜索", "搜索历史"));
        assert!(!m("搜索", "Settings"));
    }

    /// omp `fuzzy-shadowed-occurrence.test.ts`: both texts match, a mid-word hit does not hide
    /// the word-start one.
    #[test]
    fn tree_fuzzy_shadowed_occurrences_still_match() {
        assert!(m("image", "image provider"));
        assert!(m("image", "reimage image provider"));
        assert!(m("statusline", "status line"));
        assert!(m("statusline", "mystatuslinex status line"));
        assert!(m("image", "imagemagick tool"));
    }

    #[test]
    fn tree_fuzzy_normalization_splits_camel_case_and_punctuation() {
        assert_eq!(normalize("includeModelInPrompt"), "include model in prompt");
        assert_eq!(normalize("HTTPServer"), "http server");
        assert_eq!(normalize("v2Beta"), "v2 beta");
        assert_eq!(normalize("  --foo.bar__baz  "), "foo bar baz");
        assert_eq!(normalize("Café über"), "café über");
        assert_eq!(normalize("!!!"), "");
        assert!(m("model in", "includeModelInPrompt"));
    }

    #[test]
    fn tree_fuzzy_token_rules() {
        // Word prefix, and a token at most 2 longer than the word it extends.
        assert!(m("fix", "fixing the login"));
        assert!(m("fixes", "fix login"));
        assert!(!m("fixing", "fix login"));
        // In-order characters inside one word, span limited to max(len + 2, ceil(1.8 len)).
        assert!(m("cfg", "config")); // span 6 ≤ max(5, 6)
        assert!(m("mnfst", "manifest")); // span 8 ≤ max(7, 9)
        assert!(!m("mnfst", "mn ma ni fe st"));
        assert!(!m("cfgx", "config")); // no x
        assert!(!m("gnc", "config")); // letters out of order
        // Letters spread over several single-letter words match as a compact word start.
        assert!(m("abcdef", "a b c d e f"));
        assert!(m("cdz", "a b c d e f")); // a token at most 2 longer than the word "c"
        assert!(!m("cdzzz", "a b c d e f"));
        // Acronym: 2–4 letters, at most len + 2 words wide.
        assert!(m("rt", "read x y tree")); // 4 words ≤ 4
        assert!(!m("rt", "read x y z tree")); // 5 words
    }

    #[test]
    fn tree_fuzzy_digit_letter_swap_retries() {
        assert!(m("d3", "3d printing"));
        assert!(m("3d", "d3 renderer"));
        assert!(!m("d4", "3d printing"));
    }

    #[test]
    fn tree_fuzzy_every_piece_must_match_and_blank_pieces_match_all() {
        assert!(m("login bug", "Fix the Login bug"));
        assert!(!m("login crash", "Fix the Login bug"));
        assert!(m("!! login", "Fix the Login bug"));
        assert!(m("", "anything"));
        assert!(FuzzySearch::new("   ").is_empty());
        // A non-blank query never matches a text without words.
        assert!(!m("a", ""));
        assert!(!m("a", "?!"));
    }
}
