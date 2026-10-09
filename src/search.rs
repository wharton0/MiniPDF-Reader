//! Text search and string normalization routines.

use std::sync::atomic::{AtomicBool, Ordering};
use crate::types::{CharInfo, SearchMatch, SearchOutput};

/// 1:1 lowercase mapping so search indices stay aligned with char indices.
pub fn lower1(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub fn normalized_query(text: &str) -> String {
    text.trim().chars().map(lower1).collect()
}

pub fn search_page(
    out: &mut SearchOutput,
    page: i32,
    chars: &[CharInfo],
    query: &[char],
    cancel: &AtomicBool,
) {
    if query.is_empty() {
        return;
    }
    let text: Vec<char> = chars.iter().map(|c| lower1(c.ch)).collect();
    let mut start = 0;
    let mut count = 0;
    while start + query.len() <= text.len() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if text[start..start + query.len()] == *query {
            if count >= 500 || out.matches.len() >= 5000 {
                out.truncated = true;
                return;
            }
            let end = start + query.len() - 1;
            out.matches.push(SearchMatch { page, start, end });
            let raw: String = chars[start.saturating_sub(28)..(end + 29).min(chars.len())]
                .iter()
                .take(160)
                .map(|c| c.ch)
                .collect();
            out.snippets.push(format!(
                "p.{} — {}",
                page + 1,
                raw.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
            count += 1;
            start += query.len();
        } else {
            start += 1;
        }
    }
}
