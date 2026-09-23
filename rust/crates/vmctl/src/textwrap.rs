//! A faithful subset of CPython 3.14's `textwrap` module, ported from
//! `Lib/textwrap.py` so the `argparse` help renderer can reproduce its line
//! breaks byte for byte.
//!
//! Only the behaviour `argparse` relies on is implemented: the default
//! `TextWrapper` settings (`break_long_words=True`, `break_on_hyphens=True`,
//! `drop_whitespace=True`, `expand_tabs=True`, `replace_whitespace=True`) plus
//! `initial_indent` / `subsequent_indent`. The public surface is `TextWrapper`
//! with `wrap` and `fill`.

const TAB_SIZE: usize = 8;

/// US-ASCII whitespace, matching Python's `_whitespace`.
fn is_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ')
}

/// Python `\w` for a `str` pattern: `str.isalnum()` or `_`.
///
/// Rust's `char::is_alphanumeric` additionally reports the Unicode `Alphabetic`
/// property's `Other_Alphabetic` marks, which are not in category `L*`.
/// `NON_WORD_ALPHABETIC_RANGES` is exactly that difference. Both tables were
/// generated from CPython 3.14.7 and are checked over every code point by
/// `predicates_match_python_over_every_code_point`.
fn is_word(character: char) -> bool {
    if character == '_' {
        return true;
    }
    if !character.is_alphanumeric() {
        return false;
    }
    !in_ranges(character, NON_WORD_ALPHABETIC_RANGES)
}

/// Python `\d` for a `str` pattern: a Unicode decimal digit (category `Nd`).
fn is_digit(character: char) -> bool {
    let code = character as u32;
    if code <= 0x39 {
        return code >= 0x30;
    }
    in_ranges(character, DECIMAL_DIGIT_RANGES)
}

