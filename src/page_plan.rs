//! Page planning (design §6.1/§7): parse `"1-3,5"` page specs, resolve the
//! settings' range+parity against a document's page count, and expand the
//! copy/collate job plan. Pure functions only — hosts (`PrintSession::plan`
//! in rword, the dialog panel) and backends (GDI replay loops) both call
//! them.
//!
//! Bounds discipline (spec §7): spec strings ≤500 chars, page numbers
//! capped at 5000, everything rejected up front with a reason string.

use crate::settings::{PageRange, Parity};

/// Maximum accepted page-spec length in characters.
pub const MAX_SPEC_LEN: usize = 500;
/// Maximum page count a document may report (resolve-time guard).
pub const MAX_TOTAL_PAGES: usize = 5000;

/// Validate a page spec's syntax without resolving it (dialog-side use).
/// `Ok(())` for specs that parse, including the empty spec (= no pages).
pub fn validate_page_spec(spec: &str) -> Result<(), String> {
    parse_page_spec(spec).map(|_| ())
}

/// Parse `"1-3,5"` into an ascending, deduplicated list of inclusive page
/// ranges (1-based). Comma-separated segments; each is `N` or `A-B` with
/// `A <= B`; whitespace tolerated. The empty string parses to an empty
/// list (submit-side yields "nothing to print" rather than an error).
pub fn parse_page_spec(spec: &str) -> Result<Vec<std::ops::RangeInclusive<usize>>, String> {
    if spec.chars().count() > MAX_SPEC_LEN {
        return Err(format!(
            "page spec too long: {} chars (max {MAX_SPEC_LEN})",
            spec.chars().count()
        ));
    }
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranges: Vec<std::ops::RangeInclusive<usize>> = Vec::new();
    for segment in spec.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            return Err("empty page-spec segment".to_string());
        }
        let parse_num = |token: &str| -> Result<usize, String> {
            token
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid page number {token:?}"))
                .and_then(|n| {
                    if n == 0 {
                        Err("page numbers start at 1".to_string())
                    } else if n > MAX_TOTAL_PAGES {
                        Err(format!("page number {n} beyond cap {MAX_TOTAL_PAGES}"))
                    } else {
                        Ok(n)
                    }
                })
        };
        let range = match segment.split_once('-') {
            None => {
                let n = parse_num(segment)?;
                n..=n
            }
            Some((a, b)) => {
                let (a, b) = (parse_num(a)?, parse_num(b)?);
                if a > b {
                    return Err(format!("descending range {a}-{b}"));
                }
                a..=b
            }
        };
        ranges.push(range);
    }
    ranges.sort_by_key(|r| *r.start());
    // Merge overlaps (dedupe): sorted input means a merge only ever extends
    // the last kept range.
    let mut merged: Vec<std::ops::RangeInclusive<usize>> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if *r.start() <= *last.end() + 1 => {
                if *r.end() > *last.end() {
                    *last = *last.start()..=*r.end();
                }
            }
            _ => merged.push(r),
        }
    }
    Ok(merged)
}

/// Resolve range + parity into the 1-based page list to print.
/// `current_page` backs `PageRange::Current` (clamped into 1..=total).
pub fn resolve_page_list(
    range: &PageRange,
    parity: Parity,
    total_pages: usize,
    current_page: usize,
) -> Result<Vec<usize>, String> {
    if total_pages == 0 {
        return Err("document has no pages".to_string());
    }
    if total_pages > MAX_TOTAL_PAGES {
        return Err(format!(
            "document has {total_pages} pages (cap {MAX_TOTAL_PAGES})"
        ));
    }
    let selected: Vec<usize> = match range {
        PageRange::All => (1..=total_pages).collect(),
        PageRange::Current => {
            let page = current_page.clamp(1, total_pages);
            vec![page]
        }
        PageRange::Custom { spec } => {
            let mut pages = Vec::new();
            for r in parse_page_spec(spec)? {
                for n in r {
                    if n <= total_pages {
                        pages.push(n);
                    }
                }
            }
            // Ranges merged + sorted by parse; out-of-range filtered —
            // order stays ascending, no duplicates possible.
            pages
        }
    };
    Ok(match parity {
        Parity::All => selected,
        Parity::Odd => selected.into_iter().filter(|n| n % 2 == 1).collect(),
        Parity::Even => selected.into_iter().filter(|n| n % 2 == 0).collect(),
    })
}

