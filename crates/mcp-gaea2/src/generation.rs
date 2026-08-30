//! Project generation for Gaea2 terrain files.
//!
//! Emits .terrain files for the installed Gaea build, using the node types, port layouts and
//! intrinsic modifiers extracted from it (see [`crate::schema`]).

use std::collections::HashMap;
use std::path::Path;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::schema::{
    find_similar_node_type, get_default_ports, get_intrinsic_modifiers, is_generator_node,
    is_valid_node_type,
};
use crate::types::{
    BuildConfig, Connection, Modifier, Node, PortDefinition, Position, SaveDefinition, Workflow,
};

/// Generate a Gaea2 project file from a workflow.
pub async fn generate_project(
    project_name: &str,
    workflow: &Workflow,
    build_config: Option<BuildConfig>,
    output_path: Option<&str>,
) -> Result<Value, String> {
    let project_id = Uuid::new_v4().to_string();
    let terrain_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%SZ").to_string();

    let build_config = build_config.unwrap_or_default();

    // Create project structure matching Gaea2 2.2.6.0 format
    let mut ref_id_counter = 25;

    // Process nodes
    let mut nodes_dict = serde_json::Map::new();
    nodes_dict.insert("$id".to_string(), json!("6"));

    for node in &workflow.nodes {
        let (node_obj, next_ref) = create_node_object(node, ref_id_counter)?;
        nodes_dict.insert(node.id.to_string(), node_obj);
        ref_id_counter = next_ref;
    }

    // Add connections to nodes
    for conn in &workflow.connections {
        add_connection_to_nodes(&mut nodes_dict, conn, &mut ref_id_counter);
    }

    let project = json!({
        "$id": "1",
        "Assets": {
            "$id": "2",
            "$values": [{
                "$id": "3",
                "Terrain": {
                    "$id": "4",
                    "Id": terrain_id,
                    "Metadata": {
                        "$id": "5",
                        "Name": project_name,
                        "Description": "",
                        "Version": crate::schema::GAEA_VERSION,
                        "DateCreated": timestamp,
                        "DateLastBuilt": timestamp,
                        "DateLastSaved": timestamp,
                        "ModifiedVersion": crate::schema::GAEA_VERSION
                    },
                    "Nodes": Value::Object(nodes_dict),
                    "Groups": {"$id": "7"},
                    "Notes": {"$id": "8"},
                    "GraphTabs": {
                        "$id": "9",
                        "$values": [{
                            "$id": "10",
                            "Name": "Graph 1",
                            "Color": "Brass",
                            "ZoomFactor": 0.6299605249474372,
                            "ViewportLocation": {
                                "$id": "11",
                                "X": 27690.082,
                                "Y": 25804.441
                            }
                        }]
                    },
                    "Width": 5000.0,
                    "Height": 2500.0,
                    "Ratio": 0.5,
                    "Regions": {"$id": "12", "$values": []}
                },
                "Automation": {
                    "$id": "13",
                    "Bindings": {"$id": "14", "$values": []},
                    "Expressions": {"$id": "15"},
                    "Variables": {"$id": "16"}
                },
                "BuildDefinition": {
                    "$id": "17",
                    "Type": build_config.build_type,
                    "Destination": "<Builds>\\[Filename]\\[+++]",
                    "Resolution": build_config.resolution,
                    "BakeResolution": build_config.bake_resolution,
                    "TileResolution": build_config.tile_resolution,
                    "BucketResolution": build_config.resolution,
                    "NumberOfTiles": build_config.number_of_tiles,
                    "EdgeBlending": build_config.edge_blending,
                    "TileZeroIndex": true,
                    "TilePattern": "_y%Y%_x%X%",
                    "OrganizeFiles": "NodeSubFolder",
                    "ColorSpace": build_config.color_space
                },
                "State": {
                    "$id": "18",
                    "BakeResolution": 2048,
                    "PreviewResolution": 1024,
                    "HDResolution": 4096,
                    "SelectedNode": -1,
                    "NodeBookmarks": {"$id": "19", "$values": []},
                    "Viewport": {
                        "$id": "20",
                        "CameraPosition": {"$id": "21", "$values": []},
                        "Camera": {"$id": "22"},
                        "RenderMode": "Realistic",
                        "AmbientOcclusion": true,
                        "Shadows": true
                    }
                },
                "BuildProfiles": {"$id": "23"}
            }]
        },
        "Id": &project_id[..8],
        "Branch": 1,
        "Metadata": {
            "$id": "24",
            "Name": project_name,
            "Description": "",
            "Version": crate::schema::GAEA_VERSION,
            "Edition": "G2P",
            "Owner": "",
            "DateCreated": timestamp,
            "DateLastBuilt": timestamp,
            "DateLastSaved": timestamp,
            "ModifiedVersion": crate::schema::GAEA_VERSION
        }
    });

    // Write to file if output path provided
    if let Some(path) = output_path {
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create directory: {e}"))?;
        }
        let content = serde_json::to_string_pretty(&project)
            .map_err(|e| format!("Failed to serialize project: {e}"))?;
        tokio::fs::write(path, content)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;
    }

    Ok(project)
}

