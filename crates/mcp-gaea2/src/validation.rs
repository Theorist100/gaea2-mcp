//! Workflow validation for Gaea2 projects.
//!
//! Provides validation and auto-fixing of node types, connections, and properties.

use std::collections::{HashMap, HashSet};

use crate::schema::{
    find_property, find_similar_node_type, get_default_ports, has_known_ports, is_generator_node,
    is_output_node, is_valid_node_type, GAEA_VERSION,
};
use crate::types::{Connection, Node, ValidationResult, Workflow};

/// Validator for Gaea2 workflows.
pub struct Validator;

impl Validator {
    /// Validate a workflow and optionally fix issues.
    ///
    /// Anything repaired in place is reported through `fixes_applied`; `errors` holds only what
    /// is still wrong afterwards, and `valid` follows `errors`. The previous behaviour - valid
    /// whenever any fix had been applied - reported broken workflows as good as soon as a node
    /// was given a default name.
    pub async fn validate_and_fix(workflow: &Workflow, strict_mode: bool) -> ValidationResult {
        let mut errors = Vec::new();
        let mut fixes_applied = Vec::new();
        let mut warnings = Vec::new();
        let mut fixed_nodes = workflow.nodes.clone();
        let mut fixed_connections = workflow.connections.clone();

        // Validate node types
        for node in fixed_nodes.iter_mut() {
            if !is_valid_node_type(&node.node_type) {
                match find_similar_node_type(&node.node_type) {
                    Some(suggestion) => {
                        fixes_applied.push(format!(
                            "Node {} type '{}' -> '{}' (the former does not exist in Gaea {})",
                            node.id, node.node_type, suggestion, GAEA_VERSION
                        ));
                        node.node_type = suggestion.to_string();
                    },
                    None => errors.push(format!(
                        "Node {} has type '{}', which does not exist in Gaea {}",
                        node.id, node.node_type, GAEA_VERSION
                    )),
                }
            }

            // Property ranges, where the installed build declares them.
            for (name, value) in &node.properties {
                // An enumerated property written as a number makes Gaea fail to load the node,
                // and the build then writes nothing without reporting anything.
                if let Err(problem) =
                    crate::schema::check_property_value(&node.node_type, name, value)
                {
                    errors.push(format!("Node {}: {problem}", node.id));
                    continue;
                }

                let Some(declared) = find_property(&node.node_type, name) else {
                    continue;
                };
                let Some(actual) = value.as_f64() else {
                    continue;
                };
                if let (Some(min), Some(max)) = (declared.min, declared.max) {
                    if actual < min || actual > max {
                        warnings.push(format!(
                            "Node {} property '{}' is {}, outside the declared range {}..{}",
                            node.id, name, actual, min, max
                        ));
                    }
                }
            }

            // Set default name if empty
            if node.name.is_empty() {
                node.name = node.node_type.clone();
                fixes_applied.push(format!("Node {} name set to '{}'", node.id, node.name));
            }

            // Ensure generator nodes have a seed
            if is_generator_node(&node.node_type) && !node.properties.contains_key("Seed") {
                let seed = rand::random::<u32>() % 90000 + 10000;
                node.properties
                    .insert("Seed".to_string(), serde_json::Value::from(seed));
                fixes_applied.push(format!("Node {} assigned Seed {}", node.id, seed));
            }

            // Validate position
            if node.position.x < 0.0 || node.position.y < 0.0 {
                node.position.x = node.position.x.max(0.0);
                node.position.y = node.position.y.max(0.0);
                fixes_applied.push(format!("Node {} position corrected", node.id));
            }
        }

        // A mask-style modifier reads the node's input. On a node nothing feeds, it yields
        // nothing, and the whole branch below goes flat without a single error anywhere.
        let fed: HashSet<i32> = fixed_connections.iter().map(|c| c.to_node).collect();
        for node in &fixed_nodes {
            let Some(modifiers) = &node.modifiers else {
                continue;
            };
            if fed.contains(&node.id) {
                continue;
            }
            for modifier in modifiers {
                if crate::schema::modifier_uses_parent_input(&modifier.modifier_type) {
                    warnings.push(format!(
                        "Node {} ({}) has the '{}' modifier, which works off the node's input, \
                         but nothing is connected to it; it will produce an empty result",
                        node.id, node.node_type, modifier.modifier_type
                    ));
                }
            }
        }

        // A property that belongs to a mode the node is not in is accepted, written and then
        // ignored. Nothing reports it - not the build, not the file, not the result - so four
        // settings of Lake were tuned for an afternoon while only WaterLevel was doing anything.
        for node in &fixed_nodes {
            let properties: serde_json::Map<String, serde_json::Value> =
                node.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for warning in crate::schema::check_property_conditions(&node.node_type, &properties) {
                warnings.push(format!("Node {}: {warning}", node.id));
            }
        }

        // Collect valid node IDs
        let valid_ids: HashSet<i32> = fixed_nodes.iter().map(|n| n.id).collect();
        let type_of: HashMap<i32, String> = fixed_nodes
            .iter()
            .map(|n| (n.id, n.node_type.clone()))
            .collect();
        let has_explicit_ports: HashSet<i32> = fixed_nodes
            .iter()
            .filter(|n| n.ports.is_some())
            .map(|n| n.id)
            .collect();

        // Validate connections
        let mut connections_to_remove = Vec::new();
        for (i, conn) in fixed_connections.iter().enumerate() {
            if !valid_ids.contains(&conn.from_node) {
                errors.push(format!(
                    "Connection references invalid source node {}",
                    conn.from_node
                ));
                connections_to_remove.push(i);
            }
            if !valid_ids.contains(&conn.to_node) {
                errors.push(format!(
                    "Connection references invalid target node {}",
                    conn.to_node
                ));
                if !connections_to_remove.contains(&i) {
                    connections_to_remove.push(i);
                }
            }

            // Check for self-connections
            if conn.from_node == conn.to_node {
                errors.push(format!(
                    "Self-connection detected on node {}",
                    conn.from_node
                ));
                if !connections_to_remove.contains(&i) {
                    connections_to_remove.push(i);
                }
            }

            // Port names, where the node type's layout is known and not overridden by the caller.
            // A connection to a port the node does not have is silently dropped by Gaea, so the
            // graph looks wired while the input stays empty.
            check_port(
                conn.from_node,
                &conn.from_port,
                true,
                &type_of,
                &has_explicit_ports,
                &mut warnings,
            );
            check_port(
                conn.to_node,
                &conn.to_port,
                false,
                &type_of,
                &has_explicit_ports,
                &mut warnings,
            );
        }

        // Remove invalid connections (in reverse order to preserve indices)
        for i in connections_to_remove.into_iter().rev() {
            fixes_applied.push(format!(
                "Removed invalid connection from {} to {}",
                fixed_connections[i].from_node, fixed_connections[i].to_node
            ));
            fixed_connections.remove(i);
        }

        // Check for duplicate connections
        let mut seen_connections = HashSet::new();
        let mut duplicates = Vec::new();
        for (i, conn) in fixed_connections.iter().enumerate() {
            let key = (conn.from_node, conn.to_node, &conn.from_port, &conn.to_port);
            if seen_connections.contains(&key) {
                duplicates.push(i);
            } else {
                seen_connections.insert(key);
            }
        }
        for i in duplicates.into_iter().rev() {
            fixes_applied.push("Removed duplicate connection".to_string());
            fixed_connections.remove(i);
        }

        // Check for cycles (DAG validation)
        if has_cycle(&fixed_nodes, &fixed_connections) {
            errors.push("Workflow contains cycles - Gaea2 requires a DAG".to_string());
        }

        // Strict mode: additional checks
        if strict_mode {
            // Check all nodes are connected
            let connected_nodes = get_connected_nodes(&fixed_connections);
            for node in &fixed_nodes {
                if !connected_nodes.contains(&node.id) && fixed_nodes.len() > 1 {
                    errors.push(format!("Node {} is not connected to the workflow", node.id));
                }
            }

            // A workflow exports either through a node of the Output category or through a
            // SaveDefinition, which any node can carry - that is how most Gaea projects write
            // their maps. Matching on the names "Output" and "Export" alone reported projects
            // with a wired Unity node as having no output at all, and "Output" is not even a
            // node type in this build.
            let has_output = fixed_nodes.iter().any(|n| {
                is_output_node(&n.node_type)
                    || n.save_definition.as_ref().is_some_and(|s| s.enabled)
            });
            if !has_output {
                errors.push(
                    "Workflow produces no output: no node of the Output category and no enabled \
                     SaveDefinition"
                        .to_string(),
                );
            }
        }

        ValidationResult {
            valid: errors.is_empty(),
            fixed: !fixes_applied.is_empty(),
            errors,
            fixes_applied,
            warnings,
            workflow: Workflow {
                nodes: fixed_nodes,
                connections: fixed_connections,
            },
        }
    }
}

