// HexDB Core Text Analysis
//
// An analyzer turns text into the words a text index stores and a `$text`
// query looks up: a tokenizer splits the text, then filters change the tokens
// in order. A text index names its analyzer (`"analyzer": "english"`); queries
// on that index are analyzed the same way, so they match what was indexed.
//
// Built-in analyzers:
//   standard      letters and digits, lowercased (the default)
//   simple        standard, with accents removed (café = cafe)
//   whitespace    split on spaces only, lowercased (keeps codes like a-1/b)
//   keyword       the whole value as one token, lowercased (exact match)
//   english       simple, without common English words, words stemmed
//                 (running, runs, run all match)
//   ngram         3-letter pieces of each word (matches parts of words)
//   autocomplete  word prefixes of 2 to 15 letters (search as you type)
//
// More can be defined in hexdb.toml without code, as a tokenizer and filters:
//
//   [analyzers.product_code]
//   description = "SKUs: whole code and its prefixes"
//   tokenizer = "whitespace"            # standard, whitespace or keyword
//   filters = ["lowercase", "edge_ngram:3:12"]
//
// Filters: lowercase, ascii_folding, stopwords, stem, ngram:MIN:MAX,
// edge_ngram:MIN:MAX, min_length:N, max_length:N. Every hex in a lattice needs
// the same custom analyzers (indexes are replicated by definition).

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How text is split into tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tokenizer {
    /// Runs of letters and digits.
    #[default]
    Standard,
    /// Runs of non-whitespace characters.
    Whitespace,
    /// The whole text as one token.
    Keyword,
}

/// One step applied to every token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TokenFilter {
    Lowercase,
    AsciiFolding,
    Stopwords,
    Stem,
    NGram(usize, usize),
    EdgeNGram(usize, usize),
    MinLength(usize),
    MaxLength(usize),
}

impl TokenFilter {
    pub fn parse(text: &str) -> Result<TokenFilter> {
        let mut parts = text.trim().split(':');
        let name = parts.next().unwrap_or_default();
        let mut num = |what: &str| -> Result<usize> {
            parts
                .next()
                .ok_or_else(|| anyhow!("{} needs {}", name, what))?
                .parse::<usize>()
                .map_err(|_| anyhow!("{}: {} must be a number", name, what))
        };
        let filter = match name {
            "lowercase" => TokenFilter::Lowercase,
            "ascii_folding" => TokenFilter::AsciiFolding,
            "stopwords" => TokenFilter::Stopwords,
            "stem" => TokenFilter::Stem,
            "ngram" | "edge_ngram" => {
                let (min, max) = (num("a minimum length")?, num("a maximum length")?);
                if min == 0 || max < min || max > 32 {
                    bail!("{}:{}:{}: lengths must be 1 <= min <= max <= 32", name, min, max);
                }
                if name == "ngram" {
                    TokenFilter::NGram(min, max)
                } else {
                    TokenFilter::EdgeNGram(min, max)
                }
            }
            "min_length" => TokenFilter::MinLength(num("a length")?),
            "max_length" => TokenFilter::MaxLength(num("a length")?),
            other => bail!(
                "unknown filter '{}' (use lowercase, ascii_folding, stopwords, stem, ngram:MIN:MAX, edge_ngram:MIN:MAX, min_length:N, max_length:N)",
                other
            ),
        };
        Ok(filter)
    }

    fn label(&self) -> String {
        match self {
            TokenFilter::Lowercase => "lowercase".into(),
            TokenFilter::AsciiFolding => "ascii_folding".into(),
            TokenFilter::Stopwords => "stopwords".into(),
            TokenFilter::Stem => "stem".into(),
            TokenFilter::NGram(a, b) => format!("ngram:{}:{}", a, b),
            TokenFilter::EdgeNGram(a, b) => format!("edge_ngram:{}:{}", a, b),
            TokenFilter::MinLength(n) => format!("min_length:{}", n),
            TokenFilter::MaxLength(n) => format!("max_length:{}", n),
        }
    }
}

/// A custom analyzer in hexdb.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_tokenizer")]
    pub tokenizer: Tokenizer,
    #[serde(default)]
    pub filters: Vec<String>,
}

fn default_tokenizer() -> Tokenizer {
    Tokenizer::Standard
}

