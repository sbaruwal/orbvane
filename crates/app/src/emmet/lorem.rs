//! Lorem ipsum text for Emmet's `lorem` / `loremN` abbreviations (Emmet's Latin vocabulary).

use std::sync::OnceLock;

use super::Rng;

struct Vocabulary {
    common: Vec<String>,
    words: Vec<String>,
}

fn latin() -> &'static Vocabulary {
    static V: OnceLock<Vocabulary> = OnceLock::new();
    V.get_or_init(|| {
        let v: serde_json::Value = serde_json::from_str(include_str!("data/latin.json")).unwrap_or_default();
        let list = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect()).unwrap_or_default();
        Vocabulary { common: list("common"), words: list("words") }
    })
}

fn sentence(mut words: Vec<String>, end: Option<&str>, rng: &mut Rng) -> String {
    if let Some(first) = words.first_mut() {
        let mut cs = first.chars();
        if let Some(c) = cs.next() {
            *first = c.to_uppercase().chain(cs).collect();
        }
    }
    // More dots than question and exclamation marks.
    let end = end.map(str::to_string).unwrap_or_else(|| ["?", "!", ".", "."][rng.range(0, 3)].to_string());
    words.join(" ") + &end
}

fn insert_commas(mut words: Vec<String>, rng: &mut Rng) -> Vec<String> {
    let len = words.len();
    if len < 2 {
        return words;
    }
    let total = if len > 3 && len <= 6 {
        rng.range(0, 1)
    } else if len > 6 && len <= 12 {
        rng.range(0, 2)
    } else {
        rng.range(1, 4)
    };
    for _ in 0..total {
        let pos = rng.range(0, len - 2);
        if !words[pos].ends_with(',') {
            words[pos].push(',');
        }
    }
    words
}

/// A paragraph of `count` words, starting with "Lorem ipsum dolor sit amet" if `common`.
pub(super) fn paragraph(count: usize, common: bool, rng: &mut Rng) -> String {
    let dict = latin();
    let mut out = Vec::new();
    let mut total = 0;
    if common {
        let words: Vec<String> = dict.common.iter().take(count).cloned().collect();
        total += words.len();
        let words = insert_commas(words, rng);
        out.push(sentence(words, Some("."), rng));
    }
    while total < count {
        let n = rng.range(2, 30).min(count - total).min(dict.words.len());
        let mut words: Vec<String> = Vec::new();
        while words.len() < n {
            let w = &dict.words[rng.range(0, dict.words.len())];
            if !words.contains(w) {
                words.push(w.clone());
            }
        }
        total += words.len().max(1);
        let words = insert_commas(words, rng);
        out.push(sentence(words, None, rng));
    }
    out.join(" ")
}