/// Warn when a connection names a port the node type does not have.
fn check_port(
    node_id: i32,
    port: &str,
    is_source: bool,
    type_of: &HashMap<i32, String>,
    has_explicit_ports: &HashSet<i32>,
    warnings: &mut Vec<String>,
) {
    if has_explicit_ports.contains(&node_id) {
        return;
    }
    let Some(node_type) = type_of.get(&node_id) else {
        return;
    };
    if !has_known_ports(node_type) {
        return;
    }

    let ports = get_default_ports(node_type);
    if ports.iter().any(|(name, _)| *name == port) {
        return;
    }

    let available: Vec<&str> = ports
        .iter()
        .filter(|(_, kind)| {
            if is_source {
                kind.contains("Out")
            } else {
                kind.contains("In")
            }
        })
        .map(|(name, _)| *name)
        .collect();

    warnings.push(format!(
        "Node {} ({}) has no port '{}'; available {}: {}",
        node_id,
        node_type,
        port,
        if is_source { "outputs" } else { "inputs" },
        available.join(", ")
    ));
}

/// Check if the workflow has cycles (not a valid DAG).
fn has_cycle(nodes: &[Node], connections: &[Connection]) -> bool {
    // Build adjacency list
    let mut adj: HashMap<i32, Vec<i32>> = HashMap::new();
    for node in nodes {
        adj.insert(node.id, Vec::new());
    }
    for conn in connections {
        if let Some(neighbors) = adj.get_mut(&conn.from_node) {
            neighbors.push(conn.to_node);
        }
    }

    // DFS for cycle detection
    let mut visited = HashSet::new();
    let mut rec_stack = HashSet::new();

    fn dfs(
        node: i32,
        adj: &HashMap<i32, Vec<i32>>,
        visited: &mut HashSet<i32>,
        rec_stack: &mut HashSet<i32>,
    ) -> bool {
        visited.insert(node);
        rec_stack.insert(node);

        if let Some(neighbors) = adj.get(&node) {
            for &neighbor in neighbors {
                if !visited.contains(&neighbor) {
                    if dfs(neighbor, adj, visited, rec_stack) {
                        return true;
                    }
                } else if rec_stack.contains(&neighbor) {
                    return true;
                }
            }
        }

        rec_stack.remove(&node);
        false
    }

    for node in nodes {
        if !visited.contains(&node.id) && dfs(node.id, &adj, &mut visited, &mut rec_stack) {
            return true;
        }
    }

    false
}