/// A tokenizer and filters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Analyzer {
    pub name: String,
    pub description: String,
    pub tokenizer: Tokenizer,
    pub filters: Vec<TokenFilter>,
    pub builtin: bool,
}

const STANDARD: &str = "standard";

impl Analyzer {
    fn builtin(name: &str, description: &str, tokenizer: Tokenizer, filters: Vec<TokenFilter>) -> Analyzer {
        Analyzer { name: name.into(), description: description.into(), tokenizer, filters, builtin: true }
    }

    /// The default analyzer (what text indexes used before analyzers existed).
    pub fn standard() -> Analyzer {
        Analyzer::builtin(STANDARD, "Letters and digits, lowercased.", Tokenizer::Standard, vec![TokenFilter::Lowercase])
    }

    /// The pipeline as text, e.g. `standard | lowercase | stem` (also a
    /// fingerprint: index snapshots made with another pipeline are rebuilt).
    pub fn pipeline(&self) -> String {
        let mut parts = vec![format!("{:?}", self.tokenizer).to_lowercase()];
        parts.extend(self.filters.iter().map(TokenFilter::label));
        parts.join(" | ")
    }

    fn tokenize(&self, text: &str) -> Vec<String> {
        match self.tokenizer {
            Tokenizer::Standard => text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect(),
            Tokenizer::Whitespace => text.split_whitespace().map(String::from).collect(),
            Tokenizer::Keyword => {
                let t = text.trim();
                if t.is_empty() {
                    Vec::new()
                } else {
                    vec![t.to_string()]
                }
            }
        }
    }

    fn run(&self, text: &str, for_query: bool) -> Vec<String> {
        let mut tokens = self.tokenize(text);
        for filter in &self.filters {
            tokens = match filter {
                TokenFilter::Lowercase => tokens.into_iter().map(|t| t.to_lowercase()).collect(),
                TokenFilter::AsciiFolding => tokens.into_iter().map(|t| fold(&t)).collect(),
                TokenFilter::Stopwords => tokens.into_iter().filter(|t| !STOPWORDS.contains(&t.to_lowercase().as_str())).collect(),
                TokenFilter::Stem => tokens.into_iter().map(|t| stem(&t)).collect(),
                TokenFilter::MinLength(n) => tokens.into_iter().filter(|t| t.chars().count() >= *n).collect(),
                TokenFilter::MaxLength(n) => tokens.into_iter().map(|t| t.chars().take(*n).collect()).collect(),
                TokenFilter::NGram(min, max) => tokens.iter().flat_map(|t| ngrams(t, *min, *max, false)).collect(),
                // Prefixes are what's indexed; a query term is looked up as typed.
                TokenFilter::EdgeNGram(min, max) if !for_query => tokens.iter().flat_map(|t| ngrams(t, *min, *max, true)).collect(),
                TokenFilter::EdgeNGram(_, max) => tokens.into_iter().map(|t| t.chars().take(*max).collect()).collect(),
            };
        }
        tokens.retain(|t| !t.is_empty());
        tokens
    }

    /// Tokens stored in a text index for this text (deduplicated).
    pub fn index_tokens(&self, text: &str) -> Vec<String> {
        self.run(text, false).into_iter().collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Tokens a `$text` query looks up (all must match), sorted and deduplicated.
    pub fn query_tokens(&self, text: &str) -> Vec<String> {
        self.run(text, true).into_iter().collect::<BTreeSet<_>>().into_iter().collect()
    }
}

/// Every n-gram of `token` with min..=max characters (only prefixes for edge n-grams).
fn ngrams(token: &str, min: usize, max: usize, edge: bool) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < min {
        // Short tokens are kept whole, so they stay searchable.
        return vec![token.to_string()];
    }
    let mut out = Vec::new();
    let starts = if edge { 0..1 } else { 0..chars.len() };
    for start in starts {
        for len in min..=max {
            if start + len > chars.len() {
                break;
            }
            out.push(chars[start..start + len].iter().collect());
        }
    }
    out
}

