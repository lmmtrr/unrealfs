use crate::package::Package;
use crate::properties::{read_object_header, PropValue, Properties};
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct TextureParam {
    pub name: String,
    pub texture: String,
}

#[derive(Serialize, Clone)]
pub struct ScalarParam {
    pub name: String,
    pub value: f32,
}

#[derive(Serialize, Clone)]
pub struct VectorParam {
    pub name: String,
    pub value: [f32; 4],
}

#[derive(Serialize, Default, Clone)]
pub struct MaterialInfo {
    pub name: String,
    pub parents: Vec<String>,
    pub textures: Vec<TextureParam>,
    pub scalars: Vec<ScalarParam>,
    pub vectors: Vec<VectorParam>,
    pub blend_mode: Option<String>,
    pub two_sided: Option<bool>,
    pub shading_model: Option<String>,
    pub opacity_mask_clip_value: Option<f32>,
}

fn param_name(entry: &Properties) -> Option<String> {
    match entry.get("ParameterInfo") {
        Some(PropValue::Struct(info)) => match info.get("Name") {
            Some(PropValue::Name(n)) => Some(n.clone()),
            _ => None,
        },
        _ => match entry.get("ParameterName") {
            Some(PropValue::Name(n)) => Some(n.clone()),
            _ => None,
        },
    }
}

fn entries<'a>(props: &'a Properties, key: &str) -> impl Iterator<Item = (String, &'a PropValue)> {
    let items: &[PropValue] = match props.get(key) {
        Some(PropValue::Array(items)) => items,
        _ => &[],
    };
    items.iter().filter_map(|item| match item {
        PropValue::Struct(entry) => Some((param_name(entry)?, entry.get("ParameterValue")?)),
        _ => None,
    })
}

fn merge_instance(info: &mut MaterialInfo, pkg: &Package, props: &Properties) {
    for (name, value) in entries(props, "TextureParameterValues") {
        if let PropValue::Object(index) = value {
            let Some(path) = pkg.import_path(*index) else { continue };
            if !info.textures.iter().any(|t| t.name == name) {
                let texture = path.split('.').next().unwrap_or(&path).to_string();
                info.textures.push(TextureParam { name, texture });
            }
        }
    }
    for (name, value) in entries(props, "ScalarParameterValues") {
        if let PropValue::Float(v) = value {
            if !info.scalars.iter().any(|s| s.name == name) {
                info.scalars.push(ScalarParam { name, value: *v });
            }
        }
    }
    for (name, value) in entries(props, "VectorParameterValues") {
        if let PropValue::Color(v) = value {
            if !info.vectors.iter().any(|s| s.name == name) {
                info.vectors.push(VectorParam { name, value: *v });
            }
        }
    }
    if let Some(PropValue::Struct(overrides)) = props.get("BasePropertyOverrides") {
        let enabled = |field: &str| matches!(overrides.get(&format!("bOverride_{}", field)), Some(PropValue::Bool(true)));
        if info.blend_mode.is_none() && enabled("BlendMode") {
            if let Some(PropValue::Name(v)) = overrides.get("BlendMode") {
                info.blend_mode = Some(v.rsplit("::").next().unwrap_or(v).to_string());
            }
        }
        if info.shading_model.is_none() && enabled("ShadingModel") {
            if let Some(PropValue::Name(v)) = overrides.get("ShadingModel") {
                info.shading_model = Some(v.rsplit("::").next().unwrap_or(v).to_string());
            }
        }
        if info.two_sided.is_none() && enabled("TwoSided") {
            info.two_sided = Some(matches!(overrides.get("TwoSided"), Some(PropValue::Bool(true))));
        }
        if info.opacity_mask_clip_value.is_none() && enabled("OpacityMaskClipValue") {
            if let Some(PropValue::Float(v)) = overrides.get("OpacityMaskClipValue") {
                info.opacity_mask_clip_value = Some(*v);
            }
        }
    }
}

pub fn read_material<'a>(pkg: &'a Package, find: impl Fn(&str) -> Option<&'a Package>) -> Option<MaterialInfo> {
    let mut info = MaterialInfo::default();
    let mut current = pkg;
    for depth in 0..8 {
        let export = current.exports.iter().find(|e| {
            matches!(current.export_class_name(e), Some(c) if c.starts_with("MaterialInstance"))
        });
        let Some(export) = export else {
            if depth == 0 {
                return None;
            }
            break;
        };
        if depth == 0 {
            info.name = export.object_name.clone();
        }
        let data = current.export_data(export).ok()?;
        let props = read_object_header(current, export, &mut crate::reader::Reader::new(data)).ok()?;
        merge_instance(&mut info, current, &props);
        let parent = match props.get("Parent") {
            Some(PropValue::Object(i)) => current.import_path(*i),
            _ => None,
        };
        let Some(parent) = parent else { break };
        let parent_path = parent.split('.').next().unwrap_or(&parent).to_string();
        info.parents.push(parent_path.rsplit('/').next().unwrap_or(&parent_path).to_string());
        match find(&parent_path) {
            Some(next) => current = next,
            None => break,
        }
    }
    Some(info)
}
