//! Job chain management — DAG validation, dependency resolution, and execution.
//!
//! Provides cycle detection, topological sorting for execution order,
//! and cascading cancellation when dependency jobs fail.

use std::collections::{HashMap, HashSet, VecDeque};

use eyre::{bail, Result};

use super::types::{ChainStep, JobChain, JobStatus};
pub use crate::hub_core::jobs::{ChainSummary, StepSummary};

/// Validate that a set of chain steps form a valid DAG.
///
/// Checks:
/// - All dependency references point to valid step IDs
/// - No circular dependencies (cycle detection via DFS)
/// - At least one root step (no dependencies)
///
/// Returns the dependency edges as `(dependency, dependent)` tuples.
pub fn validate_chain_dag(steps: &[ChainStep]) -> Result<Vec<(String, String)>> {
    if steps.is_empty() {
        bail!("Chain must contain at least one step");
    }

    let step_ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();

    // Check for duplicate step IDs.
    if step_ids.len() != steps.len() {
        bail!("Chain contains duplicate step IDs");
    }

    // Build edges and validate all references.
    let mut edges = Vec::new();
    for step in steps {
        for dep in &step.depends_on {
            if !step_ids.contains(dep.as_str()) {
                bail!(
                    "Step '{}' depends on '{}', which is not defined in the chain",
                    step.id,
                    dep
                );
            }
            if dep == &step.id {
                bail!("Step '{}' cannot depend on itself", step.id);
            }
            edges.push((dep.clone(), step.id.clone()));
        }
    }

    // Cycle detection using DFS (check before root-step validation so
    // cycles that happen to have no roots get the more specific error).
    detect_cycles(steps, &edges)?;

    // Check for at least one root step.
    let has_root = steps.iter().any(|s| s.depends_on.is_empty());
    if !has_root {
        bail!("Chain must have at least one root step (a step with no dependencies)");
    }

    Ok(edges)
}

/// Detect cycles in the DAG using DFS with coloring.
///
/// Colors: White (unvisited), Grey (in current path), Black (fully explored).
fn detect_cycles(steps: &[ChainStep], edges: &[(String, String)]) -> Result<()> {
    // Build adjacency list: step -> list of dependents.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in steps {
        adj.entry(step.id.as_str()).or_default();
    }
    for (from, to) in edges {
        adj.entry(from.as_str()).or_default().push(to.as_str());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    let mut colors: HashMap<&str, Color> = steps
        .iter()
        .map(|s| (s.id.as_str(), Color::White))
        .collect();

    fn dfs<'a>(
        node: &'a str,
        adj: &HashMap<&'a str, Vec<&'a str>>,
        colors: &mut HashMap<&'a str, Color>,
        path: &mut Vec<&'a str>,
    ) -> Result<()> {
        colors.insert(node, Color::Grey);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for &neighbor in neighbors {
                match colors.get(neighbor) {
                    Some(Color::Grey) => {
                        // Found a cycle — build the cycle path for error reporting.
                        let cycle_start = path.iter().position(|&n| n == neighbor).unwrap();
                        let cycle: Vec<&str> = path[cycle_start..].to_vec();
                        bail!(
                            "Circular dependency detected: {} → {}",
                            cycle.join(" → "),
                            neighbor
                        );
                    }
                    Some(Color::White) | None => {
                        dfs(neighbor, adj, colors, path)?;
                    }
                    Some(Color::Black) => {
                        // Already fully explored, skip.
                    }
                }
            }
        }

        path.pop();
        colors.insert(node, Color::Black);
        Ok(())
    }

    let mut path = Vec::new();
    for step in steps {
        if colors.get(step.id.as_str()) == Some(&Color::White) {
            dfs(step.id.as_str(), &adj, &mut colors, &mut path)?;
        }
    }

    Ok(())
}

