//! Schema queries for Gaea node types, ports and properties.
//!
//! The data itself lives in [`crate::gaea_schema_generated`], extracted from an installed Gaea
//! build by `tools/extract_gaea_schema.ps1`. This module only queries it.
//!
//! The previous hand-written list described Gaea 2.2.6.0 and had drifted from the product: it
//! accepted 23 node types that do not exist (`Output`, `Max`, `Min`, `Multiply`, `Blend`, `Math`,
//! `QuickColor`, `Satmaps`, `Rockmap`, `Portal*`, `Coast`, `Sediment`, ...) while rejecting 15
//! real ones (`Invert`, `ColorThreshold`, `MathX`, `River2`, `Pond`, `Sediments`, ...). Anything
//! built from an invented type serializes to `QuadSpinner.Gaea.Nodes.<Type>, Gaea.Nodes`, which
//! Gaea then refuses to load as a corrupt file.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::gaea_schema_generated as gen;

pub use crate::gaea_schema_generated::{NodeProperty, GAEA_VERSION};

/// Ports every node has unless the extracted table says otherwise.
const FALLBACK_PORTS: &[(&str, &str)] = &[("In", "PrimaryIn"), ("Out", "PrimaryOut")];

/// All node types the installed Gaea build can deserialize.
pub static VALID_NODE_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut types = HashSet::new();
    for (_, nodes) in gen::NODE_CATEGORIES {
        types.extend(nodes.iter().copied());
    }
    // Real classes that carry no [Toolbox] attribute - hidden from the tool box, still loadable.
    types.extend(gen::UNCATEGORIZED_NODES.iter().copied());
    types
});

/// Nodes that own a Seed property.
pub static GENERATOR_NODES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| gen::SEEDED_NODES.iter().copied().collect());

/// Nodes in the `Output` category. These write files; see also [`is_output_node`].
pub static OUTPUT_NODES: &[&str] = gen::OUTPUT_NODES;

/// Check if a node type exists in the installed Gaea build.
pub fn is_valid_node_type(node_type: &str) -> bool {
    VALID_NODE_TYPES.contains(node_type)
}

/// Check if a node type is a generator (has a Seed property).
pub fn is_generator_node(node_type: &str) -> bool {
    GENERATOR_NODES.contains(node_type)
}

/// Check if a node type belongs to the `Output` category.
///
/// Note that a graph can export without one: any node can carry a `SaveDefinition`, which is how
/// most Gaea projects write their maps. Callers deciding whether a workflow produces output must
/// consider both.
pub fn is_output_node(node_type: &str) -> bool {
    gen::OUTPUT_NODES.contains(&node_type)
}

/// Get the tool box category for a node type, if it has one.
pub fn get_node_category(node_type: &str) -> Option<&'static str> {
    gen::NODE_CATEGORIES
        .iter()
        .find(|(_, nodes)| nodes.contains(&node_type))
        .map(|(category, _)| *category)
}

/// Get the port layout for a node type, in serialization order.
///
/// Order is significant: nodes read their inputs positionally, so an invented or reordered layout
/// can fault the build (`Thermal2` used to be emitted with a non-existent `Talus` port and threw
/// "Index was outside the bounds of the array"). Types absent from the extracted table fall back
/// to plain `In`/`Out` rather than a guess.
pub fn get_default_ports(node_type: &str) -> Vec<(&'static str, &'static str)> {
    gen::NODE_PORTS
        .iter()
        .find(|(name, _)| *name == node_type)
        .map(|(_, ports)| ports.to_vec())
        .unwrap_or_else(|| FALLBACK_PORTS.to_vec())
}

/// Whether the port layout of this node type is known, as opposed to falling back to `In`/`Out`.
pub fn has_known_ports(node_type: &str) -> bool {
    gen::NODE_PORTS.iter().any(|(name, _)| *name == node_type)
}

/// Modifiers Gaea attaches to a node by itself, as `(modifier type, order)`.
///
/// A node serialized without them can fault at build time.
pub fn get_intrinsic_modifiers(node_type: &str) -> &'static [(&'static str, i64)] {
    gen::INTRINSIC_MODIFIERS
        .iter()
        .find(|(name, _)| *name == node_type)
        .map(|(_, mods)| *mods)
        .unwrap_or(&[])
}

/// Properties declared by a node type.
pub fn get_node_properties(node_type: &str) -> &'static [NodeProperty] {
    gen::NODE_PROPERTIES
        .iter()
        .find(|(name, _)| *name == node_type)
        .map(|(_, props)| *props)
        .unwrap_or(&[])
}