/// Remove accents from common Latin letters.
fn fold(token: &str) -> String {
    token
        .chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' => 'A',
            'ç' | 'ć' | 'č' => 'c',
            'Ç' | 'Ć' | 'Č' => 'C',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => 'e',
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ę' | 'Ě' => 'E',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' => 'I',
            'ñ' | 'ń' | 'ň' => 'n',
            'Ñ' | 'Ń' | 'Ň' => 'N',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ő' => 'O',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ů' | 'Ű' => 'U',
            'ý' | 'ÿ' => 'y',
            'Ý' | 'Ÿ' => 'Y',
            'ś' | 'š' | 'ş' => 's',
            'Ś' | 'Š' | 'Ş' => 'S',
            'ź' | 'ż' | 'ž' => 'z',
            'Ź' | 'Ż' | 'Ž' => 'Z',
            'ł' => 'l',
            'Ł' => 'L',
            'ř' => 'r',
            'Ř' => 'R',
            'ď' => 'd',
            'Ď' => 'D',
            'ť' => 't',
            'Ť' => 'T',
            'ß' => 's',
            other => other,
        })
        .collect()
}

/// Common English words that carry little meaning in a search.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "do", "does", "for", "from", "had", "has", "have", "he", "her",
    "his", "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "my", "no", "not", "of", "on", "or", "our", "she", "so", "such",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "too", "us", "very", "was", "we", "were",
    "what", "when", "where", "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

/// A light English stemmer: plural and common verb and adverb endings, so
/// related forms share a stem (searches → search, running → run). It is
/// deliberately conservative: unlike a full Porter stemmer it never maps
/// unrelated words together, at the cost of missing some pairs.
fn stem(word: &str) -> String {
    let w = word.to_string();
    let len = w.chars().count();
    if len <= 3 || !w.chars().all(|c| c.is_alphabetic()) {
        return w;
    }
    let undouble = |s: &str| -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() >= 3 {
            let (a, b) = (chars[chars.len() - 1], chars[chars.len() - 2]);
            if a == b && !"lsz".contains(a) && !"aeiou".contains(a) {
                return chars[..chars.len() - 1].iter().collect();
            }
        }
        s.to_string()
    };
    let has_vowel = |s: &str| s.chars().any(|c| "aeiouy".contains(c));
    if let Some(base) = w.strip_suffix("ies") {
        if base.chars().count() >= 2 {
            return format!("{}y", base);
        }
    }
    for suffix in ["sses", "shes", "ches", "xes", "zes"] {
        if w.ends_with(suffix) {
            return w[..w.len() - 2].to_string();
        }
    }
    if w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        return w[..w.len() - 1].to_string();
    }
    for (suffix, min) in [("ingly", 3), ("edly", 3), ("ing", 3), ("ed", 3), ("ly", 4)] {
        if let Some(base) = w.strip_suffix(suffix) {
            if base.chars().count() >= min && has_vowel(base) {
                return undouble(base);
            }
        }
    }
    w
}

/// The built-in analyzers.
pub fn builtins() -> Vec<Analyzer> {
    use TokenFilter::*;
    vec![
        Analyzer::standard(),
        Analyzer::builtin("simple", "Letters and digits, lowercased, without accents (café = cafe).", Tokenizer::Standard, vec![Lowercase, AsciiFolding]),
        Analyzer::builtin("whitespace", "Split on spaces only, lowercased: keeps codes like A-1/B whole.", Tokenizer::Whitespace, vec![Lowercase]),
        Analyzer::builtin("keyword", "The whole value as one token, lowercased: exact matches.", Tokenizer::Keyword, vec![Lowercase]),
        Analyzer::builtin(
            "english",
            "English text: common words dropped, words stemmed, so search, searches and searching match.",
            Tokenizer::Standard,
            vec![Lowercase, AsciiFolding, Stopwords, Stem],
        ),
        Analyzer::builtin("ngram", "3-letter pieces of each word: matches parts of words (tessel finds tessellation).", Tokenizer::Standard, vec![Lowercase, AsciiFolding, NGram(3, 3)]),
        Analyzer::builtin("autocomplete", "Word prefixes of 2-15 letters: search as you type.", Tokenizer::Standard, vec![Lowercase, AsciiFolding, EdgeNGram(2, 15)]),
    ]
}