/// Perform topological sort on the chain steps.
///
/// Returns step IDs in execution order — steps with no dependencies first.
/// Steps at the same "level" (same depth in the DAG) can run in parallel.
pub fn topological_sort(steps: &[ChainStep]) -> Result<Vec<Vec<String>>> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

    for step in steps {
        in_degree.entry(step.id.as_str()).or_insert(0);
        adj.entry(step.id.as_str()).or_default();
    }

    for step in steps {
        for dep in &step.depends_on {
            adj.entry(dep.as_str()).or_default().push(step.id.as_str());
            *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
        }
    }

    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut processed = 0;

    while !queue.is_empty() {
        let mut level = Vec::new();
        let level_size = queue.len();

        for _ in 0..level_size {
            let node = queue.pop_front().unwrap();
            level.push(node.to_string());
            processed += 1;

            if let Some(neighbors) = adj.get(node) {
                for &neighbor in neighbors {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(neighbor);
                        }
                    }
                }
            }
        }

        levels.push(level);
    }

    if processed != steps.len() {
        bail!("Topological sort failed — this should not happen if cycle detection passed");
    }

    Ok(levels)
}

/// Get all downstream steps that transitively depend on the given step.
///
/// Uses BFS to walk the dependency graph forward from the failed step.
pub fn get_all_downstream(chain: &JobChain, step_id: &str) -> Vec<String> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    // Seed with direct dependents.
    for downstream in chain.downstream_steps(step_id) {
        queue.push_back(downstream.to_string());
    }

    while let Some(current) = queue.pop_front() {
        if visited.contains(&current) {
            continue;
        }
        visited.insert(current.clone());

        for downstream in chain.downstream_steps(&current) {
            if !visited.contains(downstream) {
                queue.push_back(downstream.to_string());
            }
        }
    }

    visited.into_iter().collect()
}