fn in_ranges(character: char, ranges: &[(u32, u32)]) -> bool {
    let code = character as u32;
    ranges
        .binary_search_by(|&(start, end)| {
            if code < start {
                std::cmp::Ordering::Greater
            } else if code > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Code points where Rust's `Alphabetic` property exceeds Python's `str.isalnum()`.
const NON_WORD_ALPHABETIC_RANGES: &[(u32, u32)] = &[
    (0x0345, 0x0345),
    (0x0363, 0x036F),
    (0x0558, 0x0558),
    (0x058B, 0x058C),
    (0x05B0, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C9),
    (0x0610, 0x061A),
    (0x064B, 0x0657),
    (0x0659, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06E1, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06ED, 0x06ED),
    (0x0711, 0x0711),
    (0x0730, 0x073F),
    (0x07A6, 0x07B0),
    (0x0816, 0x0817),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082C),
    (0x088F, 0x088F),
    (0x0897, 0x0897),
    (0x08D4, 0x08DF),
    (0x08E3, 0x08E9),
    (0x08F0, 0x0903),
    (0x093A, 0x093B),
    (0x093E, 0x094C),
    (0x094E, 0x094F),
    (0x0955, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0983),
    (0x09BE, 0x09C4),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CC),
    (0x09D7, 0x09D7),
    (0x09E2, 0x09E3),
    (0x0A01, 0x0A03),
    (0x0A3E, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4C),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A83),
    (0x0ABE, 0x0AC5),
    (0x0AC7, 0x0AC9),
    (0x0ACB, 0x0ACC),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFC),
    (0x0B01, 0x0B03),
    (0x0B3E, 0x0B44),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4C),
    (0x0B56, 0x0B57),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BBE, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCC),
    (0x0BD7, 0x0BD7),
    (0x0C00, 0x0C04),
    (0x0C3E, 0x0C44),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4C),
    (0x0C55, 0x0C56),
    (0x0C5C, 0x0C5C),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C83),
    (0x0CBE, 0x0CC4),
    (0x0CC6, 0x0CC8),
    (0x0CCA, 0x0CCC),
    (0x0CD5, 0x0CD6),
    (0x0CDC, 0x0CDC),
    (0x0CE2, 0x0CE3),
    (0x0CF3, 0x0CF3),
    (0x0D00, 0x0D03),
    (0x0D3E, 0x0D44),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4C),
    (0x0D57, 0x0D57),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D83),
    (0x0DCF, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0DD8, 0x0DDF),
    (0x0DF2, 0x0DF3),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E4D, 0x0E4D),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EB9),
    (0x0EBB, 0x0EBC),
    (0x0ECD, 0x0ECD),
    (0x0F71, 0x0F83),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x102B, 0x1036),
    (0x1038, 0x1038),
    (0x103B, 0x103E),
    (0x1056, 0x1059),
    (0x105E, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106D),
    (0x1071, 0x1074),
    (0x1082, 0x108D),
    (0x108F, 0x108F),
    (0x109A, 0x109D),
    (0x1712, 0x1713),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B6, 0x17C8),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x192B),
    (0x1930, 0x1938),
    (0x1A17, 0x1A1B),
    (0x1A55, 0x1A5E),
    (0x1A61, 0x1A74),
    (0x1ABF, 0x1AC0),
    (0x1ACC, 0x1ACE),
    (0x1B00, 0x1B04),
    (0x1B35, 0x1B43),
    (0x1B80, 0x1B82),
    (0x1BA1, 0x1BA9),
    (0x1BAC, 0x1BAD),
    (0x1BE7, 0x1BF1),
    (0x1C24, 0x1C36),
    (0x1DD3, 0x1DF4),
    (0x208F, 0x208F),
    (0x209D, 0x209F),
    (0x24B6, 0x24E9),
    (0x2DE0, 0x2DFF),
    (0xA674, 0xA67B),
    (0xA69E, 0xA69F),
    (0xA7CE, 0xA7CF),
    (0xA7D2, 0xA7D2),
    (0xA7D4, 0xA7D4),
    (0xA7DD, 0xA7DD),
    (0xA7E2, 0xA7E2),
    (0xA7F1, 0xA7F1),
    (0xA802, 0xA802),
    (0xA80B, 0xA80B),
    (0xA823, 0xA827),
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C3),
    (0xA8C5, 0xA8C5),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92A),
    (0xA947, 0xA952),
    (0xA980, 0xA983),
    (0xA9B4, 0xA9BF),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4D),
    (0xAA7B, 0xAA7D),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABE),
    (0xAAEB, 0xAAEF),
    (0xAAF5, 0xAAF5),
    (0xAB6C, 0xAB6D),
    (0xABE3, 0xABEA),
    (0xFB1E, 0xFB1E),
    (0x10376, 0x1037A),
    (0x107BB, 0x107BF),
    (0x10940, 0x10959),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D69),
    (0x10EAB, 0x10EAC),
    (0x10EC5, 0x10EC7),
    (0x10ECB, 0x10ECD),
    (0x10ED9, 0x10EEE),
    (0x10EF3, 0x10EF3),
    (0x10EF5, 0x10EF5),
    (0x10EF7, 0x10EF8),
    (0x10EFA, 0x10EFC),
    (0x11000, 0x11002),
    (0x11038, 0x11045),
    (0x11073, 0x11074),
    (0x11080, 0x11082),
    (0x110B0, 0x110B8),
    (0x110C2, 0x110C2),
    (0x11100, 0x11102),
    (0x11127, 0x11132),
    (0x11145, 0x11146),
    (0x11180, 0x11182),
    (0x111B3, 0x111BF),
    (0x111CE, 0x111CF),
    (0x1122C, 0x11234),
    (0x11237, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112E8),
    (0x11300, 0x11303),
    (0x1133E, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134C),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x113B8, 0x113C0),
    (0x113C2, 0x113C2),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113CA),
    (0x113CC, 0x113CD),
    (0x11435, 0x11441),
    (0x11443, 0x11445),
    (0x114B0, 0x114C1),
    (0x115AF, 0x115B5),
    (0x115B8, 0x115BE),
    (0x115DC, 0x115DD),
    (0x11630, 0x1163E),
    (0x11640, 0x11640),
    (0x116AB, 0x116B5),
    (0x1171D, 0x1172A),
    (0x1182C, 0x11838),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193B, 0x1193C),
    (0x11940, 0x11940),
    (0x11942, 0x11942),
    (0x119D1, 0x119D7),
    (0x119DA, 0x119DF),
    (0x119E4, 0x119E4),
    (0x11A01, 0x11A0A),
    (0x11A35, 0x11A39),
    (0x11A3B, 0x11A3E),
    (0x11A51, 0x11A5B),
    (0x11A8A, 0x11A97),
    (0x11B0A, 0x11B0A),
    (0x11B60, 0x11B67),
    (0x11C2F, 0x11C36),
    (0x11C38, 0x11C3E),
    (0x11C92, 0x11CA7),
    (0x11CA9, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D41),
    (0x11D43, 0x11D43),
    (0x11D47, 0x11D47),
    (0x11D8A, 0x11D8E),
    (0x11D90, 0x11D91),
    (0x11D93, 0x11D96),
    (0x11DB0, 0x11DDB),
    (0x11DE0, 0x11DE9),
    (0x11DF0, 0x11DF1),
    (0x11EF3, 0x11EF6),
    (0x11F00, 0x11F01),
    (0x11F03, 0x11F03),
    (0x11F34, 0x11F3A),
    (0x11F3E, 0x11F40),
    (0x1246F, 0x1246F),
    (0x12475, 0x1247F),
    (0x12550, 0x12686),
    (0x1611E, 0x1612E),
    (0x16EA0, 0x16EB8),
    (0x16EBB, 0x16ED3),
    (0x16F4F, 0x16F4F),
    (0x16F51, 0x16F87),
    (0x16F8F, 0x16F92),
    (0x16FF0, 0x16FF6),
    (0x187F8, 0x187FF),
    (0x18CD6, 0x18CDA),
    (0x18D09, 0x18D20),
    (0x18D80, 0x18DF2),
    (0x18E00, 0x19191),
    (0x191A0, 0x191D2),
    (0x1B123, 0x1B128),
    (0x1B168, 0x1B168),
    (0x1BC9E, 0x1BC9E),
    (0x1D6A6, 0x1D6A6),
    (0x1DF1F, 0x1DF24),
    (0x1DF2B, 0x1DF81),
    (0x1DF90, 0x1DF96),
    (0x1DFCD, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E6C0, 0x1E6DE),
    (0x1E6E0, 0x1E6F5),
    (0x1E6FE, 0x1E6FF),
    (0x1E947, 0x1E947),
    (0x1F130, 0x1F149),
    (0x1F150, 0x1F169),
    (0x1F170, 0x1F189),
    (0x2B73A, 0x2B73F),
    (0x2B81E, 0x2B81E),
    (0x2CEA2, 0x2CEAD),
    (0x323B0, 0x33479),
    (0x3D000, 0x3FC3F),
];