/// Look up one property of a node type.
pub fn find_property(node_type: &str, property: &str) -> Option<&'static NodeProperty> {
    get_node_properties(node_type)
        .iter()
        .find(|p| p.name == property)
}

/// Check if a modifier type exists in the installed build.
///
/// A modifier is serialized with its own `$type`, so an invented name breaks the project the
/// same way an invented node type does.
pub fn is_valid_modifier_type(modifier_type: &str) -> bool {
    gen::MODIFIER_TYPES.contains(&modifier_type)
}

/// Every modifier type the installed build ships.
pub fn modifier_types() -> &'static [&'static str] {
    gen::MODIFIER_TYPES
}

/// Properties declared by a modifier type.
pub fn get_modifier_properties(modifier_type: &str) -> &'static [NodeProperty] {
    gen::MODIFIER_PROPERTIES
        .iter()
        .find(|(name, _)| *name == modifier_type)
        .map(|(_, props)| *props)
        .unwrap_or(&[])
}

/// Look up one property of a modifier type.
pub fn find_modifier_property(
    modifier_type: &str,
    property: &str,
) -> Option<&'static NodeProperty> {
    get_modifier_properties(modifier_type)
        .iter()
        .find(|p| p.name == property)
}

/// Whether this modifier works off the input of the node it is attached to.
///
/// Masks by height or slope and combiners read `Parent.In`. On a generator, which has no input,
/// they produce nothing - and the build reports nothing either.
pub fn modifier_uses_parent_input(modifier_type: &str) -> bool {
    gen::MODIFIERS_USING_PARENT_INPUT.contains(&modifier_type)
}

/// Find the modifier whose interface label matches, e.g. "Height Remap" -> `Multiplier`.
///
/// Gaea labels several modifier settings differently from their serialized names, and following
/// the label to the wrong type is how a graph ends up flat: the "Height Remap" of a tutorial is
/// `Multiplier.Value`, while the `Height` modifier is a mask over the node's input.
pub fn modifier_for_label(label: &str) -> Option<(&'static str, &'static NodeProperty)> {
    gen::MODIFIER_PROPERTIES.iter().find_map(|(name, props)| {
        props
            .iter()
            .find(|p| p.label.is_some_and(|l| l.eq_ignore_ascii_case(label)))
            .map(|p| (*name, p))
    })
}