/// Create a node object in Gaea2 format.
///
/// The node type is checked against the installed build's schema first. Writing an unknown type
/// produces a `$type` Gaea cannot resolve, and the whole project then fails to open as corrupt -
/// the file is only rejected at load time, long after this function claimed success.
fn create_node_object(node: &Node, mut ref_id_counter: u32) -> Result<(Value, u32), String> {
    if !is_valid_node_type(&node.node_type) {
        let hint = match find_similar_node_type(&node.node_type) {
            Some(similar) => format!(" Did you mean '{similar}'?"),
            None => String::new(),
        };
        return Err(format!(
            "Node {} has type '{}', which does not exist in Gaea {}. Gaea would reject the \
             generated project as corrupt.{}",
            node.id,
            node.node_type,
            crate::schema::GAEA_VERSION,
            hint
        ));
    }

    let node_id_ref = ref_id_counter.to_string();

    let mut node_obj = serde_json::Map::new();

    // Required identifiers
    node_obj.insert("$id".to_string(), json!(node_id_ref));
    node_obj.insert(
        "$type".to_string(),
        json!(format!(
            "QuadSpinner.Gaea.Nodes.{}, Gaea.Nodes",
            node.node_type
        )),
    );

    // Node-specific properties
    for (key, value) in &node.properties {
        // An enumerated property written as a number stops the whole build, silently.
        crate::schema::check_property_value(&node.node_type, key, value)
            .map_err(|e| format!("Node {}: {e}", node.id))?;
        node_obj.insert(key.clone(), value.clone());
    }

    // Root-level X/Y: where nodes like Mountain, Crater and Shape sit on the map, in
    // normalized 0-1 coordinates. They are ordinary declared properties, so a value the caller
    // supplied has to win. Writing the default unconditionally silently recentred every such
    // node - two Shape masks aimed at opposite edges of the map both came out at 0.5, on top
    // of each other, while the request that produced them looked correct.
    for axis in ["X", "Y"] {
        node_obj
            .entry(axis.to_string())
            .or_insert_with(|| json!(0.5));
    }

    // Seed for generator nodes
    if is_generator_node(&node.node_type) && !node.properties.contains_key("Seed") {
        let seed = rand::random::<u32>() % 90000 + 10000;
        node_obj.insert("Seed".to_string(), json!(seed));
    }

    // Standard properties
    node_obj.insert("Id".to_string(), json!(node.id));
    node_obj.insert("Name".to_string(), json!(node.name));

    ref_id_counter += 1;
    node_obj.insert(
        "Position".to_string(),
        json!({
            "$id": ref_id_counter.to_string(),
            "X": node.position.x,
            "Y": node.position.y
        }),
    );

    ref_id_counter += 1;
    let ports_id = ref_id_counter.to_string();
    ref_id_counter += 1;
    let modifiers_id = ref_id_counter.to_string();
    ref_id_counter += 1;

    // Create ports
    let port_defs = node.ports.as_ref().map(|p| {
        p.iter()
            .map(|pd| (pd.name.as_str(), pd.port_type.as_str()))
            .collect::<Vec<_>>()
    });
    let default_ports = get_default_ports(&node.node_type);
    let ports_to_use = port_defs.as_deref().unwrap_or(&default_ports);

    let mut port_values = Vec::new();
    for (port_name, port_type) in ports_to_use {
        let port = json!({
            "$id": ref_id_counter.to_string(),
            "Name": port_name,
            "Type": port_type,
            "IsExporting": true,
            "Parent": {"$ref": node_id_ref}
        });
        port_values.push(port);
        ref_id_counter += 1;
    }

    node_obj.insert(
        "Ports".to_string(),
        json!({
            "$id": ports_id,
            "$values": port_values
        }),
    );

    // Modifiers. Some nodes carry intrinsic ones that Gaea attaches itself; a node serialized
    // without them can fault at build time (Thermal2 without its Max threw "Index was outside
    // the bounds of the array"), so they are emitted whenever the caller supplied none.
    let modifier_values: Vec<Value> = match &node.modifiers {
        Some(modifiers) => modifiers
            .iter()
            .map(|m| {
                if !crate::schema::is_valid_modifier_type(&m.modifier_type) {
                    return Err(format!(
                        "Node {} has modifier '{}', which does not exist in Gaea {}. Available: \
                         {}",
                        node.id,
                        m.modifier_type,
                        crate::schema::GAEA_VERSION,
                        crate::schema::modifier_types().join(", ")
                    ));
                }

                let mut mod_obj = json!({
                    "$id": ref_id_counter.to_string(),
                    "$type": format!("QuadSpinner.Gaea.Nodes.Modifiers.{}, Gaea.Nodes", m.modifier_type),
                    "Name": m.modifier_type,
                    "Parent": {"$ref": node_id_ref},
                    "Intrinsic": true
                });
                let obj = mod_obj
                    .as_object_mut()
                    .expect("json! built an object just above");

                // Gaea writes a modifier's settings as plain fields on the modifier itself, and
                // marks the ones that have settings with HasUI. Dropping them left the modifier
                // in the file with none of the values the caller asked for.
                for (key, value) in &m.properties {
                    crate::schema::check_modifier_property(&m.modifier_type, key, value)
                        .map_err(|e| format!("Node {}: {e}", node.id))?;
                    obj.insert(key.clone(), value.clone());
                }
                if m.has_ui || !m.properties.is_empty() {
                    obj.insert("HasUI".to_string(), json!(true));
                }

                let order = m.order.map(i64::from).or_else(|| {
                    get_intrinsic_modifiers(&node.node_type)
                        .iter()
                        .find(|(name, _)| *name == m.modifier_type)
                        .map(|(_, order)| *order)
                });
                if let Some(order) = order {
                    obj.insert("Order".to_string(), json!(order));
                }

                ref_id_counter += 1;
                Ok(mod_obj)
            })
            .collect::<Result<Vec<Value>, String>>()?,
        None => get_intrinsic_modifiers(&node.node_type)
            .iter()
            .map(|(modifier_type, order)| {
                let mod_obj = json!({
                    "$id": ref_id_counter.to_string(),
                    "$type": format!("QuadSpinner.Gaea.Nodes.Modifiers.{}, Gaea.Nodes", modifier_type),
                    "Name": modifier_type,
                    "Parent": {"$ref": node_id_ref},
                    "Intrinsic": true,
                    "Order": order
                });
                ref_id_counter += 1;
                mod_obj
            })
            .collect(),
    };

    node_obj.insert(
        "Modifiers".to_string(),
        json!({
            "$id": modifiers_id,
            "$values": modifier_values
        }),
    );

    // Save definition. Any node can write a file; this is how most Gaea projects export, so a
    // generated project could not produce output at all while this was dropped on the floor.
    if let Some(save) = &node.save_definition {
        ref_id_counter += 1;
        let save_id = ref_id_counter.to_string();
        ref_id_counter += 1;
        let profiles_id = ref_id_counter.to_string();
        ref_id_counter += 1;

        node_obj.insert(
            "SaveDefinition".to_string(),
            json!({
                "$id": save_id,
                "Node": node.id,
                "Filename": save.filename,
                "Format": save.format,
                "IsEnabled": save.enabled,
                "DisabledInProfiles": {
                    "$id": profiles_id,
                    "$values": save.disabled_profiles
                }
            }),
        );
    }

    Ok((Value::Object(node_obj), ref_id_counter))
}

