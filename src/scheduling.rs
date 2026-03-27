use crate::domain::Issue;

/// Build a dependency graph from candidate issues and compute transitive block
/// counts via BFS, mirroring Venator's `computeTopBlockers` algorithm.
///
/// Returns a map of `identifier → count` where count is the number of active
/// issues transitively blocked by the given issue. Only entries with count > 0
/// are included.
pub fn compute_transitive_block_counts(
    issues: &[Issue],
    terminal_states: &[String],
) -> std::collections::HashMap<String, usize> {
    use std::collections::{HashMap, HashSet, VecDeque};

    // Known universe of identifiers.
    let universe: HashSet<&str> = issues.iter().map(|i| i.identifier.as_str()).collect();

    // Build forward adjacency (blocker → set of issues it blocks) by inverting
    // each issue's `blocked_by` field. Track which issues have an active blocker.
    let mut forward: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut has_active_blocker: HashSet<&str> = HashSet::new();

    for issue in issues {
        for bref in &issue.blocked_by {
            let blocker_id = match bref.identifier.as_deref() {
                Some(id) => id,
                None => continue,
            };
            if !universe.contains(blocker_id) {
                continue;
            }
            // Skip terminal blockers — they're done.
            if let Some(ref st) = bref.state {
                if terminal_states.contains(&st.to_lowercase()) {
                    continue;
                }
            }
            forward
                .entry(blocker_id)
                .or_default()
                .insert(issue.identifier.as_str());
            has_active_blocker.insert(issue.identifier.as_str());
        }
    }

    // Unblocked roots: issues with no active blocker.
    let roots: Vec<&str> = universe
        .iter()
        .copied()
        .filter(|id| !has_active_blocker.contains(id))
        .collect();

    // BFS from each root to count transitively reachable issues.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for root in roots {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(root);
        queue.push_back(root);
        while let Some(node) = queue.pop_front() {
            if let Some(neighbors) = forward.get(node) {
                for &nb in neighbors {
                    if visited.insert(nb) {
                        queue.push_back(nb);
                    }
                }
            }
        }
        let count = visited.len() - 1; // exclude self
        if count > 0 {
            counts.insert(root.to_string(), count);
        }
    }

    counts
}