/// Determine which steps in a chain are ready to execute.
///
/// A step is ready when:
/// - Its status is `Submitted` (not yet started)
/// - All its upstream dependencies have `Completed` status
pub fn get_ready_steps(chain: &JobChain) -> Vec<String> {
    let status_map: HashMap<&str, &JobStatus> = chain
        .step_jobs
        .iter()
        .map(|s| (s.step_id.as_str(), &s.status))
        .collect();

    chain
        .step_jobs
        .iter()
        .filter(|s| s.status == JobStatus::Submitted)
        .filter(|s| {
            let upstream = chain.upstream_steps(&s.step_id);
            upstream.iter().all(|dep| {
                status_map
                    .get(dep)
                    .map(|st| **st == JobStatus::Completed)
                    .unwrap_or(false)
            })
        })
        .map(|s| s.step_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_runner::types::{JobConfig, ShellJobConfig, StepJobMapping};

    fn make_step(id: &str, deps: &[&str]) -> ChainStep {
        ChainStep {
            id: id.to_string(),
            config: JobConfig::Shell(ShellJobConfig {
                command: format!("echo {}", id),
                workdir: None,
            }),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            label: None,
            retry: None,
            timeout_secs: None,
            stream: false,
        }
    }

    #[test]
    fn test_valid_linear_chain() {
        let steps = vec![
            make_step("build", &[]),
            make_step("test", &["build"]),
            make_step("deploy", &["test"]),
        ];
        let edges = validate_chain_dag(&steps).unwrap();
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn test_valid_diamond_dag() {
        let steps = vec![
            make_step("build", &[]),
            make_step("test", &["build"]),
            make_step("lint", &["build"]),
            make_step("merge", &["test", "lint"]),
        ];
        let edges = validate_chain_dag(&steps).unwrap();
        assert_eq!(edges.len(), 4);
    }

    #[test]
    fn test_cycle_detection() {
        let steps = vec![
            make_step("a", &["c"]),
            make_step("b", &["a"]),
            make_step("c", &["b"]),
        ];
        let result = validate_chain_dag(&steps);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Circular dependency"));
    }

    #[test]
    fn test_self_dependency() {
        let steps = vec![make_step("a", &["a"])];
        let result = validate_chain_dag(&steps);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot depend on itself"));
    }

    #[test]
    fn test_missing_dependency() {
        let steps = vec![make_step("build", &[]), make_step("test", &["nonexistent"])];
        let result = validate_chain_dag(&steps);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not defined"));
    }

    #[test]
    fn test_no_root_steps() {
        let steps = vec![make_step("a", &["b"]), make_step("b", &["a"])];
        // This should fail on cycle detection or no-root check.
        assert!(validate_chain_dag(&steps).is_err());
    }

    #[test]
    fn test_topological_sort_levels() {
        let steps = vec![
            make_step("build", &[]),
            make_step("test", &["build"]),
            make_step("lint", &["build"]),
            make_step("merge", &["test", "lint"]),
        ];
        let levels = topological_sort(&steps).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["build"]);
        assert_eq!(levels[2], vec!["merge"]);
        // Level 1 should contain test and lint (order may vary).
        let mut level1 = levels[1].clone();
        level1.sort();
        assert_eq!(level1, vec!["lint", "test"]);
    }

    #[test]
    fn test_get_ready_steps() {
        let chain = JobChain {
            id: "ch-test".to_string(),
            agent_id: "test".to_string(),
            step_jobs: vec![
                StepJobMapping {
                    step_id: "build".to_string(),
                    job_id: "j-1".to_string(),
                    status: JobStatus::Completed,
                },
                StepJobMapping {
                    step_id: "test".to_string(),
                    job_id: "j-2".to_string(),
                    status: JobStatus::Submitted,
                },
                StepJobMapping {
                    step_id: "lint".to_string(),
                    job_id: "j-3".to_string(),
                    status: JobStatus::Submitted,
                },
                StepJobMapping {
                    step_id: "merge".to_string(),
                    job_id: "j-4".to_string(),
                    status: JobStatus::Submitted,
                },
            ],
            edges: vec![
                ("build".to_string(), "test".to_string()),
                ("build".to_string(), "lint".to_string()),
                ("test".to_string(), "merge".to_string()),
                ("lint".to_string(), "merge".to_string()),
            ],
            status: super::super::types::ChainStatus::Running,
            submitted_at: chrono::Utc::now(),
            completed_at: None,
            session_channel: None,
            notify_channel: None,
            notify_thread: None,
        };

        let ready = get_ready_steps(&chain);
        let mut ready_sorted = ready.clone();
        ready_sorted.sort();
        assert_eq!(ready_sorted, vec!["lint", "test"]);
    }

    #[test]
    fn test_get_all_downstream() {
        let chain = JobChain {
            id: "ch-test".to_string(),
            agent_id: "test".to_string(),
            step_jobs: vec![
                StepJobMapping {
                    step_id: "build".to_string(),
                    job_id: "j-1".to_string(),
                    status: JobStatus::Running,
                },
                StepJobMapping {
                    step_id: "test".to_string(),
                    job_id: "j-2".to_string(),
                    status: JobStatus::Submitted,
                },
                StepJobMapping {
                    step_id: "lint".to_string(),
                    job_id: "j-3".to_string(),
                    status: JobStatus::Submitted,
                },
                StepJobMapping {
                    step_id: "merge".to_string(),
                    job_id: "j-4".to_string(),
                    status: JobStatus::Submitted,
                },
            ],
            edges: vec![
                ("build".to_string(), "test".to_string()),
                ("build".to_string(), "lint".to_string()),
                ("test".to_string(), "merge".to_string()),
                ("lint".to_string(), "merge".to_string()),
            ],
            status: super::super::types::ChainStatus::Running,
            submitted_at: chrono::Utc::now(),
            completed_at: None,
            session_channel: None,
            notify_channel: None,
            notify_thread: None,
        };

        let mut downstream = get_all_downstream(&chain, "build");
        downstream.sort();
        assert_eq!(downstream, vec!["lint", "merge", "test"]);
    }

    #[test]
    fn test_duplicate_step_ids() {
        let steps = vec![make_step("build", &[]), make_step("build", &[])];
        assert!(validate_chain_dag(&steps).is_err());
    }

    #[test]
    fn test_empty_chain() {
        let steps: Vec<ChainStep> = vec![];
        assert!(validate_chain_dag(&steps).is_err());
    }
}