/// Get all node IDs that appear in connections.
fn get_connected_nodes(connections: &[Connection]) -> HashSet<i32> {
    let mut connected = HashSet::new();
    for conn in connections {
        connected.insert(conn.from_node);
        connected.insert(conn.to_node);
    }
    connected
}

/// Normalize connections from various input formats.
pub fn normalize_connections(connections: Vec<serde_json::Value>) -> Vec<Connection> {
    let mut normalized = Vec::new();

    for conn in connections {
        let parsed = if let Some(obj) = conn.as_object() {
            // Handle various key formats
            let from_node = obj
                .get("from_node")
                .or_else(|| obj.get("from"))
                .or_else(|| obj.get("source"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let to_node = obj
                .get("to_node")
                .or_else(|| obj.get("to"))
                .or_else(|| obj.get("target"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let from_port = obj
                .get("from_port")
                .or_else(|| obj.get("source_port"))
                .and_then(|v| v.as_str())
                .unwrap_or("Out")
                .to_string();

            let to_port = obj
                .get("to_port")
                .or_else(|| obj.get("target_port"))
                .and_then(|v| v.as_str())
                .unwrap_or("In")
                .to_string();

            Connection {
                from_node,
                to_node,
                from_port,
                to_port,
            }
        } else if let Some(arr) = conn.as_array() {
            // Handle array format [from_id, to_id]
            let from_node = arr.first().and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let to_node = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as i32;

            Connection {
                from_node,
                to_node,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }
        } else {
            continue;
        };

        if parsed.from_node != 0 && parsed.to_node != 0 {
            normalized.push(parsed);
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_validate_empty_workflow() {
        let workflow = Workflow {
            nodes: vec![],
            connections: vec![],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(result.valid);
        assert!(!result.fixed);
    }

    #[tokio::test]
    async fn test_validate_simple_workflow() {
        let workflow = Workflow {
            nodes: vec![
                Node {
                    id: 1,
                    node_type: "Mountain".to_string(),
                    name: "Mountain".to_string(),
                    position: Default::default(),
                    properties: Default::default(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
                Node {
                    id: 2,
                    node_type: "Export".to_string(),
                    name: "Export".to_string(),
                    position: Default::default(),
                    properties: Default::default(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
            ],
            connections: vec![Connection {
                from_node: 1,
                to_node: 2,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(result.valid);
    }

    /// Build a bare node for the tests below.
    fn node(id: i32, node_type: &str) -> Node {
        Node {
            id,
            node_type: node_type.to_string(),
            name: node_type.to_string(),
            position: Default::default(),
            properties: Default::default(),
            ports: None,
            modifiers: None,
            save_definition: None,
        }
    }

    #[tokio::test]
    async fn a_cosmetic_fix_does_not_make_a_broken_workflow_valid() {
        // One unnamed node (cosmetic fix) plus a connection to a node that is not there.
        let mut unnamed = node(1, "Mountain");
        unnamed.name = String::new();

        let workflow = Workflow {
            nodes: vec![unnamed],
            connections: vec![Connection {
                from_node: 1,
                to_node: 999,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(result.fixed, "the missing name was filled in");
        assert!(
            !result.valid,
            "a dangling connection is still an error: {:?}",
            result.errors
        );
    }

    #[tokio::test]
    async fn a_save_definition_counts_as_output() {
        let mut unity = node(250, "Unity");
        unity.save_definition = Some(crate::types::SaveDefinition {
            filename: "height".to_string(),
            format: "UshortRaw16".to_string(),
            enabled: true,
            disabled_profiles: vec![],
        });

        let workflow = Workflow {
            nodes: vec![node(1, "Mountain"), unity],
            connections: vec![Connection {
                from_node: 1,
                to_node: 250,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = Validator::validate_and_fix(&workflow, true).await;
        assert!(
            !result.errors.iter().any(|e| e.contains("no output")),
            "a wired Unity node is an output: {:?}",
            result.errors
        );
        assert!(result.valid, "{:?}", result.errors);
    }

    #[tokio::test]
    async fn retired_node_names_are_repaired_not_reported() {
        let workflow = Workflow {
            nodes: vec![node(1, "Mountain"), node(2, "Output")],
            connections: vec![Connection {
                from_node: 1,
                to_node: 2,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert_eq!(result.workflow.nodes[1].node_type, "Export");
        assert!(result.valid, "{:?}", result.errors);
    }

    #[tokio::test]
    async fn warns_about_a_port_the_node_does_not_have() {
        let workflow = Workflow {
            nodes: vec![node(1, "Thermal2"), node(2, "Export")],
            connections: vec![Connection {
                from_node: 1,
                to_node: 2,
                // Talus is what the old schema invented for Thermal2.
                from_port: "Talus".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(
            result.warnings.iter().any(|w| w.contains("Talus")),
            "expected a warning about Talus: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn warns_about_a_property_outside_its_range() {
        let mut satmap = node(3, "SatMap");
        satmap
            .properties
            .insert("Bias".to_string(), serde_json::Value::from(5.0));

        let workflow = Workflow {
            nodes: vec![satmap],
            connections: vec![],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(
            result.warnings.iter().any(|w| w.contains("Bias")),
            "expected a range warning: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn test_detect_cycle() {
        let workflow = Workflow {
            nodes: vec![
                Node {
                    id: 1,
                    node_type: "Mountain".to_string(),
                    name: "".to_string(),
                    position: Default::default(),
                    properties: Default::default(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
                Node {
                    id: 2,
                    node_type: "Blur".to_string(),
                    name: "".to_string(),
                    position: Default::default(),
                    properties: Default::default(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
            ],
            connections: vec![
                Connection {
                    from_node: 1,
                    to_node: 2,
                    from_port: "Out".to_string(),
                    to_port: "In".to_string(),
                },
                Connection {
                    from_node: 2,
                    to_node: 1,
                    from_port: "Out".to_string(),
                    to_port: "In".to_string(),
                },
            ],
        };

        let result = Validator::validate_and_fix(&workflow, false).await;
        assert!(result.errors.iter().any(|e| e.contains("cycles")));
    }
}
