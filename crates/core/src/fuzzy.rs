//! Subsequence fuzzy matching for quick open (⌘K) and autocomplete ranking.

/// Scores `candidate` against `query` (case-insensitive subsequence match). Higher is better.
/// Returns the matched character indices for highlighting.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let cand: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = cand.iter().map(|c| c.to_ascii_lowercase()).collect();
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }

    // Exact substring matches rank highest, earlier and at word boundaries better.
    let lower_s: String = lower.iter().collect();
    let q_s: String = q.iter().collect();
    if let Some(byte_pos) = lower_s.find(&q_s) {
        let pos = lower_s[..byte_pos].chars().count();
        let boundary = pos == 0 || is_boundary(&cand, pos);
        let mut score = 1000 - pos as i64 * 2 - (cand.len() as i64 - q.len() as i64);
        if boundary {
            score += 200;
        }
        if pos == 0 {
            score += 300;
        }
        if cand.len() == q.len() {
            score += 500;
        }
        return Some((score, (pos..pos + q.len()).collect()));
    }

    // Greedy subsequence with bonuses for boundaries and consecutive runs.
    let mut positions = Vec::with_capacity(q.len());
    let mut score = 0i64;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, &c) in lower.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            score += 10;
            if is_boundary(&cand, i) {
                score += 25;
            }
            if let Some(l) = last {
                if l + 1 == i {
                    score += 15;
                } else {
                    score -= (i - l) as i64;
                }
            } else {
                score -= i as i64;
            }
            positions.push(i);
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| (score - cand.len() as i64 / 4, positions))
}

fn is_boundary(cand: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = cand[i - 1];
    let cur = cand[i];
    matches!(prev, '_' | '.' | ' ' | '-' | '/' | ':') || (prev.is_lowercase() && cur.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_prefix_and_substring() {
        let s = |q, c| fuzzy_match(q, c).map(|(s, _)| s);
        assert!(s("ord", "orders") > s("ord", "order_items"));
        assert!(s("ord", "order_items") > s("ord", "public.border"));
        assert!(s("oi", "order_items").is_some());
        assert!(s("oi", "order_items") > s("oi", "audit_log"));
        assert_eq!(s("xyz", "orders"), None);
        assert_eq!(fuzzy_match("", "abc"), Some((0, vec![])));
        assert_eq!(fuzzy_match("us", "Users").unwrap().1, vec![0, 1]);
        assert_eq!(fuzzy_match("ae", "analytics.events").unwrap().1, vec![0, 10]);
    }
}
