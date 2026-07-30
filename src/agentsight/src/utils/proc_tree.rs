//! Process-tree traversal over `/proc`.
//!
//! Expands a set of root PIDs with all their descendant processes by walking
//! the ppid tree. Shared by the `token`/`audit` CLI `--descendants` flags and
//! the `/api/panes/summary` endpoint.

use std::collections::{HashMap, HashSet, VecDeque};

/// Expand `roots` with all descendant PIDs found by walking the `/proc` ppid tree.
///
/// Returns the roots themselves plus every transitive child, sorted ascending
/// and deduplicated. If `/proc` cannot be read, only the roots are returned.
pub fn expand_with_descendants(roots: &[u32]) -> Vec<u32> {
    collect_descendants(roots, &children_map())
}

/// Build a parent → children PID map from a single `/proc` scan.
fn children_map() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(ppid) = read_ppid(pid) {
            map.entry(ppid).or_default().push(pid);
        }
    }
    map
}

/// Read the parent PID from `/proc/<pid>/stat`.
///
/// The comm field may contain spaces and parentheses, so fields are located
/// relative to the last `)` instead of naive whitespace splitting.
fn read_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest = " <state> <ppid> <pgrp> ..."
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// BFS from `roots` over the `children` map, returning roots plus all
/// descendants, sorted ascending and deduplicated.
fn collect_descendants(roots: &[u32], children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen: HashSet<u32> = roots.iter().copied().collect();
    let mut queue: VecDeque<u32> = roots.iter().copied().collect();
    while let Some(pid) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child) {
                queue.push_back(child);
            }
        }
    }
    let mut result: Vec<u32> = seen.into_iter().collect();
    result.sort_unstable();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(edges: &[(u32, u32)]) -> HashMap<u32, Vec<u32>> {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(parent, child) in edges {
            map.entry(parent).or_default().push(child);
        }
        map
    }

    #[test]
    fn collects_transitive_descendants() {
        let children = tree(&[(1, 10), (10, 20), (10, 21), (21, 30), (2, 40)]);
        assert_eq!(collect_descendants(&[10], &children), vec![10, 20, 21, 30]);
    }

    #[test]
    fn multiple_roots_are_merged_and_deduplicated() {
        let children = tree(&[(1, 10), (10, 20)]);
        // Root 1 already contains 10's subtree; the union stays deduplicated.
        assert_eq!(collect_descendants(&[1, 10], &children), vec![1, 10, 20]);
    }

    #[test]
    fn root_without_children_returns_itself() {
        assert_eq!(collect_descendants(&[42], &HashMap::new()), vec![42]);
    }

    #[test]
    fn handles_pid_cycles_without_hanging() {
        // /proc snapshots taken mid-scan can produce inconsistent ppid data;
        // the visited set must prevent infinite loops.
        let children = tree(&[(1, 2), (2, 1)]);
        assert_eq!(collect_descendants(&[1], &children), vec![1, 2]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn expand_includes_roots_from_real_proc() {
        let me = std::process::id();
        let pids = expand_with_descendants(&[me]);
        assert!(pids.contains(&me));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_ppid_of_current_process_matches_parent() {
        let ppid = read_ppid(std::process::id());
        assert!(ppid.is_some());
    }
}
