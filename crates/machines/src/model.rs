//! A small explicit-state model checker: breadth-first enumeration of every
//! state reachable from the initial states under a successor function.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Visits every state reachable from `initial`, calling `check` once on each
/// and `successors` to expand it. Returns the number of distinct states.
///
/// Panics (failing the test) when more than `limit` states are reached: the
/// abstraction was meant to be finite, and a larger space means it is not.
pub fn explore<S: Clone + Eq + Hash>(
    initial: impl IntoIterator<Item = S>,
    limit: usize,
    mut successors: impl FnMut(&S) -> Vec<S>,
    mut check: impl FnMut(&S),
) -> usize {
    let mut seen: HashSet<S> = HashSet::new();
    let mut frontier: VecDeque<S> = VecDeque::new();
    for state in initial {
        if seen.insert(state.clone()) { frontier.push_back(state); }
    }
    while let Some(state) = frontier.pop_front() {
        check(&state);
        for next in successors(&state) {
            if seen.insert(next.clone()) {
                assert!(seen.len() <= limit, "state space exceeds {limit} states");
                frontier.push_back(next);
            }
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn explore_visits_each_reachable_state_once_and_stops() {
        let mut visited = Vec::new();
        let n = super::explore([0u8], 100, |s| vec![(s + 1) % 5, (s * 2) % 5], |s| visited.push(*s));
        assert_eq!(n, 5);
        visited.sort();
        assert_eq!(visited, [0, 1, 2, 3, 4]);
    }
}