/// Expand a page list into per-copy print order (D10 job-side emulation;
/// the payload itself is never multiplied):
/// - collate: one inner list per copy, each the full page order
///   (`[1,2,3], [1,2,3]`).
/// - non-collate: one inner list per PAGE, repeated `copies` times
///   (`[1],[1],[2],[2]`) — the driver/page-major order.
pub fn collate_plan(pages: &[usize], copies: u32, collate: bool) -> Vec<Vec<usize>> {
    if pages.is_empty() || copies == 0 {
        return Vec::new();
    }
    if collate {
        (0..copies).map(|_| pages.to_vec()).collect()
    } else {
        pages
            .iter()
            .flat_map(|p| (0..copies).map(|_| vec![*p]).collect::<Vec<_>>())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_spec() {
        assert_eq!(parse_page_spec("1-3,5").unwrap(), vec![1..=3, 5..=5]);
        assert_eq!(parse_page_spec(" 4 ").unwrap(), vec![4..=4]);
        assert_eq!(parse_page_spec("").unwrap(), Vec::new());
    }

    #[test]
    fn parses_unordered_and_merges() {
        // Unordered input, overlapping and adjacent ranges merge ascending.
        assert_eq!(parse_page_spec("5,1-3,2-4,7").unwrap(), vec![1..=5, 7..=7]);
        assert_eq!(parse_page_spec("1-1").unwrap(), vec![1..=1]);
        assert_eq!(parse_page_spec("1-2,3").unwrap(), vec![1..=3]);
    }

    #[test]
    fn rejects_bad_specs() {
        for bad in ["0", "2-1", "abc", ",,", "1-,", "-2", "1..3", "99999999"] {
            assert!(parse_page_spec(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn rejects_overlong_spec() {
        let spec = "1,".repeat(MAX_SPEC_LEN);
        assert!(parse_page_spec(&spec).is_err());
    }

    #[test]
    fn resolves_ranges_and_parity() {
        use crate::settings::PageRange;
        let all = PageRange::All;
        assert_eq!(resolve_page_list(&all, Parity::All, 5, 1).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(resolve_page_list(&all, Parity::Odd, 5, 1).unwrap(), vec![1, 3, 5]);
        assert_eq!(resolve_page_list(&all, Parity::Even, 5, 1).unwrap(), vec![2, 4]);
        let custom = PageRange::Custom { spec: "1-3,5".into() };
        assert_eq!(
            resolve_page_list(&custom, Parity::All, 10, 1).unwrap(),
            vec![1, 2, 3, 5]
        );
        // Out-of-range entries are dropped silently (Word behavior: print
        // what exists).
        assert_eq!(
            resolve_page_list(&PageRange::Custom { spec: "8-12".into() }, Parity::All, 10, 1).unwrap(),
            vec![8, 9, 10]
        );
    }

    #[test]
    fn current_page_clamps() {
        use crate::settings::PageRange;
        assert_eq!(
            resolve_page_list(&PageRange::Current, Parity::All, 5, 0).unwrap(),
            vec![1]
        );
        assert_eq!(
            resolve_page_list(&PageRange::Current, Parity::All, 5, 99).unwrap(),
            vec![5]
        );
    }

    #[test]
    fn resolve_bounds_errors() {
        use crate::settings::PageRange;
        assert!(resolve_page_list(&PageRange::All, Parity::All, 0, 1).is_err());
        assert!(resolve_page_list(&PageRange::All, Parity::All, MAX_TOTAL_PAGES + 1, 1).is_err());
    }

    #[test]
    fn collate_plan_orders() {
        let pages = vec![1, 2, 3];
        // Collate: copy-major.
        assert_eq!(collate_plan(&pages, 2, true), vec![vec![1, 2, 3], vec![1, 2, 3]]);
        // Non-collate: page-major.
        assert_eq!(
            collate_plan(&pages, 2, false),
            vec![vec![1], vec![1], vec![2], vec![2], vec![3], vec![3]]
        );
        assert!(collate_plan(&pages, 0, true).is_empty());
        assert!(collate_plan(&[], 3, true).is_empty());
    }
}