/// Every analyzer: the built-in ones, then the configured ones.
pub fn all(custom: &BTreeMap<String, AnalyzerConfig>) -> Vec<Analyzer> {
    let mut list = builtins();
    for (name, config) in custom {
        match from_config(name, config) {
            Ok(analyzer) => list.push(analyzer),
            Err(e) => tracing::warn!("⚠️ Analyzer '{}' in hexdb.toml is invalid: {:#}", name, e),
        }
    }
    list
}

fn from_config(name: &str, config: &AnalyzerConfig) -> Result<Analyzer> {
    if builtins().iter().any(|a| a.name == name) {
        bail!("'{}' is a built-in analyzer; choose another name", name);
    }
    let filters = config.filters.iter().map(|f| TokenFilter::parse(f)).collect::<Result<Vec<_>>>()?;
    Ok(Analyzer { name: name.into(), description: config.description.clone(), tokenizer: config.tokenizer, filters, builtin: false })
}

/// The analyzer with this name (`None` and "" mean standard).
pub fn find(name: Option<&str>, custom: &BTreeMap<String, AnalyzerConfig>) -> Result<Analyzer> {
    let name = name.filter(|n| !n.is_empty()).unwrap_or(STANDARD);
    if let Some(analyzer) = builtins().into_iter().find(|a| a.name == name) {
        return Ok(analyzer);
    }
    match custom.get(name) {
        Some(config) => from_config(name, config),
        None => Err(anyhow!(
            "Unknown analyzer '{}'. Built in: {}; others can be defined under [analyzers] in hexdb.toml.",
            name,
            builtins().iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(name: &str) -> Analyzer {
        find(Some(name), &BTreeMap::new()).unwrap()
    }

    #[test]
    fn standard_matches_the_original_tokenizer() {
        let text = "Hexagons, TESSELLATION and lattice-2026!";
        let old: BTreeSet<String> = crate::filter::tokenize(text).collect();
        assert_eq!(get("standard").index_tokens(text), old.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn analyzers_do_what_they_say() {
        assert_eq!(get("simple").index_tokens("Café Crème"), vec!["cafe", "creme"]);
        assert_eq!(get("whitespace").index_tokens("SKU A-1/B"), vec!["a-1/b", "sku"]);
        assert_eq!(get("keyword").index_tokens("New York"), vec!["new york"]);
        let english = get("english");
        assert_eq!(english.index_tokens("The runners were running searches"), vec!["runner", "run", "search"].into_iter().map(String::from).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>());
        assert_eq!(english.query_tokens("searching"), vec!["search"]);
        let ngram = get("ngram");
        assert!(ngram.index_tokens("tessellation").contains(&"ell".to_string()));
        assert!(ngram.query_tokens("tessel").iter().all(|t| ngram.index_tokens("tessellation").contains(t)), "part of a word matches");
        let auto = get("autocomplete");
        assert!(auto.index_tokens("Hexagon").contains(&"hex".to_string()));
        assert_eq!(auto.query_tokens("hex"), vec!["hex"], "the query isn't expanded");
    }

    #[test]
    fn stemming_is_conservative() {
        for (word, expected) in [("cats", "cat"), ("boxes", "box"), ("studies", "study"), ("running", "run"), ("jumped", "jump"), ("quickly", "quick"), ("glass", "glass"), ("bus", "bus"), ("thing", "thing"), ("sing", "sing")] {
            assert_eq!(stem(word), expected, "{}", word);
        }
    }

    #[test]
    fn custom_analyzers_come_from_config() {
        let mut custom = BTreeMap::new();
        custom.insert("codes".to_string(), AnalyzerConfig { description: "codes".into(), tokenizer: Tokenizer::Whitespace, filters: vec!["lowercase".into(), "edge_ngram:3:5".into()] });
        let codes = find(Some("codes"), &custom).unwrap();
        assert_eq!(codes.index_tokens("ABC123"), vec!["abc", "abc1", "abc12"]);
        assert_eq!(codes.pipeline(), "whitespace | lowercase | edge_ngram:3:5");
        assert!(find(Some("nope"), &custom).is_err());
        assert!(TokenFilter::parse("ngram:0:2").is_err());
        assert!(TokenFilter::parse("soundex").is_err());
        let mut bad = BTreeMap::new();
        bad.insert("english".to_string(), AnalyzerConfig::default());
        assert_eq!(all(&bad).len(), builtins().len(), "a custom analyzer can't replace a built-in");
    }
}