/// Check one property of a modifier: that it exists, and that its value fits the declaration.
pub fn check_modifier_property(
    modifier_type: &str,
    property: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let declared = get_modifier_properties(modifier_type);
    let Some(property_decl) = declared.iter().find(|p| p.name == property) else {
        let known: Vec<&str> = declared.iter().map(|p| p.name).collect();
        let hint = match modifier_for_label(property) {
            Some((owner, decl)) if owner != modifier_type => format!(
                " '{property}' is the interface label of {owner}.{}; use that modifier instead.",
                decl.name
            ),
            _ => String::new(),
        };
        return Err(format!(
            "Modifier {modifier_type} has no property '{property}'. Declared: {}.{hint}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    };

    // Enumerated settings follow the same rule as node properties: member name, never an ordinal.
    let allowed = enum_values(property_decl.cs_type);
    if !allowed.is_empty() {
        return match value.as_str() {
            Some(text) if allowed.contains(&text) => Ok(()),
            _ => Err(format!(
                "{modifier_type}.{property} is a {} and must be one of: {}",
                property_decl.cs_type,
                allowed.join(", ")
            )),
        };
    }

    match property_decl.cs_type {
        "float" | "int" | "double" => {
            let Some(actual) = value.as_f64() else {
                return Err(format!(
                    "{modifier_type}.{property} is {} and must be a number, not {value}",
                    property_decl.cs_type
                ));
            };
            if let (Some(min), Some(max)) = (property_decl.min, property_decl.max) {
                if actual < min || actual > max {
                    return Err(format!(
                        "{modifier_type}.{property} is {actual}, outside its range {min}..{max}"
                    ));
                }
            }
            Ok(())
        },
        // A pair is written as an object with X and Y.
        "Float2" => {
            let Some(pair) = value.as_object() else {
                return Err(format!(
                    "{modifier_type}.{property} is a pair and must be written as an object with \
                     X and Y, not {value}"
                ));
            };
            for axis in ["X", "Y"] {
                let Some(component) = pair.get(axis) else {
                    continue;
                };
                let Some(actual) = component.as_f64() else {
                    return Err(format!(
                        "{modifier_type}.{property}.{axis} must be a number, not {component}"
                    ));
                };
                if let (Some(min), Some(max)) = (property_decl.min, property_decl.max) {
                    if actual < min || actual > max {
                        return Err(format!(
                            "{modifier_type}.{property}.{axis} is {actual}, outside {min}..{max}"
                        ));
                    }
                }
            }
            Ok(())
        },
        "bool" => match value.as_bool() {
            Some(_) => Ok(()),
            None => Err(format!(
                "{modifier_type}.{property} is a flag and must be true or false, not {value}"
            )),
        },
        _ => Ok(()),
    }
}

/// Members of an enumerated property type, empty when the type is not an enumeration.
pub fn enum_values(type_name: &str) -> &'static [&'static str] {
    gen::ENUM_VALUES
        .iter()
        .find(|(name, _)| *name == type_name)
        .map(|(_, values)| *values)
        .unwrap_or(&[])
}

/// Check a value about to be written for one property of a node.
///
/// Only enumerated properties are rejected outright, and for a reason worth the strictness:
/// Gaea stores them by member name, and an ordinal makes it fail to load the node. The build
/// then exits with code 1, no files and no crash log - indistinguishable from a broken graph.
/// `Rivers.RiverValleyWidth = 0` cost exactly that investigation; `"minus4"` builds fine.
pub fn check_property_value(
    node_type: &str,
    property: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let Some(declared) = find_property(node_type, property) else {
        return Ok(());
    };
    let allowed = enum_values(declared.cs_type);
    if allowed.is_empty() {
        return Ok(());
    }

    match value.as_str() {
        Some(text) if allowed.contains(&text) => Ok(()),
        Some(text) => Err(format!(
            "'{property}' of {node_type} is a {} and has no member '{text}'. Expected one of: {}",
            declared.cs_type,
            allowed.join(", ")
        )),
        None => Err(format!(
            "'{property}' of {node_type} is a {} and must be written as a member name, not as \
             {value}. Gaea would fail to load the node and the build would produce nothing. \
             Expected one of: {}",
            declared.cs_type,
            allowed.join(", ")
        )),
    }
}

/// Build resolutions Gaea computes at: powers of two from 256 to 8192.
///
/// Off-list values are not rejected by Gaea's command line - it starts the build, works through
/// the first node and exits without writing a single file, which reads like a broken graph. The
/// heightfield sizes offered by the `Unity` node (513, 1025, 2049, 4097) belong to that node's
/// `TargetSize`, not here, and 2049 as a build resolution is exactly how this was hit.
pub const BUILD_RESOLUTIONS: &[u32] = &[256, 512, 1024, 2048, 4096, 8192];

/// Whether Gaea can build at this resolution.
pub fn is_valid_build_resolution(resolution: u32) -> bool {
    BUILD_RESOLUTIONS.contains(&resolution)
}

/// Find a valid node type close to an invalid one, for repair suggestions.
///
/// Exact-name aliases come first: they map names the old 2.2.6 schema used to the type the
/// installed build actually ships.
pub fn find_similar_node_type(invalid_type: &str) -> Option<&'static str> {
    /// Retired names and the node that replaced them.
    const ALIASES: &[(&str, &str)] = &[
        ("Output", "Export"),
        ("QuickColor", "SatMap"),
        ("Satmaps", "SatMap"),
        ("Colorize", "SatMap"),
        ("Rockmap", "RockMap"),
        ("Sediment", "Sediments"),
        ("Mixer2", "Mixer"),
        ("Coast", "Sea"),
        ("Beach", "Sea"),
        ("Fluvial", "Rivers"),
        ("HeightMask", "Height"),
        ("SlopeMask", "Slope"),
        ("Math", "MathX"),
        ("Details", "GroundTexture"),
        ("Gradient", "LinearGradient"),
        ("Pattern", "Shape"),
    ];

    if let Some((_, replacement)) = ALIASES
        .iter()
        .find(|(old, _)| old.eq_ignore_ascii_case(invalid_type))
    {
        return Some(replacement);
    }

    // Case-only mistakes: "erosion2" -> "Erosion2".
    if let Some(exact) = VALID_NODE_TYPES
        .iter()
        .find(|t| t.eq_ignore_ascii_case(invalid_type))
    {
        return Some(exact);
    }

    None
}

/// Suggest nodes that would round out the current workflow.
pub fn suggest_nodes(current_nodes: &[String], context: Option<&str>) -> Vec<String> {
    let mut suggestions = Vec::new();

    let has_generator = current_nodes.iter().any(|n| is_generator_node(n));
    let has_erosion = current_nodes
        .iter()
        .any(|n| n == "Erosion2" || n == "Erosion");
    let has_colorize = current_nodes
        .iter()
        .any(|n| gen::COLORIZE_NODES.contains(&n.as_str()));
    let has_output = current_nodes.iter().any(|n| is_output_node(n));

    if !has_generator {
        suggestions.extend(["Mountain", "Perlin", "Voronoi"].map(String::from));
    }
    if has_generator && !has_erosion {
        suggestions.push("Erosion2".to_string());
    }
    if !has_colorize {
        suggestions.extend(["SatMap", "SuperColor"].map(String::from));
    }
    if !has_output {
        suggestions.push("Export".to_string());
    }

    if let Some(ctx) = context {
        let ctx_lower = ctx.to_lowercase();
        if ctx_lower.contains("mountain") || ctx_lower.contains("alpine") {
            suggestions.extend(["Snow", "Glacier", "Stones"].map(String::from));
        } else if ctx_lower.contains("desert") || ctx_lower.contains("dune") {
            suggestions.extend(["DuneSea", "Sandstone", "SlopeWarp"].map(String::from));
        } else if ctx_lower.contains("coast") || ctx_lower.contains("beach") {
            suggestions.extend(["Sea", "Lake", "Rivers"].map(String::from));
        } else if ctx_lower.contains("volcano") {
            suggestions.extend(["Volcano", "Thermal2", "Stratify"].map(String::from));
        } else if ctx_lower.contains("canyon") {
            suggestions.extend(["Canyon", "Stratify", "Rivers"].map(String::from));
        }
    }

    suggestions.retain(|s| !current_nodes.contains(s));
    suggestions.sort();
    suggestions.dedup();
    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_types_the_installed_build_ships() {
        assert!(is_valid_node_type("Mountain"));
        assert!(is_valid_node_type("Erosion2"));
        assert!(is_valid_node_type("Export"));
        assert!(is_valid_node_type("Unity"));
        // Present in 2.3, missing from the old hand-written schema.
        assert!(is_valid_node_type("Sediments"));
        assert!(is_valid_node_type("ColorThreshold"));
        assert!(!is_valid_node_type("InvalidNode"));
    }

    #[test]
    fn rejects_types_the_old_schema_invented() {
        // "Output" never existed; the export node is "Export".
        assert!(!is_valid_node_type("Output"));
        assert!(!is_valid_node_type("Bias"));
        assert!(!is_valid_node_type("QuickColor"));
        assert!(!is_valid_node_type("Rockmap"));
        assert!(!is_valid_node_type("PortalTransmit"));
    }

    #[test]
    fn maps_retired_names_onto_shipped_ones() {
        assert_eq!(find_similar_node_type("Output"), Some("Export"));
        assert_eq!(find_similar_node_type("Rockmap"), Some("RockMap"));
        assert_eq!(find_similar_node_type("Sediment"), Some("Sediments"));
        assert_eq!(find_similar_node_type("erosion2"), Some("Erosion2"));
        assert_eq!(find_similar_node_type("Nonsense"), None);
    }

    #[test]
    fn generator_nodes_come_from_the_seed_property() {
        assert!(is_generator_node("Mountain"));
        assert!(is_generator_node("Perlin"));
        assert!(!is_generator_node("Export"));
        assert!(!is_generator_node("Blur"));
    }

    #[test]
    fn categories_match_the_tool_box() {
        assert_eq!(get_node_category("Mountain"), Some("Terrain"));
        assert_eq!(get_node_category("Erosion2"), Some("Simulate"));
        assert_eq!(get_node_category("Unity"), Some("Output"));
        assert_eq!(get_node_category("Invalid"), None);
        assert!(is_output_node("Unity"));
        assert!(is_output_node("Export"));
        assert!(!is_output_node("Mountain"));
    }

    #[test]
    fn thermal2_carries_its_real_ports_and_intrinsic_modifier() {
        let ports = get_default_ports("Thermal2");
        let names: Vec<_> = ports.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "In",
                "Out",
                "AreaMask",
                "SedimentRemoval",
                "Wear",
                "Deposits"
            ]
        );
        // The invented port that used to fault the build.
        assert!(!names.contains(&"Talus"));
        assert_eq!(get_intrinsic_modifiers("Thermal2"), &[("Max", 66)]);
    }

    #[test]
    fn output_nodes_keep_their_out_port() {
        // Export-class nodes were previously stripped of "Out", which Gaea does emit.
        let ports = get_default_ports("Unity");
        assert!(ports.iter().any(|(n, _)| *n == "Out"));
    }

    #[test]
    fn enumerated_properties_must_be_written_by_name() {
        use serde_json::json;

        // The value that silently killed a build: an ordinal where a member name belongs.
        let err = check_property_value("Rivers", "RiverValleyWidth", &json!(0))
            .expect_err("an ordinal is not a member name");
        assert!(err.contains("minus4"), "should list the members: {err}");

        assert!(check_property_value("Rivers", "RiverValleyWidth", &json!("minus4")).is_ok());
        assert!(check_property_value("Rivers", "RiverValleyWidth", &json!("nonsense")).is_err());
        assert!(check_property_value("Mountain", "Style", &json!("Alpine")).is_ok());

        // Plain numeric and unknown properties are left alone here.
        assert!(check_property_value("Rivers", "Water", &json!(0.5)).is_ok());
        assert!(check_property_value("Rivers", "NoSuchProperty", &json!(1)).is_ok());
    }

    #[test]
    fn height_remap_resolves_to_the_multiplier() {
        // Following the interface label to the Height modifier is what flattened a whole graph.
        let (modifier, property) =
            modifier_for_label("Height Remap").expect("the label exists in this build");
        assert_eq!(modifier, "Multiplier");
        assert_eq!(property.name, "Value");
        assert_eq!(property.max, Some(4.0), "2.23 has to fit");
    }

    #[test]
    fn mask_modifiers_are_marked_as_reading_the_input() {
        assert!(modifier_uses_parent_input("Height"));
        assert!(modifier_uses_parent_input("Slope"));
        assert!(modifier_uses_parent_input("Max"));
        // These transform the node's own output and are fine on a generator.
        assert!(!modifier_uses_parent_input("Multiplier"));
        assert!(!modifier_uses_parent_input("Blur"));
        assert!(!modifier_uses_parent_input("Shaper"));
    }

    #[test]
    fn modifier_properties_are_checked_by_name_and_type() {
        use serde_json::json;

        assert!(check_modifier_property("Blur", "Factor", &json!(0.7)).is_ok());
        assert!(check_modifier_property("Multiplier", "Value", &json!({"Y": 2.23})).is_ok());
        assert!(check_modifier_property("Height", "Range", &json!({"X": 0, "Y": 0.5})).is_ok());

        // Out of the declared range: Height.Range is 0..1, so 2.23 does not belong there.
        assert!(check_modifier_property("Height", "Range", &json!({"X": 0, "Y": 2.23})).is_err());
        // A pair written as a bare number.
        assert!(check_modifier_property("Multiplier", "Value", &json!(2.23)).is_err());
        // A setting that belongs to another modifier, with a hint about which.
        let err = check_modifier_property("Blur", "Height Remap", &json!(1))
            .expect_err("Blur has no such setting");
        assert!(err.contains("Multiplier"), "{err}");
    }

    #[test]
    fn enum_members_come_from_the_installed_build() {
        assert_eq!(
            enum_values("RiverValleyWidths"),
            &["minus4", "minus2", "zero", "plus2", "plus4"]
        );
        assert!(enum_values("float").is_empty());
    }

    #[test]
    fn build_resolutions_exclude_the_unity_heightfield_sizes() {
        assert!(is_valid_build_resolution(512));
        assert!(is_valid_build_resolution(8192));
        // 2049 is a Unity TargetSize. As a build resolution Gaea writes no file at all.
        assert!(!is_valid_build_resolution(2049));
        assert!(!is_valid_build_resolution(1000));
        assert!(!is_valid_build_resolution(0));
    }

    #[test]
    fn properties_carry_ranges_where_gaea_declares_them() {
        let bias = find_property("SatMap", "Bias").expect("SatMap has Bias");
        assert_eq!(bias.min, Some(-1.0));
        assert_eq!(bias.max, Some(1.0));
        // Bias is a property of several nodes; it is not a node of its own.
        assert!(find_property("Soil", "Bias").is_some());
        assert!(!is_valid_node_type("Bias"));
    }
}