/// Python `\\d` (category `Nd`) ranges, excluding ASCII digits.
const DECIMAL_DIGIT_RANGES: &[(u32, u32)] = &[
    (0x0030, 0x0039),
    (0x0660, 0x0669),
    (0x06F0, 0x06F9),
    (0x07C0, 0x07C9),
    (0x0966, 0x096F),
    (0x09E6, 0x09EF),
    (0x0A66, 0x0A6F),
    (0x0AE6, 0x0AEF),
    (0x0B66, 0x0B6F),
    (0x0BE6, 0x0BEF),
    (0x0C66, 0x0C6F),
    (0x0CE6, 0x0CEF),
    (0x0D66, 0x0D6F),
    (0x0DE6, 0x0DEF),
    (0x0E50, 0x0E59),
    (0x0ED0, 0x0ED9),
    (0x0F20, 0x0F29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x17E0, 0x17E9),
    (0x1810, 0x1819),
    (0x1946, 0x194F),
    (0x19D0, 0x19D9),
    (0x1A80, 0x1A89),
    (0x1A90, 0x1A99),
    (0x1B50, 0x1B59),
    (0x1BB0, 0x1BB9),
    (0x1C40, 0x1C49),
    (0x1C50, 0x1C59),
    (0xA620, 0xA629),
    (0xA8D0, 0xA8D9),
    (0xA900, 0xA909),
    (0xA9D0, 0xA9D9),
    (0xA9F0, 0xA9F9),
    (0xAA50, 0xAA59),
    (0xABF0, 0xABF9),
    (0xFF10, 0xFF19),
    (0x104A0, 0x104A9),
    (0x10D30, 0x10D39),
    (0x10D40, 0x10D49),
    (0x11066, 0x1106F),
    (0x110F0, 0x110F9),
    (0x11136, 0x1113F),
    (0x111D0, 0x111D9),
    (0x112F0, 0x112F9),
    (0x11450, 0x11459),
    (0x114D0, 0x114D9),
    (0x11650, 0x11659),
    (0x116C0, 0x116C9),
    (0x116D0, 0x116E3),
    (0x11730, 0x11739),
    (0x118E0, 0x118E9),
    (0x11950, 0x11959),
    (0x11BF0, 0x11BF9),
    (0x11C50, 0x11C59),
    (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9),
    (0x11F50, 0x11F59),
    (0x16130, 0x16139),
    (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9),
    (0x16B50, 0x16B59),
    (0x16D70, 0x16D79),
    (0x1CCF0, 0x1CCF9),
    (0x1D7CE, 0x1D7FF),
    (0x1E140, 0x1E149),
    (0x1E2F0, 0x1E2F9),
    (0x1E4F0, 0x1E4F9),
    (0x1E5F1, 0x1E5FA),
    (0x1E950, 0x1E959),
    (0x1FBF0, 0x1FBF9),
];