/// Add a connection to the nodes in the project.
fn add_connection_to_nodes(
    nodes_dict: &mut serde_json::Map<String, Value>,
    conn: &Connection,
    ref_id_counter: &mut u32,
) {
    let to_id = conn.to_node.to_string();

    if let Some(Value::Object(target_node)) = nodes_dict.get_mut(&to_id) {
        if let Some(Value::Object(ports)) = target_node.get_mut("Ports") {
            if let Some(Value::Array(port_values)) = ports.get_mut("$values") {
                for port in port_values.iter_mut() {
                    if let Value::Object(port_obj) = port {
                        if port_obj.get("Name") == Some(&json!(conn.to_port)) {
                            // Update port type to Required if input
                            if let Some(Value::String(port_type)) = port_obj.get("Type") {
                                if port_type.contains("In") && !port_type.contains("Required") {
                                    port_obj.insert(
                                        "Type".to_string(),
                                        json!(format!("{}, Required", port_type)),
                                    );
                                }
                            }

                            // Add connection record
                            port_obj.insert(
                                "Record".to_string(),
                                json!({
                                    "$id": ref_id_counter.to_string(),
                                    "From": conn.from_node,
                                    "To": conn.to_node,
                                    "FromPort": conn.from_port,
                                    "ToPort": conn.to_port,
                                    "IsValid": true
                                }),
                            );
                            *ref_id_counter += 1;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Parse nodes from JSON input.
pub fn parse_nodes(nodes_json: &[Value]) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();

    for (i, node_val) in nodes_json.iter().enumerate() {
        let obj = node_val
            .as_object()
            .ok_or_else(|| format!("Node {i} is not an object"))?;

        let id = obj
            .get("id")
            .and_then(|v| v.as_i64())
            .unwrap_or((100 + i) as i64) as i32;

        let node_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!("Node {id} has no 'type'; a node type must be given explicitly")
            })?
            .to_string();

        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&node_type)
            .to_string();

        let position = if let Some(pos) = obj.get("position") {
            Position {
                x: pos
                    .get("x")
                    .or_else(|| pos.get("X"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(25000.0),
                y: pos
                    .get("y")
                    .or_else(|| pos.get("Y"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(25000.0),
            }
        } else {
            Position::default()
        };

        let properties = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .map(|m| m.clone().into_iter().collect())
            .unwrap_or_default();

        // Optional shapes: explicit ports, modifiers and a save definition. Absent fields keep
        // the schema defaults; a present one is honoured verbatim.
        let ports = obj
            .get("ports")
            .map(|v| {
                serde_json::from_value::<Vec<PortDefinition>>(v.clone())
                    .map_err(|e| format!("Node {id}: invalid 'ports': {e}"))
            })
            .transpose()?;

        let modifiers = obj
            .get("modifiers")
            .map(|v| {
                serde_json::from_value::<Vec<Modifier>>(v.clone())
                    .map_err(|e| format!("Node {id}: invalid 'modifiers': {e}"))
            })
            .transpose()?;

        let save_definition = obj
            .get("save_definition")
            .or_else(|| obj.get("save"))
            .map(|v| {
                serde_json::from_value::<SaveDefinition>(v.clone())
                    .map_err(|e| format!("Node {id}: invalid 'save_definition': {e}"))
            })
            .transpose()?;

        nodes.push(Node {
            id,
            node_type,
            name,
            position,
            properties,
            ports,
            modifiers,
            save_definition,
        });
    }

    Ok(nodes)
}

/// Parse connections from JSON input.
pub fn parse_connections(connections_json: &[Value]) -> Result<Vec<Connection>, String> {
    let mut connections = Vec::new();

    for conn_val in connections_json {
        if let Some(obj) = conn_val.as_object() {
            let from_node = obj
                .get("from_node")
                .or_else(|| obj.get("from"))
                .or_else(|| obj.get("source"))
                .and_then(|v| v.as_i64())
                .ok_or("Connection missing from_node")? as i32;

            let to_node = obj
                .get("to_node")
                .or_else(|| obj.get("to"))
                .or_else(|| obj.get("target"))
                .and_then(|v| v.as_i64())
                .ok_or("Connection missing to_node")? as i32;

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

            connections.push(Connection {
                from_node,
                to_node,
                from_port,
                to_port,
            });
        } else if let Some(arr) = conn_val.as_array() {
            // Handle [from, to] format
            if arr.len() >= 2 {
                connections.push(Connection {
                    from_node: arr[0].as_i64().unwrap_or(0) as i32,
                    to_node: arr[1].as_i64().unwrap_or(0) as i32,
                    from_port: "Out".to_string(),
                    to_port: "In".to_string(),
                });
            }
        }
    }

    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_generate_project() {
        let workflow = Workflow {
            nodes: vec![
                Node {
                    id: 100,
                    node_type: "Mountain".to_string(),
                    name: "Mountain".to_string(),
                    position: Position {
                        x: 25000.0,
                        y: 25000.0,
                    },
                    properties: HashMap::new(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
                Node {
                    id: 101,
                    node_type: "Export".to_string(),
                    name: "Export".to_string(),
                    position: Position {
                        x: 25300.0,
                        y: 25000.0,
                    },
                    properties: HashMap::new(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
            ],
            connections: vec![Connection {
                from_node: 100,
                to_node: 101,
                from_port: "Out".to_string(),
                to_port: "In".to_string(),
            }],
        };

        let result = generate_project("test_project", &workflow, None, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn refuses_a_node_type_gaea_does_not_have() {
        let workflow = Workflow {
            nodes: vec![Node {
                id: 100,
                node_type: "Bias".to_string(),
                name: "Bias".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: None,
                save_definition: None,
            }],
            connections: vec![],
        };

        let err = generate_project("bad_project", &workflow, None, None)
            .await
            .expect_err("an unknown node type must not reach the file");
        assert!(err.contains("Bias"), "error should name the type: {err}");
    }

    #[tokio::test]
    async fn emits_intrinsic_modifier_for_thermal2() {
        let workflow = Workflow {
            nodes: vec![Node {
                id: 400,
                node_type: "Thermal2".to_string(),
                name: "Thermal2".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: None,
                save_definition: None,
            }],
            connections: vec![],
        };

        let project = generate_project("thermal", &workflow, None, None)
            .await
            .expect("Thermal2 is a valid type");
        let node = &project["Assets"]["$values"][0]["Terrain"]["Nodes"]["400"];

        let modifiers = node["Modifiers"]["$values"]
            .as_array()
            .expect("modifiers array");
        assert_eq!(modifiers.len(), 1, "Thermal2 carries an intrinsic Max");
        assert_eq!(modifiers[0]["Name"], "Max");
        assert_eq!(modifiers[0]["Order"], 66);

        let ports: Vec<&str> = node["Ports"]["$values"]
            .as_array()
            .expect("ports array")
            .iter()
            .filter_map(|p| p["Name"].as_str())
            .collect();
        assert_eq!(
            ports,
            vec![
                "In",
                "Out",
                "AreaMask",
                "SedimentRemoval",
                "Wear",
                "Deposits"
            ]
        );
    }

    #[tokio::test]
    async fn writes_the_settings_of_a_modifier() {
        let mut properties = HashMap::new();
        properties.insert("Level".to_string(), serde_json::json!(0.55));

        let workflow = Workflow {
            nodes: vec![Node {
                id: 1,
                node_type: "Mountain".to_string(),
                name: "Mountain".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: Some(vec![Modifier {
                    modifier_type: "Threshold".to_string(),
                    properties,
                    order: None,
                    has_ui: false,
                }]),
                save_definition: None,
            }],
            connections: vec![],
        };

        let project = generate_project("modifier", &workflow, None, None)
            .await
            .expect("Threshold is a real modifier");
        let modifier =
            &project["Assets"]["$values"][0]["Terrain"]["Nodes"]["1"]["Modifiers"]["$values"][0];

        assert_eq!(modifier["Name"], "Threshold");
        assert_eq!(modifier["Level"], 0.55, "the setting must reach the file");
        assert_eq!(
            modifier["HasUI"], true,
            "Gaea marks modifiers that carry settings"
        );
    }

    #[tokio::test]
    async fn refuses_a_modifier_gaea_does_not_have() {
        let workflow = Workflow {
            nodes: vec![Node {
                id: 1,
                node_type: "Mountain".to_string(),
                name: "Mountain".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: Some(vec![Modifier {
                    modifier_type: "NotAModifier".to_string(),
                    properties: HashMap::new(),
                    order: None,
                    has_ui: false,
                }]),
                save_definition: None,
            }],
            connections: vec![],
        };

        let err = generate_project("bad_modifier", &workflow, None, None)
            .await
            .expect_err("an unknown modifier must not reach the file");
        assert!(err.contains("NotAModifier"), "{err}");
    }

    #[tokio::test]
    async fn always_writes_the_build_type() {
        // Without BuildDefinition.Type Gaea exits 0 and writes nothing at all.
        let workflow = Workflow {
            nodes: vec![Node {
                id: 1,
                node_type: "Mountain".to_string(),
                name: "Mountain".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: None,
                save_definition: None,
            }],
            connections: vec![],
        };

        let project = generate_project("build_type", &workflow, None, None)
            .await
            .expect("Mountain is a valid type");
        let build = &project["Assets"]["$values"][0]["BuildDefinition"];
        assert_eq!(build["Type"], "Standard");
        assert!(build["Destination"].is_string());
    }

    #[tokio::test]
    async fn keeps_the_placement_the_caller_asked_for() {
        // X and Y place the node on the map. Two shapes aimed at opposite edges must not both
        // end up in the centre.
        let mut north = HashMap::new();
        north.insert("Y".to_string(), serde_json::json!(0.82));
        let mut south = HashMap::new();
        south.insert("Y".to_string(), serde_json::json!(0.18));
        south.insert("X".to_string(), serde_json::json!(0.3));

        let workflow = Workflow {
            nodes: vec![
                Node {
                    id: 10,
                    node_type: "Shape".to_string(),
                    name: "North".to_string(),
                    position: Position::default(),
                    properties: north,
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
                Node {
                    id: 20,
                    node_type: "Shape".to_string(),
                    name: "South".to_string(),
                    position: Position::default(),
                    properties: south,
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
                Node {
                    id: 30,
                    node_type: "Shape".to_string(),
                    name: "Default".to_string(),
                    position: Position::default(),
                    properties: HashMap::new(),
                    ports: None,
                    modifiers: None,
                    save_definition: None,
                },
            ],
            connections: vec![],
        };

        let project = generate_project("placement", &workflow, None, None)
            .await
            .expect("Shape is a valid type");
        let nodes = &project["Assets"]["$values"][0]["Terrain"]["Nodes"];

        assert_eq!(nodes["10"]["Y"], 0.82);
        assert_eq!(nodes["20"]["Y"], 0.18);
        assert_eq!(nodes["20"]["X"], 0.3);
        // Untouched axes still get the centre default.
        assert_eq!(nodes["10"]["X"], 0.5);
        assert_eq!(nodes["30"]["X"], 0.5);
        assert_eq!(nodes["30"]["Y"], 0.5);
    }

    #[tokio::test]
    async fn writes_the_save_definition_of_an_exporting_node() {
        let workflow = Workflow {
            nodes: vec![Node {
                id: 250,
                node_type: "Unity".to_string(),
                name: "Unity".to_string(),
                position: Position::default(),
                properties: HashMap::new(),
                ports: None,
                modifiers: None,
                save_definition: Some(SaveDefinition {
                    filename: "height".to_string(),
                    format: "UshortRaw16".to_string(),
                    enabled: true,
                    disabled_profiles: vec![],
                }),
            }],
            connections: vec![],
        };

        let project = generate_project("export", &workflow, None, None)
            .await
            .expect("Unity is a valid type");
        let save = &project["Assets"]["$values"][0]["Terrain"]["Nodes"]["250"]["SaveDefinition"];
        assert_eq!(save["Filename"], "height");
        assert_eq!(save["Format"], "UshortRaw16");
        assert_eq!(save["IsEnabled"], true);
        assert_eq!(save["Node"], 250);
    }

    #[test]
    fn test_parse_nodes() {
        let nodes_json = vec![
            json!({"id": 1, "type": "Mountain", "name": "Mountain"}),
            json!({"id": 2, "type": "Export", "name": "Export"}),
        ];

        let nodes = parse_nodes(&nodes_json).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].node_type, "Mountain");
    }

    #[test]
    fn parse_nodes_requires_an_explicit_type() {
        let nodes_json = vec![json!({"id": 1, "name": "Mystery"})];
        assert!(parse_nodes(&nodes_json).is_err());
    }

    #[test]
    fn parse_nodes_reads_a_save_definition() {
        let nodes_json = vec![json!({
            "id": 7,
            "type": "Unity",
            "save_definition": {"filename": "height", "format": "UshortRaw16"}
        })];

        let nodes = parse_nodes(&nodes_json).unwrap();
        let save = nodes[0].save_definition.as_ref().expect("save definition");
        assert_eq!(save.filename, "height");
        assert!(save.enabled, "a save definition defaults to enabled");
    }

    #[test]
    fn test_parse_connections() {
        let conns_json = vec![json!({"from_node": 1, "to_node": 2}), json!([1, 3])];

        let connections = parse_connections(&conns_json).unwrap();
        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0].from_node, 1);
        assert_eq!(connections[0].to_node, 2);
    }
}
