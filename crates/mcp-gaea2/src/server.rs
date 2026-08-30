//! Gaea2 MCP Server implementation.
//!
//! Provides tools for terrain generation, validation, and execution.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use mcp_core::prelude::*;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::cli::Gaea2CLI;
use crate::config::Gaea2Config;
use crate::generation::{generate_project, parse_connections, parse_nodes};
use crate::schema::suggest_nodes;
use crate::templates::{get_template, list_templates};
use crate::types::{
    AnalysisType, BuildConfig, ExecutionHistoryEntry, ExecutionResult, FileInfo, OptimizationMode,
    Workflow,
};
use crate::validation::Validator;

/// Gaea2 MCP Server.
pub struct Gaea2Server {
    config: Arc<Gaea2Config>,
    cli: Option<Arc<Gaea2CLI>>,
    execution_history: Arc<RwLock<Vec<ExecutionHistoryEntry>>>,
}

impl Gaea2Server {
    /// Create a new Gaea2 server instance.
    pub async fn new(gaea_path: Option<String>, output_dir: String) -> anyhow::Result<Self> {
        let config = Arc::new(Gaea2Config::new(gaea_path.clone(), output_dir));

        let cli = config.gaea_path.clone().map(|p| Arc::new(Gaea2CLI::new(p)));

        Ok(Self {
            config,
            cli,
            execution_history: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Get the output directory.
    pub fn output_dir(&self) -> String {
        self.config.output_dir_str()
    }

    /// Get the Gaea2 executable path.
    pub fn gaea_path(&self) -> Option<String> {
        self.config
            .gaea_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
    }

    /// Get all tools as boxed trait objects.
    pub fn tools(&self) -> Vec<BoxedTool> {
        let refs = ServerRefs {
            config: self.config.clone(),
            cli: self.cli.clone(),
            execution_history: self.execution_history.clone(),
        };

        vec![
            Arc::new(CreateProjectTool { refs: refs.clone() }),
            Arc::new(CreateFromTemplateTool { refs: refs.clone() }),
            Arc::new(ValidateWorkflowTool { refs: refs.clone() }),
            Arc::new(SuggestNodesTool { refs: refs.clone() }),
            Arc::new(OptimizePropertiesTool { refs: refs.clone() }),
            Arc::new(AnalyzeWorkflowTool { refs: refs.clone() }),
            Arc::new(RunProjectTool { refs: refs.clone() }),
            Arc::new(DownloadProjectTool { refs: refs.clone() }),
            Arc::new(ListProjectsTool { refs: refs.clone() }),
            Arc::new(ListTemplatesTool { refs: refs.clone() }),
            Arc::new(AnalyzeExecutionHistoryTool { refs: refs.clone() }),
            Arc::new(RepairProjectTool { refs: refs.clone() }),
            Arc::new(ValidateRuntimeTool { refs: refs.clone() }),
            Arc::new(NodeInfoTool { refs: refs.clone() }),
            Arc::new(AnalyzeBuildTool),
            Arc::new(AnalyzeTerrainTool),
            Arc::new(SetSaveDefinitionTool),
            Arc::new(PatchProjectTool),
            Arc::new(CalibrateNodesTool { refs }),
        ]
    }
}

/// Locate the node map inside a parsed .terrain document.
fn terrain_nodes_mut(project: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    project
        .get_mut("Assets")?
        .get_mut("$values")?
        .get_mut(0)?
        .get_mut("Terrain")?
        .get_mut("Nodes")?
        .as_object_mut()
}

/// Locate the asset object that holds Terrain and BuildDefinition.
fn terrain_asset_mut(project: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    project
        .get_mut("Assets")?
        .get_mut("$values")?
        .get_mut(0)?
        .as_object_mut()
}

/// Whether any node in the project writes a file.
fn has_enabled_save(project: &Value) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                if let Some(save) = map.get("SaveDefinition") {
                    if save
                        .get("IsEnabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }
                map.values().any(walk)
            },
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    walk(project)
}

/// Make sure the project carries the build settings Gaea needs, reporting what was added.
///
/// `BuildDefinition.Type` is the one that decides whether a build happens at all. Without it
/// Gaea.Swarm loads the project, exits with code 0 and writes nothing - no files, no crash log -
/// which reads as an empty graph rather than a missing field. The scenes Gaea ships as examples
/// have no `Type`, so enabling an output on a copy of one and building it produces exactly that
/// silence. Verified by adding the fields one at a time: `Type` alone is enough, `Destination`
/// and `ColorSpace` alone change nothing.
fn ensure_buildable(project: &mut Value) -> Vec<String> {
    let mut fixes = Vec::new();
    let Some(asset) = terrain_asset_mut(project) else {
        return fixes;
    };

    let build = asset
        .entry("BuildDefinition".to_string())
        .or_insert_with(|| json!({}));
    let Some(build) = build.as_object_mut() else {
        return fixes;
    };

    if !build.contains_key("Type") {
        build.insert("Type".to_string(), json!("Standard"));
        fixes.push(
            "BuildDefinition.Type was missing and set to 'Standard'; without it Gaea builds \
             nothing and reports no error"
                .to_string(),
        );
    }
    if !build.contains_key("Destination") {
        build.insert(
            "Destination".to_string(),
            json!("<Builds>\\[Filename]\\[+++]"),
        );
        fixes.push("BuildDefinition.Destination was missing and set to the default".to_string());
    }

    fixes
}

/// The node type of a serialized node, taken from its `$type`.
fn node_type_of(node: &Value) -> Option<&str> {
    node.get("$type")?
        .as_str()?
        .strip_prefix("QuadSpinner.Gaea.Nodes.")?
        .split(',')
        .next()
}

/// Read a .terrain file, optionally back it up, and hand back the parsed document.
async fn open_project(path: &PathBuf, create_backup: bool) -> Result<(Value, Option<String>)> {
    if !path.exists() {
        return Err(MCPError::InvalidParameters(format!(
            "Project file not found: {}",
            path.display()
        )));
    }

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| MCPError::Internal(format!("Failed to read file: {e}")))?;
    let project: Value = serde_json::from_str(&content)
        .map_err(|e| MCPError::Internal(format!("Failed to parse project: {e}")))?;

    let backup_path = if create_backup {
        let backup = path.with_extension("terrain.backup");
        tokio::fs::copy(path, &backup)
            .await
            .map_err(|e| MCPError::Internal(format!("Failed to create backup: {e}")))?;
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    };

    Ok((project, backup_path))
}

/// Write a .terrain document back to disk.
async fn save_project(path: &PathBuf, project: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(project)
        .map_err(|e| MCPError::Internal(format!("Failed to serialize: {e}")))?;
    tokio::fs::write(path, text)
        .await
        .map_err(|e| MCPError::Internal(format!("Failed to write: {e}")))
}

/// Shared references for tools.
#[derive(Clone)]
struct ServerRefs {
    config: Arc<Gaea2Config>,
    cli: Option<Arc<Gaea2CLI>>,
    execution_history: Arc<RwLock<Vec<ExecutionHistoryEntry>>>,
}

// =============================================================================
// Tool: create_gaea2_project
// =============================================================================

struct CreateProjectTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for CreateProjectTool {
    fn name(&self) -> &str {
        "create_gaea2_project"
    }

    fn description(&self) -> &str {
        "Create a custom Gaea2 terrain project from nodes and connections"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_name": {
                    "type": "string",
                    "description": "Name for the terrain project"
                },
                "nodes": {
                    "type": "array",
                    "description": "Array of node definitions with id, type, name, position, and properties",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "integer"},
                            "type": {"type": "string"},
                            "name": {"type": "string"},
                            "position": {
                                "type": "object",
                                "properties": {
                                    "x": {"type": "number"},
                                    "y": {"type": "number"}
                                }
                            },
                            "properties": {"type": "object"}
                        },
                        "required": ["type"]
                    }
                },
                "connections": {
                    "type": "array",
                    "description": "Array of connections between nodes",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from_node": {"type": "integer"},
                            "to_node": {"type": "integer"},
                            "from_port": {"type": "string", "default": "Out"},
                            "to_port": {"type": "string", "default": "In"}
                        },
                        "required": ["from_node", "to_node"]
                    }
                },
                "build_config": {
                    "type": "object",
                    "description": "Optional build configuration",
                    "properties": {
                        "resolution": {"type": "integer", "default": 2048},
                        "color_space": {"type": "string", "default": "sRGB"}
                    }
                }
            },
            "required": ["project_name", "nodes"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let project_name = args
            .get("project_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_name'".to_string()))?;

        let nodes_json = args
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'nodes' array".to_string()))?;

        let nodes = parse_nodes(nodes_json)
            .map_err(|e| MCPError::InvalidParameters(format!("Invalid nodes: {e}")))?;

        let connections = if let Some(conns) = args.get("connections").and_then(|v| v.as_array()) {
            parse_connections(conns)
                .map_err(|e| MCPError::InvalidParameters(format!("Invalid connections: {e}")))?
        } else {
            vec![]
        };

        let build_config = match args.get("build_config") {
            Some(v) => {
                let mut config = BuildConfig::default();
                if let Some(res) = v.get("resolution").and_then(|r| r.as_i64()) {
                    // An off-list resolution builds nothing at all; refuse it here rather than
                    // baking it into the file for the build to fail on later.
                    let res_u32 = u32::try_from(res).unwrap_or(0);
                    if !crate::schema::is_valid_build_resolution(res_u32) {
                        return Err(MCPError::InvalidParameters(format!(
                            "build_config.resolution {} is not one of {:?}",
                            res,
                            crate::schema::BUILD_RESOLUTIONS
                        )));
                    }
                    config.resolution = res as i32;
                    config.bake_resolution = res as i32;
                }
                if let Some(cs) = v.get("color_space").and_then(|c| c.as_str()) {
                    config.color_space = cs.to_string();
                }
                Some(config)
            },
            None => None,
        };

        let workflow = Workflow { nodes, connections };

        // Generate output path
        let output_path = self.refs.config.generate_output_path(project_name);
        let output_path_str = output_path.to_string_lossy().to_string();

        let project = generate_project(
            project_name,
            &workflow,
            build_config,
            Some(&output_path_str),
        )
        .await
        .map_err(|e| MCPError::Internal(format!("Failed to generate project: {e}")))?;

        let result = json!({
            "success": true,
            "project_name": project_name,
            "output_path": output_path_str,
            "node_count": workflow.nodes.len(),
            "connection_count": workflow.connections.len()
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: create_gaea2_from_template
// =============================================================================

struct CreateFromTemplateTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for CreateFromTemplateTool {
    fn name(&self) -> &str {
        "create_gaea2_from_template"
    }

    fn description(&self) -> &str {
        "Create a Gaea2 project from a pre-built template"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "template_name": {
                    "type": "string",
                    "description": "Name of the template to use",
                    "enum": list_templates().iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>()
                },
                "project_name": {
                    "type": "string",
                    "description": "Name for the output project"
                },
                "modifications": {
                    "type": "object",
                    "description": "Optional modifications to apply to the template"
                }
            },
            "required": ["template_name", "project_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let template_name = args
            .get("template_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'template_name'".to_string()))?;

        let project_name = args
            .get("project_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_name'".to_string()))?;

        let template = get_template(template_name).ok_or_else(|| {
            let available: Vec<_> = list_templates()
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            MCPError::InvalidParameters(format!(
                "Unknown template '{template_name}'. Available: {available:?}"
            ))
        })?;

        let workflow = Workflow {
            nodes: template.nodes,
            connections: template.connections,
        };

        let output_path = self.refs.config.generate_output_path(project_name);
        let output_path_str = output_path.to_string_lossy().to_string();

        generate_project(project_name, &workflow, None, Some(&output_path_str))
            .await
            .map_err(|e| MCPError::Internal(format!("Failed to generate project: {e}")))?;

        let result = json!({
            "success": true,
            "template_name": template_name,
            "project_name": project_name,
            "output_path": output_path_str,
            "node_count": workflow.nodes.len(),
            "connection_count": workflow.connections.len()
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: validate_and_fix_workflow
// =============================================================================

struct ValidateWorkflowTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for ValidateWorkflowTool {
    fn name(&self) -> &str {
        "validate_and_fix_workflow"
    }

    fn description(&self) -> &str {
        "Validate a Gaea2 workflow and optionally fix issues automatically"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "description": "Array of node definitions"
                },
                "connections": {
                    "type": "array",
                    "description": "Array of connections"
                },
                "strict_mode": {
                    "type": "boolean",
                    "description": "Enable strict validation (check for unconnected nodes, missing outputs)",
                    "default": false
                },
                "runtime_check": {
                    "type": "boolean",
                    "description": "Also validate by running through Gaea.Swarm.exe CLI (requires Windows with Gaea2)",
                    "default": false
                }
            },
            "required": ["nodes"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let nodes_json = args
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'nodes' array".to_string()))?;

        let nodes = parse_nodes(nodes_json)
            .map_err(|e| MCPError::InvalidParameters(format!("Invalid nodes: {e}")))?;

        let connections = if let Some(conns) = args.get("connections").and_then(|v| v.as_array()) {
            parse_connections(conns)
                .map_err(|e| MCPError::InvalidParameters(format!("Invalid connections: {e}")))?
        } else {
            vec![]
        };

        let strict_mode = args
            .get("strict_mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let runtime_check = args
            .get("runtime_check")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let workflow = Workflow { nodes, connections };
        let mut result = Validator::validate_and_fix(&workflow, strict_mode).await;

        // Optional runtime validation via CLI
        if runtime_check {
            if let Some(cli) = &self.refs.cli {
                // Create a temp file for runtime validation
                let temp_path = self.refs.config.generate_output_path("_validation_temp");
                let temp_path_str = temp_path.to_string_lossy().to_string();

                // Generate the project file
                if let Ok(_) = generate_project(
                    "_validation_temp",
                    &result.workflow,
                    None,
                    Some(&temp_path_str),
                )
                .await
                {
                    // Run CLI validation
                    let cli_result = cli
                        .run_project(
                            &temp_path_str,
                            "512",
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            false,
                            false,
                            30,
                        )
                        .await;

                    // Add runtime validation result
                    result.valid = result.valid && cli_result.success;
                    if !cli_result.success {
                        if let Some(error) = cli_result.error {
                            result
                                .errors
                                .push(format!("Runtime validation failed: {error}"));
                        }
                    }

                    // Clean up temp file
                    let _ = tokio::fs::remove_file(&temp_path).await;
                }
            } else {
                result
                    .errors
                    .push("Runtime check requested but Gaea2 CLI not configured".to_string());
            }
        }

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: suggest_gaea2_nodes
// =============================================================================

struct SuggestNodesTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for SuggestNodesTool {
    fn name(&self) -> &str {
        "suggest_gaea2_nodes"
    }

    fn description(&self) -> &str {
        "Get intelligent node suggestions based on the current workflow and context"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "current_nodes": {
                    "type": "array",
                    "description": "List of current node types in the workflow",
                    "items": {"type": "string"}
                },
                "context": {
                    "type": "string",
                    "description": "Description of the terrain being created (e.g., 'mountain', 'desert canyon')"
                }
            },
            "required": ["current_nodes"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let current_nodes: Vec<String> = args
            .get("current_nodes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let context = args.get("context").and_then(|v| v.as_str());

        let suggestions = suggest_nodes(&current_nodes, context);

        let result = json!({
            "suggestions": suggestions,
            "current_node_count": current_nodes.len()
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: optimize_gaea2_properties
// =============================================================================

struct OptimizePropertiesTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for OptimizePropertiesTool {
    fn name(&self) -> &str {
        "optimize_gaea2_properties"
    }

    fn description(&self) -> &str {
        "Optimize node properties for performance or quality"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "description": "Array of node definitions"
                },
                "mode": {
                    "type": "string",
                    "description": "Optimization mode",
                    "enum": ["performance", "quality", "balanced"],
                    "default": "balanced"
                }
            },
            "required": ["nodes"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let nodes_json = args
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'nodes' array".to_string()))?;

        let mut nodes = parse_nodes(nodes_json)
            .map_err(|e| MCPError::InvalidParameters(format!("Invalid nodes: {e}")))?;

        let mode: OptimizationMode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_value(json!(s)).ok())
            .unwrap_or_default();

        let mut optimizations = Vec::new();

        for node in &mut nodes {
            match node.node_type.as_str() {
                "Erosion2" => {
                    let (duration, strength) = match mode {
                        OptimizationMode::Performance => (0.5, 0.2),
                        OptimizationMode::Quality => (2.0, 0.4),
                        OptimizationMode::Balanced => (1.0, 0.3),
                    };
                    if !node.properties.contains_key("Duration") {
                        node.properties
                            .insert("Duration".to_string(), json!(duration));
                        optimizations.push(format!("{}: Duration set to {}", node.name, duration));
                    }
                    if !node.properties.contains_key("Strength") {
                        node.properties
                            .insert("Strength".to_string(), json!(strength));
                        optimizations.push(format!("{}: Strength set to {}", node.name, strength));
                    }
                },
                "Mountain" => {
                    let scale = match mode {
                        OptimizationMode::Performance => 0.5,
                        OptimizationMode::Quality => 0.8,
                        OptimizationMode::Balanced => 0.65,
                    };
                    if !node.properties.contains_key("Scale") {
                        node.properties.insert("Scale".to_string(), json!(scale));
                        optimizations.push(format!("{}: Scale set to {}", node.name, scale));
                    }
                },
                _ => {},
            }
        }

        let result = json!({
            "success": true,
            "mode": format!("{:?}", mode),
            "optimizations_applied": optimizations,
            "optimized_nodes": nodes
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: analyze_workflow_patterns
// =============================================================================

struct AnalyzeWorkflowTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for AnalyzeWorkflowTool {
    fn name(&self) -> &str {
        "analyze_workflow_patterns"
    }

    fn description(&self) -> &str {
        "Analyze workflow patterns and suggest improvements"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "description": "Array of node definitions"
                },
                "connections": {
                    "type": "array",
                    "description": "Array of connections"
                },
                "analysis_type": {
                    "type": "string",
                    "description": "Type of analysis",
                    "enum": ["patterns", "performance", "quality", "all"],
                    "default": "all"
                },
                "workflow_type": {
                    "type": "string",
                    "description": "Expected terrain type for context-specific suggestions",
                    "enum": ["mountain", "volcanic", "canyon", "coastal", "arctic", "desert", "river", "general"]
                }
            },
            "required": ["nodes"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let nodes_json = args
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'nodes' array".to_string()))?;

        let nodes = parse_nodes(nodes_json)
            .map_err(|e| MCPError::InvalidParameters(format!("Invalid nodes: {e}")))?;

        let connections = if let Some(conns) = args.get("connections").and_then(|v| v.as_array()) {
            parse_connections(conns).unwrap_or_default()
        } else {
            vec![]
        };

        let workflow_type = args
            .get("workflow_type")
            .and_then(|v| v.as_str())
            .unwrap_or("general");

        let mut patterns = Vec::new();
        let mut suggestions = Vec::new();

        // Detect patterns
        let node_types: Vec<&str> = nodes.iter().map(|n| n.node_type.as_str()).collect();

        if node_types.contains(&"Mountain") && node_types.contains(&"Erosion2") {
            patterns.push("Classic terrain pipeline: Mountain -> Erosion");
        }

        if node_types.contains(&"Volcano") {
            patterns.push("Volcanic terrain detected");
        }

        if node_types
            .iter()
            .filter(|t| **t == "Erosion2" || **t == "Erosion")
            .count()
            > 1
        {
            patterns.push("Multi-stage erosion (advanced detail)");
        }

        // Common suggestions
        if !node_types.contains(&"Output") && !node_types.contains(&"Export") {
            suggestions.push("Add an Output or Export node to enable terrain export");
        }

        if node_types.contains(&"Mountain") && !node_types.contains(&"Erosion2") {
            suggestions.push("Consider adding Erosion2 for realistic terrain detail");
        }

        if !node_types
            .iter()
            .any(|t| ["QuickColor", "Satmaps", "SatMap", "Colorize", "CLUTer"].contains(t))
        {
            suggestions.push("Add a colorization node for visual output");
        }

        // Workflow type-specific suggestions
        match workflow_type {
            "mountain" | "alpine" => {
                if !node_types.contains(&"Snow") && !node_types.contains(&"Snowfield") {
                    suggestions.push("Consider adding Snow for alpine terrain");
                }
                if !node_types
                    .iter()
                    .any(|t| *t == "Rocks" || *t == "RockNoise")
                {
                    suggestions.push("Add rock detail with Rocks or RockNoise");
                }
            },
            "volcanic" => {
                if !node_types.contains(&"Thermal") && !node_types.contains(&"Thermal2") {
                    suggestions.push("Add Thermal erosion for volcanic weathering");
                }
                if !node_types.contains(&"Stratify") {
                    suggestions.push("Stratify can add rock layering typical of volcanic terrain");
                }
            },
            "canyon" => {
                if !node_types.contains(&"Stratify") {
                    suggestions.push("Add Stratify for canyon rock layers");
                }
                if !node_types.contains(&"FractalTerraces") && !node_types.contains(&"Terraces") {
                    suggestions.push("Consider FractalTerraces for canyon shelf formations");
                }
            },
            "coastal" => {
                if !node_types.contains(&"Coast") && !node_types.contains(&"Sea") {
                    suggestions.push("Add Coast or Sea for water simulation");
                }
                if !node_types.contains(&"Beach") {
                    suggestions.push("Beach node creates realistic shorelines");
                }
            },
            "arctic" => {
                if !node_types.contains(&"Glacier") && !node_types.contains(&"IceFloe") {
                    suggestions.push("Add Glacier for ice features");
                }
                if !node_types.contains(&"Snow") {
                    suggestions.push("Snow is essential for arctic terrain");
                }
            },
            "desert" => {
                if !node_types.contains(&"Sand") && !node_types.contains(&"DuneSea") {
                    suggestions.push("Add Sand or DuneSea for desert terrain");
                }
                if !node_types.contains(&"Sandstone") {
                    suggestions.push("Sandstone adds characteristic desert erosion");
                }
            },
            "river" => {
                if !node_types.contains(&"Rivers") {
                    suggestions.push("Rivers node is essential for river terrain");
                }
                if !node_types.contains(&"Fluvial") && !node_types.contains(&"Sediment") {
                    suggestions.push("Add Fluvial or Sediment for river deposits");
                }
            },
            _ => {},
        }

        // Complexity score (simple heuristic)
        let complexity = (nodes.len() as f64 * 0.3) + (connections.len() as f64 * 0.2);

        let result = json!({
            "patterns": patterns,
            "suggestions": suggestions,
            "node_count": nodes.len(),
            "connection_count": connections.len(),
            "complexity_score": complexity
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: run_gaea2_project
// =============================================================================

struct RunProjectTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for RunProjectTool {
    fn name(&self) -> &str {
        "run_gaea2_project"
    }

    fn description(&self) -> &str {
        "Run a Gaea2 project to generate terrain outputs (requires Gaea2 CLI)"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Path to the .terrain file"
                },
                "resolution": {
                    "type": "string",
                    "description": "Build resolution",
                    "enum": ["512", "1024", "2048", "4096", "8192"],
                    "default": "1024"
                },
                "build_path": {
                    "type": "string",
                    "description": "Output directory (optional)"
                },
                "profile": {
                    "type": "string",
                    "description": "Build profile name"
                },
                "seed": {
                    "type": "integer",
                    "description": "Mutation seed for variations"
                },
                "region": {
                    "type": "string",
                    "description": "Region to build (for tiled builds)"
                },
                "target_node": {
                    "type": "string",
                    "description": "Specific node to build (by ID or name)"
                },
                "variables": {
                    "type": "object",
                    "description": "Variable overrides as key-value pairs",
                    "additionalProperties": true
                },
                "ignore_cache": {
                    "type": "boolean",
                    "description": "Force rebuild ignoring cache",
                    "default": false
                },
                "verbose": {
                    "type": "boolean",
                    "description": "Enable verbose output",
                    "default": false
                },
                "timeout": {
                    "type": "integer",
                    "description": "Maximum execution time in seconds",
                    "default": 300
                }
            },
            "required": ["project_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let cli = self.refs.cli.as_ref().ok_or_else(|| {
            MCPError::Internal("Gaea2 CLI not configured - set GAEA2_PATH".to_string())
        })?;

        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;

        // Accept a number as readily as a string; callers write both.
        let resolution = match args.get("resolution") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => "1024".to_string(),
        };
        let resolution = resolution.as_str();

        // Gaea does not reject an off-list resolution: it starts the build, works through the
        // first node and exits without writing anything, which looks like a broken graph.
        match resolution.parse::<u32>() {
            Ok(value) if crate::schema::is_valid_build_resolution(value) => {},
            _ => {
                return Err(MCPError::InvalidParameters(format!(
                    "Resolution '{}' is not one of {:?}. Gaea would start the build and exit \
                     without writing any file. Sizes like 2049 belong to the Unity node's \
                     TargetSize, not to the build resolution.",
                    resolution,
                    crate::schema::BUILD_RESOLUTIONS
                )));
            },
        }

        let build_path = args.get("build_path").and_then(|v| v.as_str());
        let profile = args.get("profile").and_then(|v| v.as_str());
        let seed = args.get("seed").and_then(|v| v.as_i64());
        let region = args.get("region").and_then(|v| v.as_str());
        let target_node = args.get("target_node").and_then(|v| v.as_str());

        // A build that cannot write anything still exits 0 and leaves no crash log, so say so
        // here rather than reporting success over an empty directory.
        if let Ok(text) = tokio::fs::read_to_string(project_path).await {
            if let Ok(project) = serde_json::from_str::<Value>(&text) {
                let has_type = project
                    .get("Assets")
                    .and_then(|a| a.get("$values"))
                    .and_then(|v| v.get(0))
                    .and_then(|a| a.get("BuildDefinition"))
                    .and_then(|b| b.get("Type"))
                    .is_some();

                if !has_type {
                    return Err(MCPError::InvalidParameters(
                        "This project has no BuildDefinition.Type, and Gaea will exit cleanly \
                         without writing a single file or a crash log. Scenes shipped as Gaea \
                         examples are missing it. Run repair_gaea2_project on the file first."
                            .to_string(),
                    ));
                }

                if target_node.is_none() && !has_enabled_save(&project) {
                    return Err(MCPError::InvalidParameters(
                        "No node in this project has an enabled SaveDefinition, so the build \
                         would produce no files. Use set_gaea2_save_definition to pick an \
                         output, or pass target_node to build one node."
                            .to_string(),
                    ));
                }
            }
        }
        let variables = args
            .get("variables")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .collect::<std::collections::HashMap<_, _>>()
            });
        let ignore_cache = args
            .get("ignore_cache")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let verbose = args
            .get("verbose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

        let result = cli
            .run_project(
                project_path,
                resolution,
                build_path,
                profile,
                region,
                seed,
                target_node,
                variables,
                ignore_cache,
                verbose,
                timeout,
            )
            .await;

        // Record in execution history
        let entry = ExecutionHistoryEntry {
            timestamp: Utc::now().to_rfc3339(),
            project: project_path.to_string(),
            result: result.clone(),
        };
        self.refs.execution_history.write().await.push(entry);

        let mut reported = serde_json::to_value(&result).unwrap_or_else(|_| json!({}));

        // A build that wrote nothing says nothing about why. Nodes marked [RequiresBaking] are
        // one known way to get an empty result out of a graph that looks complete, so name the
        // ones present rather than leaving the caller to guess.
        if !result.success && result.file_count == 0 {
            if let Ok(text) = tokio::fs::read_to_string(project_path).await {
                let baking: Vec<&str> = crate::gaea_schema_generated::NODES_REQUIRING_BAKING
                    .iter()
                    .copied()
                    .filter(|t| text.contains(&format!("QuadSpinner.Gaea.Nodes.{t}, Gaea.Nodes")))
                    .collect();
                if !baking.is_empty() {
                    reported["nodes_requiring_baking"] = json!(baking);
                    reported["baking_hint"] = json!(
                        "These nodes are marked [RequiresBaking] in the installed build. Such a \
                         node can finish without producing data, which shows up as a build that \
                         writes no files and reports no error."
                    );
                }
            }
        }

        ToolResult::json(&reported)
    }
}

// =============================================================================
// Tool: download_gaea2_project
// =============================================================================

struct DownloadProjectTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for DownloadProjectTool {
    fn name(&self) -> &str {
        "download_gaea2_project"
    }

    fn description(&self) -> &str {
        "Download a Gaea2 project file with optional encoding"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Path to the .terrain file to download"
                },
                "encoding": {
                    "type": "string",
                    "description": "Encoding format for the content",
                    "enum": ["base64", "raw"],
                    "default": "base64"
                }
            },
            "required": ["project_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;

        let encoding = args
            .get("encoding")
            .and_then(|v| v.as_str())
            .unwrap_or("base64");

        let path = PathBuf::from(project_path);
        if !path.exists() {
            return Err(MCPError::InvalidParameters(format!(
                "Project file not found: {project_path}"
            )));
        }

        let content = tokio::fs::read(&path)
            .await
            .map_err(|e| MCPError::Internal(format!("Failed to read file: {e}")))?;

        let result = if encoding == "raw" {
            // Return raw string content (for JSON/text files)
            let raw_content = String::from_utf8_lossy(&content).to_string();
            json!({
                "success": true,
                "filename": path.file_name().map(|n| n.to_string_lossy().to_string()),
                "size": content.len(),
                "encoding": "raw",
                "content": raw_content
            })
        } else {
            // Return base64 encoded content
            let encoded =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &content);
            json!({
                "success": true,
                "filename": path.file_name().map(|n| n.to_string_lossy().to_string()),
                "size": content.len(),
                "encoding": "base64",
                "content_base64": encoded
            })
        };

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: list_gaea2_projects
// =============================================================================

struct ListProjectsTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for ListProjectsTool {
    fn name(&self) -> &str {
        "list_gaea2_projects"
    }

    fn description(&self) -> &str {
        "List all Gaea2 project files in the output directory"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "directory": {
                    "type": "string",
                    "description": "Directory to list (defaults to output directory)"
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let directory = args
            .get("directory")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.refs.config.output_dir.clone());

        let mut files = Vec::new();

        if let Ok(mut entries) = tokio::fs::read_dir(&directory).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().map(|e| e == "terrain").unwrap_or(false) {
                    if let Ok(metadata) = entry.metadata().await {
                        let modified = metadata
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| {
                                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();

                        files.push(FileInfo {
                            filename: path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                            path: path.to_string_lossy().to_string(),
                            size: metadata.len(),
                            modified,
                        });
                    }
                }
            }
        }

        files.sort_by(|a, b| b.modified.cmp(&a.modified));

        let result = json!({
            "directory": directory.to_string_lossy().to_string(),
            "files": files,
            "count": files.len()
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: list_templates
// =============================================================================

struct ListTemplatesTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for ListTemplatesTool {
    fn name(&self) -> &str {
        "list_gaea2_templates"
    }

    fn description(&self) -> &str {
        "List all available Gaea2 project templates"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        let templates: Vec<_> = list_templates()
            .into_iter()
            .map(|(name, description)| {
                let template = get_template(&name);
                json!({
                    "name": name,
                    "description": description,
                    "node_count": template.as_ref().map(|t| t.nodes.len()).unwrap_or(0),
                    "connection_count": template.as_ref().map(|t| t.connections.len()).unwrap_or(0)
                })
            })
            .collect();

        let result = json!({
            "templates": templates,
            "count": templates.len()
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: analyze_execution_history
// =============================================================================

struct AnalyzeExecutionHistoryTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for AnalyzeExecutionHistoryTool {
    fn name(&self) -> &str {
        "analyze_execution_history"
    }

    fn description(&self) -> &str {
        "Analyze execution history for debugging and monitoring"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return",
                    "default": 10
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let history = self.refs.execution_history.read().await;
        let recent: Vec<_> = history.iter().rev().take(limit).cloned().collect();

        let success_count = recent.iter().filter(|e| e.result.success).count();
        let failure_count = recent.len() - success_count;

        let avg_time = recent
            .iter()
            .filter_map(|e| e.result.execution_time)
            .sum::<f64>()
            / recent.len().max(1) as f64;

        let result = json!({
            "recent_executions": recent,
            "total_count": history.len(),
            "success_count": success_count,
            "failure_count": failure_count,
            "average_execution_time": avg_time
        });

        ToolResult::json(&result)
    }
}

// =============================================================================
// Tool: repair_gaea2_project
// =============================================================================

struct RepairProjectTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for RepairProjectTool {
    fn name(&self) -> &str {
        "repair_gaea2_project"
    }

    fn description(&self) -> &str {
        "Repair a Gaea2 project file against the installed build: retired node types, port \
         layouts and intrinsic modifiers. Reports property values outside their range. Leaves \
         the file untouched when there is nothing to fix."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Path to the .terrain file to repair"
                },
                "create_backup": {
                    "type": "boolean",
                    "description": "Create a backup before repairing",
                    "default": true
                }
            },
            "required": ["project_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;

        let create_backup = args
            .get("create_backup")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let path = PathBuf::from(project_path);
        let (mut project, backup_path) = open_project(&path, create_backup).await?;

        let mut next_id = highest_ref_id(&project) + 1;
        let mut repairs = Vec::new();
        let mut warnings = Vec::new();

        {
            let nodes = terrain_nodes_mut(&mut project).ok_or_else(|| {
                MCPError::Internal("Project has no Assets/Terrain/Nodes section".to_string())
            })?;

            let ids: Vec<String> = nodes.keys().cloned().collect();
            for id in ids {
                if id.starts_with('$') {
                    continue;
                }
                let Some(node) = nodes.get_mut(&id) else {
                    continue;
                };
                let Some(mut node_type) = node_type_of(node).map(str::to_string) else {
                    continue;
                };

                // A retired name serializes to a $type Gaea cannot resolve, and the whole file
                // is then rejected as corrupt.
                if !crate::schema::is_valid_node_type(&node_type) {
                    match crate::schema::find_similar_node_type(&node_type) {
                        Some(replacement) => {
                            if let Some(obj) = node.as_object_mut() {
                                obj.insert(
                                    "$type".to_string(),
                                    json!(format!(
                                        "QuadSpinner.Gaea.Nodes.{replacement}, Gaea.Nodes"
                                    )),
                                );
                            }
                            repairs
                                .push(format!("Node {id}: type '{node_type}' -> '{replacement}'"));
                            node_type = replacement.to_string();
                        },
                        None => {
                            warnings.push(format!(
                                "Node {id} has type '{node_type}', which does not exist in Gaea \
                                 {} and has no known replacement",
                                crate::schema::GAEA_VERSION
                            ));
                            continue;
                        },
                    }
                }

                repair_ports(
                    node,
                    &id,
                    &node_type,
                    &mut next_id,
                    &mut repairs,
                    &mut warnings,
                );
                repair_intrinsic_modifiers(node, &id, &node_type, &mut next_id, &mut repairs);
                report_property_ranges(node, &id, &node_type, &mut warnings);
                report_modifier_problems(node, &id, &node_type, &mut warnings);
            }
        }

        repairs.extend(ensure_buildable(&mut project));

        if !has_enabled_save(&project) {
            warnings.push(
                "No node in this project has an enabled SaveDefinition, so a build would write \
                 nothing. Use set_gaea2_save_definition to pick an output."
                    .to_string(),
            );
        }

        // Rewriting an unchanged file only moves its checksum, which then reads as a repair
        // that never happened.
        if repairs.is_empty() {
            return ToolResult::json(&json!({
                "success": true,
                "project_path": project_path,
                "backup_path": backup_path,
                "repairs_applied": repairs,
                "warnings": warnings,
                "changed": false,
                "message": "Nothing to repair; the file was left byte for byte as it was"
            }));
        }

        save_project(&path, &project).await?;

        ToolResult::json(&json!({
            "success": true,
            "project_path": project_path,
            "backup_path": backup_path,
            "repairs_applied": repairs,
            "warnings": warnings,
            "changed": true
        }))
    }
}

/// Highest numeric `$id` in the document, so new objects can be given unused ones.
fn highest_ref_id(value: &Value) -> u64 {
    match value {
        Value::Object(map) => {
            let own = map
                .get("$id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            map.values()
                .map(highest_ref_id)
                .chain([own])
                .max()
                .unwrap_or(0)
        },
        Value::Array(items) => items.iter().map(highest_ref_id).max().unwrap_or(0),
        _ => 0,
    }
}

/// Bring a node's ports in line with the installed build, keeping existing connections.
fn repair_ports(
    node: &mut Value,
    id: &str,
    node_type: &str,
    next_id: &mut u64,
    repairs: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if !crate::schema::has_known_ports(node_type) {
        return;
    }
    let expected = crate::schema::get_default_ports(node_type);
    let node_ref = node
        .get("$id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();

    let Some(existing) = node
        .get("Ports")
        .and_then(|p| p.get("$values"))
        .and_then(|v| v.as_array())
        .cloned()
    else {
        return;
    };

    let current: Vec<&str> = existing
        .iter()
        .filter_map(|p| p.get("Name").and_then(|n| n.as_str()))
        .collect();
    let wanted: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
    if current == wanted {
        return;
    }

    let mut rebuilt = Vec::new();
    for (name, kind) in &expected {
        match existing
            .iter()
            .find(|p| p.get("Name").and_then(|n| n.as_str()) == Some(*name))
        {
            // Keep the port as it stands: its Record carries the connection.
            Some(port) => rebuilt.push(port.clone()),
            None => {
                rebuilt.push(json!({
                    "$id": next_id.to_string(),
                    "Name": name,
                    "Type": kind,
                    "IsExporting": true,
                    "Parent": {"$ref": node_ref}
                }));
                *next_id += 1;
            },
        }
    }

    // A port the installed build does not have, but which something is wired to, is kept:
    // dropping it would silently cut the connection.
    let mut kept_unknown = Vec::new();
    for port in &existing {
        let Some(name) = port.get("Name").and_then(|n| n.as_str()) else {
            continue;
        };
        if wanted.contains(&name) {
            continue;
        }
        if port.get("Record").is_some() {
            rebuilt.push(port.clone());
            kept_unknown.push(name.to_string());
        }
    }

    let ports_id = node
        .get("Ports")
        .and_then(|p| p.get("$id"))
        .cloned()
        .unwrap_or_else(|| {
            let id = json!(next_id.to_string());
            *next_id += 1;
            id
        });

    if let Some(obj) = node.as_object_mut() {
        obj.insert(
            "Ports".to_string(),
            json!({"$id": ports_id, "$values": rebuilt}),
        );
    }

    repairs.push(format!(
        "Node {id} ({node_type}): ports {current:?} -> {wanted:?}"
    ));
    for name in kept_unknown {
        warnings.push(format!(
            "Node {id} ({node_type}): kept port '{name}', which this node type does not have, \
             because a connection uses it"
        ));
    }
}

/// Add the modifiers Gaea attaches to a node itself, when they are missing.
fn repair_intrinsic_modifiers(
    node: &mut Value,
    id: &str,
    node_type: &str,
    next_id: &mut u64,
    repairs: &mut Vec<String>,
) {
    let intrinsic = crate::schema::get_intrinsic_modifiers(node_type);
    if intrinsic.is_empty() {
        return;
    }

    let node_ref = node
        .get("$id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();

    let mut values = node
        .get("Modifiers")
        .and_then(|m| m.get("$values"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut added = Vec::new();
    for (modifier_type, order) in intrinsic {
        let present = values
            .iter()
            .any(|m| m.get("Name").and_then(|n| n.as_str()) == Some(*modifier_type));
        if present {
            continue;
        }
        values.push(json!({
            "$id": next_id.to_string(),
            "$type": format!("QuadSpinner.Gaea.Nodes.Modifiers.{modifier_type}, Gaea.Nodes"),
            "Name": modifier_type,
            "Parent": {"$ref": node_ref},
            "Intrinsic": true,
            "Order": order
        }));
        *next_id += 1;
        added.push(*modifier_type);
    }

    if added.is_empty() {
        return;
    }

    let modifiers_id = node
        .get("Modifiers")
        .and_then(|m| m.get("$id"))
        .cloned()
        .unwrap_or_else(|| {
            let id = json!(next_id.to_string());
            *next_id += 1;
            id
        });

    if let Some(obj) = node.as_object_mut() {
        obj.insert(
            "Modifiers".to_string(),
            json!({"$id": modifiers_id, "$values": values}),
        );
    }
    repairs.push(format!(
        "Node {id} ({node_type}): added intrinsic modifier(s) {added:?}"
    ));
}

/// Report modifiers that will not do what the file implies.
///
/// Two failures look identical from outside - a flat result and no error at all: a modifier
/// setting that does not exist on that modifier, and a mask-style modifier on a node with
/// nothing connected to it.
fn report_modifier_problems(node: &Value, id: &str, node_type: &str, warnings: &mut Vec<String>) {
    let Some(modifiers) = node
        .get("Modifiers")
        .and_then(|m| m.get("$values"))
        .and_then(|v| v.as_array())
    else {
        return;
    };

    let has_input = node
        .get("Ports")
        .and_then(|p| p.get("$values"))
        .and_then(|v| v.as_array())
        .is_some_and(|ports| {
            ports.iter().any(|port| {
                port.get("Record").is_some()
                    && port
                        .get("Type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind.contains("In"))
            })
        });

    for modifier in modifiers {
        let Some(name) = modifier.get("Name").and_then(Value::as_str) else {
            continue;
        };

        if !crate::schema::is_valid_modifier_type(name) {
            warnings.push(format!(
                "Node {id} ({node_type}): modifier '{name}' does not exist in Gaea {}",
                crate::schema::GAEA_VERSION
            ));
            continue;
        }

        if crate::schema::modifier_uses_parent_input(name) && !has_input {
            warnings.push(format!(
                "Node {id} ({node_type}): the '{name}' modifier works off the node's input, but \
                 nothing is connected to it - it will produce an empty result and the build will \
                 report nothing"
            ));
        }

        let Some(fields) = modifier.as_object() else {
            continue;
        };
        for (key, value) in fields {
            if key.starts_with('$')
                || matches!(
                    key.as_str(),
                    "Name" | "Parent" | "Intrinsic" | "HasUI" | "Order"
                )
            {
                continue;
            }
            if let Err(problem) = crate::schema::check_modifier_property(name, key, value) {
                warnings.push(format!("Node {id} ({node_type}): {problem}"));
            }
        }
    }
}

/// Note property values that fall outside the range the build declares.
fn report_property_ranges(node: &Value, id: &str, node_type: &str, warnings: &mut Vec<String>) {
    let Some(obj) = node.as_object() else {
        return;
    };
    for (name, value) in obj {
        if name.starts_with('$') {
            continue;
        }

        // An enumerated property stored as a number stops the build dead, with no diagnostics
        // of any kind, so it is worth naming even though repair cannot guess the right member.
        if let Err(problem) = crate::schema::check_property_value(node_type, name, value) {
            warnings.push(format!("Node {id}: {problem}"));
            continue;
        }

        let Some(declared) = crate::schema::find_property(node_type, name) else {
            continue;
        };
        let (Some(actual), Some(min), Some(max)) = (value.as_f64(), declared.min, declared.max)
        else {
            continue;
        };
        if actual < min || actual > max {
            warnings.push(format!(
                "Node {id} ({node_type}): '{name}' is {actual}, outside {min}..{max}"
            ));
        }
    }
}

// =============================================================================
// Tool: validate_gaea2_runtime
// =============================================================================

struct ValidateRuntimeTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for ValidateRuntimeTool {
    fn name(&self) -> &str {
        "validate_gaea2_runtime"
    }

    fn description(&self) -> &str {
        "Validate a Gaea2 project file by running it through Gaea.Swarm.exe CLI (requires Windows with Gaea2)"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Path to the .terrain file to validate"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Maximum time to wait for validation in seconds",
                    "default": 30
                }
            },
            "required": ["project_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let cli = self.refs.cli.as_ref().ok_or_else(|| {
            MCPError::Internal("Gaea2 CLI not configured - set GAEA2_PATH".to_string())
        })?;

        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;

        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);

        let path = PathBuf::from(project_path);
        if !path.exists() {
            return Err(MCPError::InvalidParameters(format!(
                "Project file not found: {project_path}"
            )));
        }

        // Validate by running a minimal 512 resolution build
        // If the file is corrupt, it will fail
        let result = cli
            .run_project(
                project_path,
                "512", // Minimal resolution for fast validation
                None,
                None,
                None,
                None,
                None,
                None,
                false,
                false,
                timeout,
            )
            .await;

        let validation_result = json!({
            "success": result.success,
            "project_path": project_path,
            "validation_type": "runtime",
            "execution_time": result.execution_time,
            "error": result.error,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "output_files": result.output_files
        });

        ToolResult::json(&validation_result)
    }
}

// =============================================================================
// Tool: gaea2_node_info
// =============================================================================

/// Answers "what is this node and what can I set on it" straight from the installed build,
/// so callers stop inferring ports and property names from example files.
struct NodeInfoTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for NodeInfoTool {
    fn name(&self) -> &str {
        "gaea2_node_info"
    }

    fn description(&self) -> &str {
        "Describe node and modifier types of the installed Gaea build: category, ports in \
         serialization order, intrinsic modifiers, and properties with their declared ranges, \
         accepted enum members and interface labels. Give node_type for one node, modifier for \
         one modifier, label to resolve an interface name such as 'Height Remap', or \
         search/category to list."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "node_type": {
                    "type": "string",
                    "description": "Exact node type, e.g. 'Erosion2'"
                },
                "modifier": {
                    "type": "string",
                    "description": "Exact modifier type, e.g. 'Multiplier'. Pass '*' to list them all."
                },
                "label": {
                    "type": "string",
                    "description": "Interface label of a modifier setting, e.g. 'Height Remap', to find which modifier it belongs to"
                },
                "search": {
                    "type": "string",
                    "description": "Case-insensitive substring to search node names for"
                },
                "category": {
                    "type": "string",
                    "description": "Restrict to one category: Primitive, Terrain, Modify, Surface, Simulate, Derive, Colorize, Output, Utility"
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        use crate::schema::{
            find_similar_node_type, get_default_ports, get_intrinsic_modifiers, get_node_category,
            get_node_properties, has_known_ports, is_generator_node, is_output_node,
            is_valid_node_type, GAEA_VERSION, VALID_NODE_TYPES,
        };

        // Measurements, when this machine has taken any. They belong beside the schema rather
        // than inside it: what a node produces is a fact about a build on a machine, not a
        // declaration in an assembly.
        let measured = crate::calibration::load(&crate::calibration::default_path(
            &self.refs.config.output_dir,
        ));

        /// Full description of one node type.
        fn describe(node_type: &str, measured: Option<&crate::calibration::Calibration>) -> Value {
            let ports: Vec<Value> = get_default_ports(node_type)
                .iter()
                .map(|(name, kind)| json!({"name": name, "type": kind}))
                .collect();

            let properties: Vec<Value> = get_node_properties(node_type)
                .iter()
                .map(|p| {
                    let allowed = crate::schema::enum_values(p.cs_type);
                    let mut described = json!({
                        "name": p.name,
                        "type": p.cs_type,
                        "default": p.default_value,
                        "min": p.min,
                        "max": p.max
                    });
                    // Enumerated and boolean properties carry their default as a name.
                    if let Some(text) = p.default_text {
                        described["default"] = json!(text);
                    }
                    if let Some(curve) = p.curve {
                        described["curve"] = json!(curve);
                        described["curve_note"] = json!(
                            "Non-linear: a value halfway along the range is not half the effect."
                        );
                    }
                    // Enumerated properties must be written by member name, so list them.
                    if !allowed.is_empty() {
                        described["values"] = json!(allowed);
                    }
                    // Half the properties of some nodes only apply in one of their modes, and
                    // outside it the value is accepted and ignored.
                    if let Some((switch, allowed)) =
                        crate::schema::property_condition(node_type, p.name)
                    {
                        described["applies_when"] = json!({
                            "property": switch,
                            "is": allowed,
                        });
                    }
                    // What the shipped scenes chose, which the declared range cannot tell you.
                    if let Some(u) = crate::schema::find_property_usage(node_type, p.name) {
                        let mut seen = json!({"times_set": u.times_set});
                        if let (Some(low), Some(median), Some(high)) = (u.low, u.median, u.high) {
                            seen["low"] = json!(low);
                            seen["median"] = json!(median);
                            seen["high"] = json!(high);
                        }
                        if let Some(common) = u.most_common {
                            seen["most_common"] = json!(common);
                        }
                        described["in_shipped_scenes"] = seen;
                    }
                    described
                })
                .collect();

            let modifiers: Vec<Value> = get_intrinsic_modifiers(node_type)
                .iter()
                .map(|(name, order)| json!({"type": name, "order": order}))
                .collect();

            // Where this node tends to sit in a graph, taken from the scenes Gaea ships.
            let follows: Vec<Value> = crate::schema::common_predecessors(node_type)
                .into_iter()
                .take(5)
                .map(|(from, from_port, to_port, count)| {
                    json!({"node": from, "from_port": from_port, "into_port": to_port, "times": count})
                })
                .collect();
            let feeds: Vec<Value> = crate::schema::common_successors(node_type)
                .into_iter()
                .take(5)
                .map(|(from_port, to, to_port, count)| {
                    json!({"node": to, "from_port": from_port, "into_port": to_port, "times": count})
                })
                .collect();

            let mut described = json!({
                "type": node_type,
                "category": get_node_category(node_type),
                "family": crate::schema::node_family(node_type),
                "short_code": crate::schema::node_short_code(node_type),
                "is_generator": is_generator_node(node_type),
                "is_output": is_output_node(node_type),
                "ports": ports,
                "ports_verified": has_known_ports(node_type),
                "intrinsic_modifiers": modifiers,
                "properties": properties,
                "used_in_shipped_scenes": crate::schema::usage_count(node_type),
                "usually_fed_by": follows,
                "usually_feeds": feeds
            });

            let keywords = crate::schema::node_keywords(node_type);
            if !keywords.is_empty() {
                described["keywords"] = json!(keywords);
            }
            if crate::schema::requires_baking(node_type) {
                described["requires_baking"] = json!(true);
                described["baking_note"] = json!(
                    "Marked [RequiresBaking]: built on its own this node can finish without \
                     writing a file and without an error."
                );
            }

            // What the node was measured to produce, which is the only place a caller learns
            // that a parameter and its effect are not the same thing.
            if let Some(calibration) = measured {
                let taken = calibration.for_node(node_type);
                if !taken.is_empty() {
                    let rows: Vec<Value> = taken
                        .iter()
                        .map(|m| {
                            let mut row = json!({
                                "fills_range": (m.amplitude * 10_000.0).round() / 100.0,
                                "mean_level": (m.mean_level * 10_000.0).round() / 100.0,
                                "seed_spread": [
                                    (m.amplitude_low * 10_000.0).round() / 100.0,
                                    (m.amplitude_high * 10_000.0).round() / 100.0
                                ]
                            });
                            match (&m.property, m.value) {
                                (Some(property), Some(value)) => {
                                    row["at"] = json!(format!("{property} = {value}"));
                                },
                                _ => row["at"] = json!("defaults"),
                            }
                            row
                        })
                        .collect();
                    described["measured"] = json!({
                        "gaea_version": calibration.gaea_version,
                        "resolution": calibration.resolution,
                        "note": "Share of the full vertical range the output occupies, as built \
                                 on this machine. A parameter is not an outcome: some nodes \
                                 scale their height with their footprint.",
                        "points": rows
                    });
                }
            }
            described
        }

        /// Describe one modifier: what it does with the node it sits on, and what it accepts.
        fn describe_modifier(modifier_type: &str) -> Value {
            let properties: Vec<Value> = crate::schema::get_modifier_properties(modifier_type)
                .iter()
                .map(|p| {
                    let allowed = crate::schema::enum_values(p.cs_type);
                    let mut described = json!({
                        "name": p.name,
                        "type": p.cs_type,
                        "default": p.default_value,
                        "min": p.min,
                        "max": p.max
                    });
                    if let Some(label) = p.label {
                        described["label"] = json!(label);
                    }
                    if !allowed.is_empty() {
                        described["values"] = json!(allowed);
                    }
                    described
                })
                .collect();

            let reads_input = crate::schema::modifier_uses_parent_input(modifier_type);
            json!({
                "type": modifier_type,
                "reads_node_input": reads_input,
                "role": if reads_input {
                    "Works off the node's input - a mask or a combiner. On a node with nothing \
                     connected it produces an empty result, silently."
                } else {
                    "Transforms the node's own output; usable on a generator."
                },
                "properties": properties
            })
        }

        // Resolve an interface label, e.g. "Height Remap" -> Multiplier.Value.
        if let Some(label) = args.get("label").and_then(|v| v.as_str()) {
            return match crate::schema::modifier_for_label(label) {
                Some((modifier_type, property)) => {
                    let mut described = describe_modifier(modifier_type);
                    described["matched_label"] = json!(label);
                    described["matched_property"] = json!(property.name);
                    described["found"] = json!(true);
                    ToolResult::json(&described)
                },
                None => ToolResult::json(&json!({
                    "found": false,
                    "label": label,
                    "message": format!("No modifier setting is labelled '{label}' in Gaea {}",
                                       crate::schema::GAEA_VERSION)
                })),
            };
        }

        if let Some(modifier) = args.get("modifier").and_then(|v| v.as_str()) {
            if modifier == "*" {
                let all: Vec<Value> = crate::schema::modifier_types()
                    .iter()
                    .map(|m| describe_modifier(m))
                    .collect();
                return ToolResult::json(&json!({
                    "gaea_version": crate::schema::GAEA_VERSION,
                    "count": all.len(),
                    "modifiers": all
                }));
            }

            if !crate::schema::is_valid_modifier_type(modifier) {
                return ToolResult::json(&json!({
                    "found": false,
                    "modifier": modifier,
                    "available": crate::schema::modifier_types()
                }));
            }

            let mut described = describe_modifier(modifier);
            described["found"] = json!(true);
            described["gaea_version"] = json!(crate::schema::GAEA_VERSION);
            return ToolResult::json(&described);
        }

        if let Some(node_type) = args.get("node_type").and_then(|v| v.as_str()) {
            if !is_valid_node_type(node_type) {
                let mut result = json!({
                    "found": false,
                    "type": node_type,
                    "gaea_version": GAEA_VERSION,
                    "message": format!("No node type '{}' in Gaea {}", node_type, GAEA_VERSION)
                });
                if let Some(similar) = find_similar_node_type(node_type) {
                    result["suggestion"] = json!(similar);
                }
                return ToolResult::json(&result);
            }

            let mut described = describe(node_type, measured.as_ref());
            described["found"] = json!(true);
            described["gaea_version"] = json!(GAEA_VERSION);
            return ToolResult::json(&described);
        }

        // Listing mode.
        let search = args
            .get("search")
            .and_then(|v| v.as_str())
            .map(str::to_lowercase);
        let category = args.get("category").and_then(|v| v.as_str());

        let mut matches: Vec<&str> = VALID_NODE_TYPES
            .iter()
            .copied()
            .filter(|t| {
                // Searching names alone misses what Gaea's own tool box finds: Dusting answers
                // to "snow", Glacier to "ice", MountainSide to "slopenoise".
                let by_search = search.as_ref().is_none_or(|s| {
                    t.to_lowercase().contains(s.as_str())
                        || crate::schema::node_keywords(t)
                            .iter()
                            .any(|k| k.to_lowercase().contains(s.as_str()))
                });
                let by_category = category.is_none_or(|c| {
                    get_node_category(t).is_some_and(|actual| actual.eq_ignore_ascii_case(c))
                });
                by_search && by_category
            })
            .collect();
        // Most-used first: a node in forty scenes is the one a caller most likely wants.
        matches.sort_unstable_by(|a, b| {
            crate::schema::usage_count(b)
                .cmp(&crate::schema::usage_count(a))
                .then(a.cmp(b))
        });

        let listed: Vec<Value> = matches
            .iter()
            .map(|t| {
                let mut entry = json!({
                    "type": t,
                    "category": get_node_category(t),
                    "family": crate::schema::node_family(t),
                    "is_generator": is_generator_node(t),
                    "is_output": is_output_node(t),
                    "used_in_shipped_scenes": crate::schema::usage_count(t)
                });
                let keywords = crate::schema::node_keywords(t);
                if !keywords.is_empty() {
                    entry["keywords"] = json!(keywords);
                }
                entry
            })
            .collect();

        ToolResult::json(&json!({
            "gaea_version": GAEA_VERSION,
            "count": listed.len(),
            "nodes": listed
        }))
    }
}

// =============================================================================
// Tool: analyze_gaea2_build
// =============================================================================

/// Reads a build directory the way a person would: what came out, how big it is, and - when
/// something faulted - which node failed first, because everything downstream then reports that
/// its input returned no data.
struct AnalyzeBuildTool;

#[async_trait]
impl Tool for AnalyzeBuildTool {
    fn name(&self) -> &str {
        "analyze_gaea2_build"
    }

    fn description(&self) -> &str {
        "Inspect a Gaea build directory: output files with sizes, and the first faulting node \
         from CRASH_LOG.txt if the build failed."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "build_dir": {
                    "type": "string",
                    "description": "Directory Gaea wrote its build into"
                },
                "expected_files": {
                    "type": "array",
                    "description": "Optional file names that must be present; each is reported as found or missing",
                    "items": {"type": "string"}
                }
            },
            "required": ["build_dir"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let build_dir = args
            .get("build_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'build_dir'".to_string()))?;

        let dir = PathBuf::from(build_dir);
        if !dir.exists() {
            return Err(MCPError::InvalidParameters(format!(
                "Build directory not found: {build_dir}"
            )));
        }

        let mut files = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| MCPError::Internal(format!("Failed to read directory: {e}")))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            total_bytes += metadata.len();
            files.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "size": metadata.len()
            }));
        }
        files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

        let crash = crate::cli::read_crash_log(&dir).await;
        let first_fault = crash.as_ref().and_then(|(_, fault)| fault.clone());
        let crash_log = crash.as_ref().map(|(text, _)| text.clone());

        let expected: Vec<Value> = args
            .get("expected_files")
            .and_then(|v| v.as_array())
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str())
                    .map(|name| {
                        let present = files
                            .iter()
                            .any(|f| f["name"].as_str().is_some_and(|f| f == name));
                        json!({"name": name, "present": present})
                    })
                    .collect()
            })
            .unwrap_or_default();

        // The heightfield is the point of the build, so describe it here instead of making the
        // caller find the file and ask again.
        let terrain = files
            .iter()
            .filter_map(|f| f["name"].as_str())
            .find(|name| name.to_lowercase().ends_with(".raw"))
            .and_then(|name| {
                let full = dir.join(name);
                crate::terrain::load(&full).ok().map(|field| {
                    let report = field.report(4800.0, 300.0, 5.0, 0.6);
                    json!({
                        "file": name,
                        "resolution": report.resolution,
                        "relief_metres": report.relief_metres,
                        "is_flat": report.is_flat,
                        "centre_gentle_percent": report.centre_gentle_percent,
                        "edge_lift_metres": report.edge_lift_metres,
                        "height_classes": report.height_classes,
                    })
                })
            });

        ToolResult::json(&json!({
            "build_dir": build_dir,
            "file_count": files.len(),
            "total_bytes": total_bytes,
            "files": files,
            "crashed": first_fault.is_some(),
            "first_fault": first_fault,
            "crash_log": crash_log,
            "expected_files": expected,
            "terrain": terrain
        }))
    }
}

// =============================================================================
// Tool: analyze_gaea2_terrain
// =============================================================================

/// Answers questions about the land a build produced, not about its files.
struct AnalyzeTerrainTool;

#[async_trait]
impl Tool for AnalyzeTerrainTool {
    fn name(&self) -> &str {
        "analyze_gaea2_terrain"
    }

    fn description(&self) -> &str {
        "Read a built heightfield (.raw or .png) and describe the terrain: relief in metres, how \
         much of the playable middle is gentle enough to build on, whether the edges stand above \
         that middle, the split into lowland / plain / commanding height, and profiles across \
         both axes. Says outright when the field is flat."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Heightfield written by a build: .raw (16-bit) or .png"
                },
                "metres_across": {
                    "type": "number",
                    "description": "Width of the map in metres",
                    "default": 4800
                },
                "metres_up": {
                    "type": "number",
                    "description": "Metres between the lowest and highest possible point",
                    "default": 300
                },
                "gentle_degrees": {
                    "type": "number",
                    "description": "Slope up to which ground counts as buildable",
                    "default": 5
                },
                "centre_fraction": {
                    "type": "number",
                    "description": "Share of the map treated as the playable middle; the rest is the frame",
                    "default": 0.6
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'path'".to_string()))?;

        let number =
            |key: &str, fallback: f64| args.get(key).and_then(Value::as_f64).unwrap_or(fallback);

        let field = crate::terrain::load(std::path::Path::new(path))
            .map_err(MCPError::InvalidParameters)?;

        let report = field.report(
            number("metres_across", 4800.0),
            number("metres_up", 300.0),
            number("gentle_degrees", 5.0),
            number("centre_fraction", 0.6),
        );

        let mut value = serde_json::to_value(&report)
            .map_err(|e| MCPError::Internal(format!("Cannot serialise the report: {e}")))?;
        value["path"] = json!(path);

        // A one-line read of the shape, so the numbers do not have to be interpreted every time.
        let shape = if report.is_flat {
            "flat: the graph produced no relief at all"
        } else if report.edge_lift_metres > 20.0 {
            "a basin: edges stand above the middle, which is what a playable map wants"
        } else if report.edge_lift_metres < -20.0 {
            "a mound: the middle stands above the edges, so the playable area is the slope"
        } else {
            "even: no strong difference between the middle and the frame"
        };
        value["shape"] = json!(shape);

        ToolResult::json(&value)
    }
}

// =============================================================================
// Tool: set_gaea2_save_definition
// =============================================================================

/// Turning a node's output on or off is the single most common edit to an existing project, and
/// it decides what a build actually writes.
struct SetSaveDefinitionTool;

#[async_trait]
impl Tool for SetSaveDefinitionTool {
    fn name(&self) -> &str {
        "set_gaea2_save_definition"
    }

    fn description(&self) -> &str {
        "Enable, disable or retarget the file a node of an existing .terrain project writes."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {"type": "string", "description": "Path to the .terrain file"},
                "node_id": {"type": "integer", "description": "Id of the node to change"},
                "enabled": {"type": "boolean", "description": "Whether the node writes a file"},
                "filename": {"type": "string", "description": "Output file name, without extension"},
                "format": {
                    "type": "string",
                    "description": "Output format, e.g. PNG8, PNG16, UshortRaw16, FloatRaw, EXR"
                },
                "create_backup": {"type": "boolean", "default": true}
            },
            "required": ["project_path", "node_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;
        let node_id = args
            .get("node_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'node_id'".to_string()))?;
        let create_backup = args
            .get("create_backup")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let path = PathBuf::from(project_path);
        let (mut project, backup_path) = open_project(&path, create_backup).await?;

        let nodes = terrain_nodes_mut(&mut project).ok_or_else(|| {
            MCPError::Internal("Project has no Assets/Terrain/Nodes section".to_string())
        })?;
        let node = nodes.get_mut(&node_id.to_string()).ok_or_else(|| {
            MCPError::InvalidParameters(format!("No node {node_id} in this project"))
        })?;

        let node_type = node_type_of(node).unwrap_or("unknown").to_string();

        // Reuse the existing save definition where there is one, so untouched fields survive.
        let existing = node.get("SaveDefinition").cloned();
        let mut save = existing.clone().unwrap_or_else(|| {
            json!({
                "Node": node_id,
                "Filename": "",
                "Format": "PNG16",
                "IsEnabled": true,
                "DisabledInProfiles": {"$values": []}
            })
        });

        if let Some(filename) = args.get("filename").and_then(|v| v.as_str()) {
            save["Filename"] = json!(filename);
        }
        if let Some(format) = args.get("format").and_then(|v| v.as_str()) {
            save["Format"] = json!(format);
        }
        if let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) {
            save["IsEnabled"] = json!(enabled);
        }
        save["Node"] = json!(node_id);

        if save["Filename"].as_str().is_none_or(str::is_empty) {
            return Err(MCPError::InvalidParameters(
                "This node has no output file name yet; pass 'filename'".to_string(),
            ));
        }

        node.as_object_mut()
            .ok_or_else(|| MCPError::Internal(format!("Node {node_id} is not an object")))?
            .insert("SaveDefinition".to_string(), save.clone());

        // An output is only half of what a build needs; see ensure_buildable.
        let build_fixes = ensure_buildable(&mut project);

        save_project(&path, &project).await?;

        ToolResult::json(&json!({
            "success": true,
            "project_path": project_path,
            "backup_path": backup_path,
            "node_id": node_id,
            "node_type": node_type,
            "created": existing.is_none(),
            "save_definition": save,
            "build_settings_fixed": build_fixes
        }))
    }
}

// =============================================================================
// Tool: patch_gaea2_project
// =============================================================================

/// Editing a couple of properties should not mean regenerating the whole graph, which loses
/// everything the file carries that the caller did not restate.
struct PatchProjectTool;

#[async_trait]
impl Tool for PatchProjectTool {
    fn name(&self) -> &str {
        "patch_gaea2_project"
    }

    fn description(&self) -> &str {
        "Change properties of nodes in an existing .terrain project in place. Property names are \
         checked against the installed build, and values against their declared range."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_path": {"type": "string", "description": "Path to the .terrain file"},
                "patches": {
                    "type": "array",
                    "description": "One entry per node to change",
                    "items": {
                        "type": "object",
                        "properties": {
                            "node_id": {"type": "integer"},
                            "properties": {
                                "type": "object",
                                "description": "Property name to new value"
                            }
                        },
                        "required": ["node_id", "properties"]
                    }
                },
                "allow_unknown_properties": {
                    "type": "boolean",
                    "description": "Write property names the node does not declare (off by default: Gaea ignores them silently)",
                    "default": false
                },
                "create_backup": {"type": "boolean", "default": true}
            },
            "required": ["project_path", "patches"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        use crate::schema::find_property;

        let project_path = args
            .get("project_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'project_path'".to_string()))?;
        let patches = args
            .get("patches")
            .and_then(|v| v.as_array())
            .ok_or_else(|| MCPError::InvalidParameters("Missing 'patches' array".to_string()))?;
        let allow_unknown = args
            .get("allow_unknown_properties")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let create_backup = args
            .get("create_backup")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let path = PathBuf::from(project_path);
        let (mut project, backup_path) = open_project(&path, create_backup).await?;

        let mut applied = Vec::new();
        let mut warnings = Vec::new();
        let mut rejected = Vec::new();

        {
            let nodes = terrain_nodes_mut(&mut project).ok_or_else(|| {
                MCPError::Internal("Project has no Assets/Terrain/Nodes section".to_string())
            })?;

            for patch in patches {
                let Some(node_id) = patch.get("node_id").and_then(|v| v.as_i64()) else {
                    rejected.push("A patch entry has no 'node_id'".to_string());
                    continue;
                };
                let Some(properties) = patch.get("properties").and_then(|v| v.as_object()) else {
                    rejected.push(format!("Patch for node {node_id} has no 'properties'"));
                    continue;
                };
                let Some(node) = nodes.get_mut(&node_id.to_string()) else {
                    rejected.push(format!("No node {node_id} in this project"));
                    continue;
                };

                let node_type = node_type_of(node).unwrap_or("unknown").to_string();
                let Some(node_obj) = node.as_object_mut() else {
                    rejected.push(format!("Node {node_id} is not an object"));
                    continue;
                };

                for (name, value) in properties {
                    // An enumerated property written as a number makes Gaea fail to load the
                    // node, and the build then produces nothing at all.
                    if let Err(problem) =
                        crate::schema::check_property_value(&node_type, name, value)
                    {
                        rejected.push(format!("Node {node_id}: {problem}"));
                        continue;
                    }

                    match find_property(&node_type, name) {
                        Some(declared) => {
                            if let (Some(actual), Some(min), Some(max)) =
                                (value.as_f64(), declared.min, declared.max)
                            {
                                if actual < min || actual > max {
                                    warnings.push(format!(
                                        "Node {node_id} ({node_type}): '{name}' set to {actual}, outside {min}..{max}"
                                    ));
                                }
                            }
                        },
                        None => {
                            if !allow_unknown {
                                rejected.push(format!(
                                    "Node {node_id} ({node_type}) has no property '{name}'; Gaea would ignore it \
                                     silently. Pass allow_unknown_properties to write it anyway."
                                ));
                                continue;
                            }
                            warnings.push(format!(
                                "Node {node_id} ({node_type}): '{name}' is not declared by this node type"
                            ));
                        },
                    }

                    node_obj.insert(name.clone(), value.clone());
                    applied.push(format!("Node {node_id} ({node_type}): {name} = {value}"));
                }
            }
        }

        if applied.is_empty() {
            return ToolResult::json(&json!({
                "success": false,
                "project_path": project_path,
                "backup_path": backup_path,
                "applied": applied,
                "warnings": warnings,
                "rejected": rejected,
                "message": "Nothing was changed; the file was left as it was"
            }));
        }

        save_project(&path, &project).await?;

        ToolResult::json(&json!({
            "success": true,
            "project_path": project_path,
            "backup_path": backup_path,
            "applied": applied,
            "warnings": warnings,
            "rejected": rejected
        }))
    }
}

// =============================================================================
// Tool: calibrate_gaea2_nodes
// =============================================================================

/// Measures what generators actually produce, because a parameter is not an outcome.
///
/// `Mountain` with `Height 0.42` at `Scale 0.16` fills 3% of the vertical range: the height
/// scales with the footprint, and nothing in the metadata says so. Finding that out cost a
/// build, a saved probe output and a separate measurement. This does the same for every
/// generator at once, since Gaea writes one file per saved node and a single build can carry
/// them all.
struct CalibrateNodesTool {
    refs: ServerRefs,
}

#[async_trait]
impl Tool for CalibrateNodesTool {
    fn name(&self) -> &str {
        "calibrate_gaea2_nodes"
    }

    fn description(&self) -> &str {
        "Build generator nodes and measure what they actually produce: the share of the vertical \
         range each one fills, and how that changes across a property sweep and across seeds. \
         Answers 'how tall does this come out', which no declared range can. Writes a \
         calibration file that gaea2_node_info then reports alongside the schema."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Node types to measure. Defaults to every generator in the Primitive and Terrain categories."
                },
                "property": {
                    "type": "string",
                    "description": "Property to sweep, e.g. 'Height'. Omit to measure at defaults only."
                },
                "steps": {
                    "type": "integer",
                    "description": "Points in the sweep, spread across the declared range including both ends",
                    "default": 3,
                    "minimum": 2,
                    "maximum": 9
                },
                "seeds": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Seeds to build each point with; the spread across them says how much the seed alone moves the result",
                    "default": [1001, 2002, 3003]
                },
                "resolution": {
                    "type": "integer",
                    "description": "Build resolution. 512 is enough: amplitude is a property of the shape, not of the sampling.",
                    "default": 512
                },
                "output_path": {
                    "type": "string",
                    "description": "Where to write the calibration. Defaults to gaea_calibration.json in the output directory."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Seconds to allow per build",
                    "default": 1800
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        use crate::calibration::{
            self, default_path, measurable_nodes, measure, median, sweep_values, Calibration,
            Measurement,
        };

        let Some(cli) = self.refs.cli.as_ref() else {
            return Err(MCPError::InvalidParameters(
                "Gaea is not configured; set GAEA2_PATH to measure anything".to_string(),
            ));
        };

        let nodes: Vec<String> = match args.get("nodes").and_then(|v| v.as_array()) {
            Some(list) => list
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            None => measurable_nodes().into_iter().map(str::to_string).collect(),
        };
        if nodes.is_empty() {
            return Err(MCPError::InvalidParameters(
                "No nodes to measure".to_string(),
            ));
        }
        for node in &nodes {
            if !crate::schema::is_valid_node_type(node) {
                return Err(MCPError::InvalidParameters(format!(
                    "'{node}' is not a node type in Gaea {}",
                    crate::schema::GAEA_VERSION
                )));
            }
        }

        let property = args
            .get("property")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let steps = args
            .get("steps")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .clamp(2, 9) as usize;
        let seeds: Vec<i64> = match args.get("seeds").and_then(|v| v.as_array()) {
            Some(list) => list.iter().filter_map(|v| v.as_i64()).collect(),
            None => vec![1001, 2002, 3003],
        };
        if seeds.is_empty() {
            return Err(MCPError::InvalidParameters(
                "At least one seed is needed".to_string(),
            ));
        }
        let resolution = args
            .get("resolution")
            .and_then(|v| v.as_u64())
            .unwrap_or(512) as u32;
        if !crate::schema::is_valid_build_resolution(resolution) {
            return Err(MCPError::InvalidParameters(format!(
                "{resolution} is not a build resolution Gaea accepts; use a power of two from \
                 256 to 8192"
            )));
        }
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(1800);

        // One build per sweep point, carrying every node at every seed. Gaea writes a file per
        // saved node, so a point costs one launch regardless of how many nodes ride on it -
        // which is what makes measuring forty generators affordable at all.
        //
        // A point is a fraction of each node's own declared range, not an absolute value:
        // Height runs to 3 on one node and to 1 on another, and pooling absolute values would
        // turn three points into one build per distinct number across forty nodes.
        let points: Vec<Option<f64>> = match property.as_deref() {
            None => vec![None],
            Some(prop) => {
                let usable = nodes
                    .iter()
                    .any(|node| !sweep_values(node, prop, steps).is_empty());
                if !usable {
                    return Err(MCPError::InvalidParameters(format!(
                        "None of the given nodes declare a numeric range for '{prop}'"
                    )));
                }
                (0..steps)
                    .map(|i| Some(i as f64 / (steps - 1).max(1) as f64))
                    .collect()
            },
        };

        let work_dir = self.refs.config.output_dir.join("calibration");
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| MCPError::ToolExecutionFailed(format!("Cannot create work directory: {e}")))?;

        let mut measurements: Vec<Measurement> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();

        // A single node that faults takes the whole build down with it, and a batch of forty
        // then yields nothing. Failed batches are halved and retried, so one bad node costs a
        // few extra builds instead of the entire run - and ends up named rather than hidden.
        let mut batches: Vec<(usize, Vec<String>)> = points
            .iter()
            .enumerate()
            .map(|(index, _)| (index, nodes.clone()))
            .collect();
        let mut attempt = 0usize;

        while let Some((point_index, batch)) = batches.pop() {
            let point = &points[point_index];
            attempt += 1;
            let mut planned: Vec<(String, String)> = Vec::new();
            let mut workflow_nodes: Vec<Value> = Vec::new();
            let mut next_id = 10;

            // Absolute value each node takes at this point, kept for the record: 0.5 of Height
            // means 1.5 on one node and 0.5 on another, and the measurement is worthless
            // without saying which.
            let mut actual_value: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();

            for node_type in &batch {
                let mut properties = serde_json::Map::new();
                if let Some(fraction) = point {
                    let Some(prop) = property.as_deref() else {
                        continue;
                    };
                    // A node that does not declare this property with a range sits this out
                    // rather than being forced to a value it rejects.
                    let declared = crate::schema::find_property(node_type, prop);
                    let Some((min, max)) = declared.and_then(|d| Some((d.min?, d.max?))) else {
                        continue;
                    };
                    let value = ((min + (max - min) * fraction) * 10_000.0).round() / 10_000.0;
                    actual_value.insert(node_type.clone(), value);
                    properties.insert(prop.to_string(), json!(value));
                }

                for seed in &seeds {
                    if crate::schema::is_generator_node(node_type) {
                        properties.insert("Seed".to_string(), json!(seed));
                    }
                    // Gaea names an output after the node that fed it, so the node name is how a
                    // file finds its way back to what produced it.
                    let stem = format!("cal_{node_type}_{seed}");
                    workflow_nodes.push(json!({
                        "id": next_id,
                        "type": node_type,
                        "name": stem,
                        "properties": Value::Object(properties.clone()),
                        "save_definition": {"filename": stem, "format": "UshortRaw16"}
                    }));
                    planned.push((stem, node_type.clone()));
                    next_id += 10;
                }
            }

            if planned.is_empty() {
                continue;
            }

            let label = match point {
                Some(v) => format!("{v}"),
                None => "defaults".to_string(),
            };
            // Each attempt gets its own directory: a retried half must not measure the files a
            // failed batch happened to write before it faulted.
            let build_dir = work_dir.join(format!("point_{}_{attempt}", label.replace('.', "_")));
            // A file left by an earlier run would be measured as if it were fresh.
            let _ = tokio::fs::remove_dir_all(&build_dir).await;

            let parsed = parse_nodes(&workflow_nodes)
                .map_err(|e| MCPError::ToolExecutionFailed(format!("Cannot build the probe: {e}")))?;
            let workflow = Workflow {
                nodes: parsed,
                connections: Vec::new(),
            };
            let build_config = BuildConfig {
                resolution: resolution as i32,
                ..Default::default()
            };
            let project_file = work_dir.join(format!("calibration_{label}.terrain"));
            let project_path = project_file.to_string_lossy().to_string();
            generate_project(
                &format!("Calibration_{label}"),
                &workflow,
                Some(build_config),
                Some(&project_path),
            )
            .await
            .map_err(MCPError::ToolExecutionFailed)?;

            let result = cli
                .run_project(
                    &project_path,
                    &resolution.to_string(),
                    Some(&build_dir.to_string_lossy()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    true,
                    false,
                    timeout,
                )
                .await;

            if !result.success && result.file_count == 0 {
                let reason = result
                    .error
                    .unwrap_or_else(|| "the build wrote nothing".to_string());
                if batch.len() > 1 {
                    // Halve and retry: whichever half holds the offender fails again, and the
                    // other half is measured normally.
                    let half = batch.len() / 2;
                    batches.push((point_index, batch[half..].to_vec()));
                    batches.push((point_index, batch[..half].to_vec()));
                } else {
                    skipped.push(json!({
                        "node": batch[0],
                        "point": label,
                        "reason": reason
                    }));
                }
                continue;
            }

            let mut per_node: std::collections::HashMap<String, Vec<(f64, f64)>> =
                std::collections::HashMap::new();
            for (stem, node_type) in &planned {
                let file = build_dir.join(format!("{stem}_Out.r16"));
                match measure(&file) {
                    Ok(pair) => per_node.entry(node_type.clone()).or_default().push(pair),
                    Err(_) => {
                        skipped.push(json!({
                            "node": node_type,
                            "point": label,
                            "reason": "the build produced no file for this node"
                        }));
                    },
                }
            }

            for (node_type, pairs) in per_node {
                let mut amplitudes: Vec<f64> = pairs.iter().map(|(a, _)| *a).collect();
                let mut levels: Vec<f64> = pairs.iter().map(|(_, l)| *l).collect();
                let low = amplitudes.iter().copied().fold(f64::MAX, f64::min);
                let high = amplitudes.iter().copied().fold(f64::MIN, f64::max);
                let value = actual_value.get(&node_type).copied();
                measurements.push(Measurement {
                    property: point.and(property.clone()),
                    value,
                    node_type,
                    seeds: seeds.clone(),
                    amplitude: median(&mut amplitudes),
                    amplitude_low: low,
                    amplitude_high: high,
                    mean_level: median(&mut levels),
                });
            }
        }

        if measurements.is_empty() {
            return Err(MCPError::ToolExecutionFailed(format!(
                "Nothing was measured. {}",
                serde_json::to_string(&skipped).unwrap_or_default()
            )));
        }

        let fresh = Calibration {
            gaea_version: crate::schema::GAEA_VERSION.to_string(),
            measured_at: Utc::now().to_rfc3339(),
            resolution,
            measurements,
        };

        let out = args
            .get("output_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_path(&self.refs.config.output_dir));
        // A run covers one property; folding into what is there keeps earlier sweeps.
        let taken = fresh.measurements.len();
        let calibration = calibration::merge(calibration::load(&out), fresh);
        calibration::save(&out, &calibration).map_err(MCPError::ToolExecutionFailed)?;

        // Report the outliers rather than the whole table: the useful thing to see first is
        // which nodes do not do what their parameters suggest.
        let mut quiet: Vec<Value> = calibration
            .measurements
            .iter()
            .filter(|m| m.amplitude < 0.2)
            .map(|m| {
                json!({
                    "node": m.node_type,
                    "value": m.value,
                    "fills": format!("{:.1}%", m.amplitude * 100.0)
                })
            })
            .collect();
        quiet.truncate(15);

        ToolResult::json(&json!({
            "success": true,
            "calibration_path": out.to_string_lossy(),
            "gaea_version": calibration.gaea_version,
            "resolution": resolution,
            "nodes_measured": calibration::by_node(&calibration.measurements).len(),
            "measurements_taken": taken,
            "measurements_on_file": calibration.measurements.len(),
            "sweep": property,
            "points": points.len(),
            "fills_under_a_fifth_of_the_range": quiet,
            "skipped": skipped
        }))
    }
}