/// Python `letter = [^\d\W]`: a word character that is not a digit.
fn is_letter(character: char) -> bool {
    is_word(character) && !is_digit(character)
}

/// Python `word_punct = [\w!"\'&.,?]`.
fn is_word_punct(character: char) -> bool {
    is_word(character) || matches!(character, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

/// `TextWrapper`, configured with the argparse defaults.
pub struct TextWrapper {
    pub width: usize,
    pub initial_indent: String,
    pub subsequent_indent: String,
    pub expand_tabs: bool,
    pub replace_whitespace: bool,
    pub break_long_words: bool,
    pub drop_whitespace: bool,
    pub break_on_hyphens: bool,
    pub tabsize: usize,
}

impl TextWrapper {
    /// `textwrap.wrap(text, width)` / `textwrap.fill(text, width)`.
    pub fn new(width: usize) -> Self {
        Self {
            width,
            initial_indent: String::new(),
            subsequent_indent: String::new(),
            expand_tabs: true,
            replace_whitespace: true,
            break_long_words: true,
            drop_whitespace: true,
            break_on_hyphens: true,
            tabsize: TAB_SIZE,
        }
    }

    /// `textwrap.fill(text, width, initial_indent=indent, subsequent_indent=indent)`.
    pub fn with_indents(width: usize, initial_indent: &str, subsequent_indent: &str) -> Self {
        Self {
            initial_indent: initial_indent.to_string(),
            subsequent_indent: subsequent_indent.to_string(),
            ..Self::new(width)
        }
    }

    /// `TextWrapper.wrap(text)`.
    pub fn wrap(&self, text: &str) -> Vec<String> {
        let chunks = self.split_chunks(text);
        self.wrap_chunks(chunks)
    }

    /// `TextWrapper.fill(text)`.
    pub fn fill(&self, text: &str) -> String {
        self.wrap(text).join("\n")
    }

    fn munge_whitespace(&self, text: &str) -> String {
        let mut expanded = String::with_capacity(text.len());
        if self.expand_tabs {
            let mut column = 0usize;
            for character in text.chars() {
                match character {
                    '\t' => {
                        let spaces = self.tabsize - (column % self.tabsize);
                        for _ in 0..spaces {
                            expanded.push(' ');
                        }
                        column += spaces;
                    }
                    '\n' | '\r' => {
                        expanded.push(character);
                        column = 0;
                    }
                    other => {
                        expanded.push(other);
                        column += 1;
                    }
                }
            }
        } else {
            expanded.push_str(text);
        }
        if self.replace_whitespace {
            expanded
                .chars()
                .map(|character| {
                    if is_whitespace(character) {
                        ' '
                    } else {
                        character
                    }
                })
                .collect()
        } else {
            expanded
        }
    }

    fn split_chunks(&self, text: &str) -> Vec<String> {
        let text = self.munge_whitespace(text);
        if self.break_on_hyphens {
            self.split_on_wordsep(&text)
        } else {
            self.split_on_simple_whitespace(&text)
        }
    }

    /// Python `wordsep_simple_re.split`, which splits on runs of whitespace.
    fn split_on_simple_whitespace(&self, text: &str) -> Vec<String> {
        let mut chunks = Vec::new();
        let mut current = String::new();
        for character in text.chars() {
            if is_whitespace(character) {
                current.push(character);
            } else {
                if !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                current.push(character);
            }
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        chunks
    }

    /// Python `wordsep_re.split`: partition the text into whitespace runs and
    /// words, breaking a hyphenated word after its hyphen.
    fn split_on_wordsep(&self, text: &str) -> Vec<String> {
        let characters: Vec<char> = text.chars().collect();
        let mut chunks = Vec::new();
        let mut position = 0usize;
        while position < characters.len() {
            // `[ws]+`: a run of whitespace.
            if is_whitespace(characters[position]) {
                let start = position;
                while position < characters.len() && is_whitespace(characters[position]) {
                    position += 1;
                }
                chunks.push(characters[start..position].iter().collect());
                continue;
            }

            // `(?<=word_punct) -{2,} (?=\w)`: an em-dash between words.
            if characters[position] == '-'
                && position > 0
                && is_word_punct(characters[position - 1])
            {
                let start = position;
                let mut end = position;
                while end < characters.len() && characters[end] == '-' {
                    end += 1;
                }
                if end - start >= 2 && end < characters.len() && is_word(characters[end]) {
                    chunks.push(characters[start..end].iter().collect());
                    position = end;
                    continue;
                }
            }

            // `[^ws]+?` followed by the end-of-word / hyphenated-word /
            // em-dash condition. Lazy matching picks the shortest chunk for
            // which one of the conditions holds.
            let start = position;
            let mut end = None;
            let mut cursor = start + 1;
            loop {
                // Hyphenated word: `-(?: (?<=letter{2}-) | (?<=letter-letter-) ) (?=letter -? letter)`.
                if cursor < characters.len() && characters[cursor] == '-' {
                    let hyphen = cursor;
                    let two_letters = hyphen >= 2
                        && is_letter(characters[hyphen - 1])
                        && is_letter(characters[hyphen - 2]);
                    let letter_hyphen_letter = hyphen >= 3
                        && is_letter(characters[hyphen - 3])
                        && characters[hyphen - 2] == '-'
                        && is_letter(characters[hyphen - 1]);
                    if two_letters || letter_hyphen_letter {
                        let after = hyphen + 1;
                        let lookahead = after < characters.len()
                            && is_letter(characters[after])
                            && if after + 1 < characters.len() && characters[after + 1] == '-' {
                                after + 2 < characters.len() && is_letter(characters[after + 2])
                            } else {
                                after + 1 < characters.len() && is_letter(characters[after + 1])
                            };
                        if lookahead {
                            end = Some(hyphen + 1);
                            break;
                        }
                    }
                }

                // End of word: `(?=ws|\z)`.
                if cursor == characters.len() || is_whitespace(characters[cursor]) {
                    end = Some(cursor);
                    break;
                }

                // Em-dash after a word character: `(?<=word_punct) (?=-{2,}\w)`.
                if cursor < characters.len() && cursor > 0 && is_word_punct(characters[cursor - 1])
                {
                    let mut run = cursor;
                    while run < characters.len() && characters[run] == '-' {
                        run += 1;
                    }
                    if run - cursor >= 2 && run < characters.len() && is_word(characters[run]) {
                        end = Some(cursor);
                        break;
                    }
                }

                if cursor >= characters.len() {
                    break;
                }
                cursor += 1;
            }

            let end = end.unwrap_or(characters.len());
            chunks.push(characters[start..end].iter().collect());
            position = end.max(start + 1);
        }
        chunks
    }

    fn handle_long_word(
        &self,
        reversed_chunks: &mut Vec<String>,
        cur_line: &mut Vec<String>,
        cur_len: i64,
        width: i64,
    ) {
        let space_left = if width < 1 { 1 } else { width - cur_len };

        if self.break_long_words && space_left > 0 {
            let mut end = space_left as usize;
            let chunk = reversed_chunks.last().cloned().unwrap_or_default();
            let characters: Vec<char> = chunk.chars().collect();
            if self.break_on_hyphens && characters.len() > space_left as usize {
                // Break after the last hyphen that leaves non-hyphen text
                // before it, searching only within `chunk[:space_left]`.
                let limit = (space_left as usize).min(characters.len());
                let hyphen = (0..limit).rev().find(|&index| characters[index] == '-');
                if let Some(hyphen) = hyphen {
                    if hyphen > 0 && characters[..hyphen].iter().any(|&c| c != '-') {
                        end = hyphen + 1;
                    }
                }
            }
            let end = end.min(characters.len());
            cur_line.push(characters[..end].iter().collect());
            let remainder: String = characters[end..].iter().collect();
            if let Some(last) = reversed_chunks.last_mut() {
                *last = remainder;
            }
        } else if cur_line.is_empty() {
            if let Some(chunk) = reversed_chunks.pop() {
                cur_line.push(chunk);
            }
        }
    }

    fn wrap_chunks(&self, chunks: Vec<String>) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut chunks = chunks;
        chunks.reverse();

        while !chunks.is_empty() {
            let mut cur_line: Vec<String> = Vec::new();
            let mut cur_len: i64 = 0;
            let indent = if !lines.is_empty() {
                self.subsequent_indent.clone()
            } else {
                self.initial_indent.clone()
            };
            let width = self.width as i64 - indent.chars().count() as i64;

            // Drop leading whitespace unless this is the very first line.
            if self.drop_whitespace
                && chunks
                    .last()
                    .map(|chunk| chunk.trim().is_empty())
                    .unwrap_or(false)
                && !lines.is_empty()
            {
                chunks.pop();
            }

            while !chunks.is_empty() {
                let length = chunks.last().unwrap().chars().count() as i64;
                if cur_len + length <= width {
                    cur_line.push(chunks.pop().unwrap());
                    cur_len += length;
                } else {
                    break;
                }
            }

            if !chunks.is_empty() && chunks.last().unwrap().chars().count() as i64 > width {
                self.handle_long_word(&mut chunks, &mut cur_line, cur_len, width);
            }

            if self.drop_whitespace
                && !cur_line.is_empty()
                && cur_line.last().unwrap().trim().is_empty()
            {
                cur_line.pop();
            }

            if !cur_line.is_empty() {
                lines.push(format!("{}{}", indent, cur_line.concat()));
            }
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, width: usize) -> Vec<String> {
        TextWrapper::new(width).wrap(text)
    }

    /// References produced by CPython 3.14.7 `textwrap.wrap` with defaults.
    #[test]
    fn splits_and_wraps_like_cpython() {
        assert_eq!(
            wrap("Hello there -- you goof-ball, use the -b option!", 20),
            ["Hello there -- you", "goof-ball, use the", "-b option!"]
        );
        assert_eq!(
            wrap(
                "block until a lease is running (useful after --no-wait)",
                54
            ),
            [
                "block until a lease is running (useful after --no-",
                "wait)"
            ]
        );
        assert_eq!(
            wrap("default | none | <env-pack-name> (credential env vars)", 30),
            ["default | none | <env-pack-", "name> (credential env vars)"]
        );
    }

    /// Reference produced by CPython 3.14.7 `textwrap.fill`.
    #[test]
    fn fill_honors_initial_and_subsequent_indent() {
        let wrapper = TextWrapper::with_indents(20, "  ", "  ");
        assert_eq!(
            wrapper.fill("describe acquisition and console prerequisites"),
            "  describe\n  acquisition and\n  console\n  prerequisites"
        );
    }

    /// FNV-1a over each predicate's result for every code point except the
    /// surrogate range, which no `char` can hold. The expected values are
    /// CPython 3.14.7's `re.fullmatch(r'\w', chr(cp))` and `r'\d'`.
    #[test]
    fn predicates_match_python_over_every_code_point() {
        let mut word_hash: u64 = 0xcbf29ce484222325;
        let mut digit_hash: u64 = 0xcbf29ce484222325;
        let mut word_count = 0u64;
        let mut digit_count = 0u64;
        for code in 0..0x110000u32 {
            if (0xD800..=0xDFFF).contains(&code) {
                continue;
            }
            let Some(character) = char::from_u32(code) else {
                continue;
            };
            let word = is_word(character);
            let digit = is_digit(character);
            word_count += u64::from(word);
            digit_count += u64::from(digit);
            for (hash, bit) in [(&mut word_hash, word), (&mut digit_hash, digit)] {
                *hash ^= u64::from(bit as u8) + b'0' as u64;
                *hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        assert_eq!(word_count, 142940, "word count");
        assert_eq!(digit_count, 760, "digit count");
        assert_eq!(word_hash, 0xe75e3e04c9a15a79, "\\w hash");
        assert_eq!(digit_hash, 0xa35767a1aa686821, "\\d hash");
    }
}