/// Sort `issues` in-place by dispatch priority:
/// 1. Transitive block count descending (most-blocking first).
/// 2. Priority ascending (None last).
/// 3. `created_at` oldest first (None last).
/// 4. `identifier` lexicographic ascending.
pub fn sort_candidates(
    issues: &mut [Issue],
    block_counts: &std::collections::HashMap<String, usize>,
) {
    issues.sort_by(|a, b| {
        // Transitive block count: higher first (descending).
        let ba = block_counts.get(&a.identifier).copied().unwrap_or(0);
        let bb = block_counts.get(&b.identifier).copied().unwrap_or(0);
        let block_cmp = bb.cmp(&ba);
        if block_cmp != std::cmp::Ordering::Equal {
            return block_cmp;
        }

        // Priority: Some(low) < Some(high) < None.
        let pa = a.priority;
        let pb = b.priority;
        let prio_cmp = match (pa, pb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if prio_cmp != std::cmp::Ordering::Equal {
            return prio_cmp;
        }

        // Created-at: oldest (smallest timestamp) first; None last.
        let ca = a.created_at;
        let cb = b.created_at;
        let date_cmp = match (ca, cb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if date_cmp != std::cmp::Ordering::Equal {
            return date_cmp;
        }

        // Identifier: lexicographic ascending.
        a.identifier.cmp(&b.identifier)
    });
}

/// Compute exponential backoff for retry `attempt` (1-based), capped at
/// `max_ms`.
///
/// Formula: `min(10_000 * 2^(attempt-1), max_ms)`.
pub fn compute_backoff(attempt: u32, max_ms: u64) -> u64 {
    let base = 10_000u64;
    let exp = 2u64.saturating_pow(attempt.saturating_sub(1));
    base.saturating_mul(exp).min(max_ms)
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockerRef, Issue};
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn make_issue(id: &str, identifier: &str, state: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: "Test issue".to_string(),
            description: None,
            priority: None,
            state: state.to_string(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    fn terminal() -> Vec<String> {
        vec!["done".to_string(), "cancelled".to_string()]
    }

    // ---------------------------------------------------------------------- //
    // compute_backoff
    // ---------------------------------------------------------------------- //

    #[test]
    fn backoff_attempt_1_is_base() {
        assert_eq!(compute_backoff(1, 300_000), 10_000);
    }

    #[test]
    fn backoff_attempt_2_doubles() {
        assert_eq!(compute_backoff(2, 300_000), 20_000);
    }

    #[test]
    fn backoff_attempt_3() {
        assert_eq!(compute_backoff(3, 300_000), 40_000);
    }

    #[test]
    fn backoff_capped_at_max() {
        assert_eq!(compute_backoff(100, 300_000), 300_000);
    }

    #[test]
    fn backoff_respects_small_max() {
        assert_eq!(compute_backoff(1, 5_000), 5_000);
    }

    #[test]
    fn backoff_attempt_0_treated_as_1() {
        assert_eq!(compute_backoff(0, 300_000), 10_000);
    }

    // ---------------------------------------------------------------------- //
    // sort_candidates
    // ---------------------------------------------------------------------- //

    #[test]
    fn sort_by_priority_ascending_none_last() {
        let mut issues = vec![
            {
                let mut i = make_issue("3", "ENG-3", "todo");
                i.priority = None;
                i
            },
            {
                let mut i = make_issue("1", "ENG-1", "todo");
                i.priority = Some(3);
                i
            },
            {
                let mut i = make_issue("2", "ENG-2", "todo");
                i.priority = Some(1);
                i
            },
        ];
        sort_candidates(&mut issues, &HashMap::new());
        assert_eq!(issues[0].identifier, "ENG-2");
        assert_eq!(issues[1].identifier, "ENG-1");
        assert_eq!(issues[2].identifier, "ENG-3");
    }

    #[test]
    fn sort_by_created_at_oldest_first_none_last() {
        let t1 = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();

        let mut issues = vec![
            {
                let mut i = make_issue("3", "ENG-3", "todo");
                i.created_at = None;
                i
            },
            {
                let mut i = make_issue("2", "ENG-2", "todo");
                i.created_at = Some(t2);
                i
            },
            {
                let mut i = make_issue("1", "ENG-1", "todo");
                i.created_at = Some(t1);
                i
            },
        ];
        sort_candidates(&mut issues, &HashMap::new());
        assert_eq!(issues[0].identifier, "ENG-1");
        assert_eq!(issues[1].identifier, "ENG-2");
        assert_eq!(issues[2].identifier, "ENG-3");
    }

    #[test]
    fn sort_tiebreaker_by_identifier() {
        let t = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let mut issues = vec![
            {
                let mut i = make_issue("b", "ENG-B", "todo");
                i.priority = Some(1);
                i.created_at = Some(t);
                i
            },
            {
                let mut i = make_issue("a", "ENG-A", "todo");
                i.priority = Some(1);
                i.created_at = Some(t);
                i
            },
        ];
        sort_candidates(&mut issues, &HashMap::new());
        assert_eq!(issues[0].identifier, "ENG-A");
        assert_eq!(issues[1].identifier, "ENG-B");
    }

    #[test]
    fn sort_empty_list() {
        let mut issues: Vec<Issue> = vec![];
        sort_candidates(&mut issues, &HashMap::new());
        assert!(issues.is_empty());
    }

    #[test]
    fn sort_single_element() {
        let mut issues = vec![make_issue("1", "ENG-1", "todo")];
        sort_candidates(&mut issues, &HashMap::new());
        assert_eq!(issues.len(), 1);
    }

    // ---------------------------------------------------------------------- //
    // compute_transitive_block_counts
    // ---------------------------------------------------------------------- //

    #[test]
    fn block_counts_empty_when_no_blockers() {
        let issues = vec![
            make_issue("1", "ENG-1", "todo"),
            make_issue("2", "ENG-2", "todo"),
        ];
        let counts = compute_transitive_block_counts(&issues, &terminal());
        assert!(counts.is_empty());
    }

    #[test]
    fn block_counts_simple_chain() {
        let a = make_issue("a", "ENG-A", "todo");
        let mut b = make_issue("b", "ENG-B", "todo");
        b.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-A".to_string()),
            state: Some("todo".to_string()),
        }];
        let mut c = make_issue("c", "ENG-C", "todo");
        c.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-B".to_string()),
            state: Some("todo".to_string()),
        }];

        let counts = compute_transitive_block_counts(&[a, b, c], &terminal());
        assert_eq!(counts.get("ENG-A").copied().unwrap_or(0), 2);
        assert_eq!(counts.get("ENG-B").copied().unwrap_or(0), 0);
        assert_eq!(counts.get("ENG-C").copied().unwrap_or(0), 0);
    }

    #[test]
    fn block_counts_diamond() {
        let a = make_issue("a", "ENG-A", "todo");
        let mut b = make_issue("b", "ENG-B", "todo");
        b.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-A".to_string()),
            state: Some("todo".to_string()),
        }];
        let mut c = make_issue("c", "ENG-C", "todo");
        c.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-A".to_string()),
            state: Some("todo".to_string()),
        }];
        let mut d = make_issue("d", "ENG-D", "todo");
        d.blocked_by = vec![
            BlockerRef {
                id: None,
                identifier: Some("ENG-B".to_string()),
                state: Some("todo".to_string()),
            },
            BlockerRef {
                id: None,
                identifier: Some("ENG-C".to_string()),
                state: Some("todo".to_string()),
            },
        ];

        let counts = compute_transitive_block_counts(&[a, b, c, d], &terminal());
        assert_eq!(counts.get("ENG-A").copied().unwrap_or(0), 3);
        assert_eq!(counts.get("ENG-B").copied().unwrap_or(0), 0);
        assert_eq!(counts.get("ENG-C").copied().unwrap_or(0), 0);
        assert_eq!(counts.get("ENG-D").copied().unwrap_or(0), 0);
    }

    #[test]
    fn block_counts_terminal_blocker_excluded() {
        let a = make_issue("a", "ENG-A", "todo");
        let mut b = make_issue("b", "ENG-B", "todo");
        b.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-A".to_string()),
            state: Some("Done".to_string()),
        }];

        let counts = compute_transitive_block_counts(&[a, b], &terminal());
        assert!(counts.is_empty());
    }

    #[test]
    fn block_counts_external_blocker_ignored() {
        let mut b = make_issue("b", "ENG-B", "todo");
        b.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-X".to_string()),
            state: Some("todo".to_string()),
        }];

        let counts = compute_transitive_block_counts(&[b], &terminal());
        assert!(counts.is_empty());
    }

    #[test]
    fn block_counts_disjoint_subgraphs() {
        let a = make_issue("a", "ENG-A", "todo");
        let mut b = make_issue("b", "ENG-B", "todo");
        b.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-A".to_string()),
            state: Some("todo".to_string()),
        }];
        let mut c = make_issue("c", "ENG-C", "todo");
        c.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-B".to_string()),
            state: Some("todo".to_string()),
        }];
        let d = make_issue("d", "ENG-D", "todo");
        let mut e = make_issue("e", "ENG-E", "todo");
        e.blocked_by = vec![BlockerRef {
            id: None,
            identifier: Some("ENG-D".to_string()),
            state: Some("todo".to_string()),
        }];

        let counts = compute_transitive_block_counts(&[a, b, c, d, e], &terminal());
        assert_eq!(counts.get("ENG-A").copied().unwrap_or(0), 2);
        assert_eq!(counts.get("ENG-B").copied().unwrap_or(0), 0);
        assert_eq!(counts.get("ENG-D").copied().unwrap_or(0), 1);
    }

    #[test]
    fn sort_block_count_is_primary_key() {
        let mut a = make_issue("a", "ENG-A", "todo");
        a.priority = Some(4);
        let mut b = make_issue("b", "ENG-B", "todo");
        b.priority = Some(1);

        let mut counts = HashMap::new();
        counts.insert("ENG-A".to_string(), 3usize);

        let mut issues = vec![b, a];
        sort_candidates(&mut issues, &counts);
        assert_eq!(issues[0].identifier, "ENG-A");
        assert_eq!(issues[1].identifier, "ENG-B");
    }

    #[test]
    fn sort_block_count_tie_falls_through_to_priority() {
        let mut a = make_issue("a", "ENG-A", "todo");
        a.priority = Some(3);
        let mut b = make_issue("b", "ENG-B", "todo");
        b.priority = Some(1);

        let mut issues = vec![a, b];
        sort_candidates(&mut issues, &HashMap::new());
        assert_eq!(issues[0].identifier, "ENG-B");
        assert_eq!(issues[1].identifier, "ENG-A");
    }
}
