use crate::package::{ObjectExport, Package};
use crate::unversioned::read_unversioned_properties;
use crate::reader::{Reader, Result};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum PropValue {
    Bool(bool),
    Int(i64),
    Float(f32),
    Name(String),
    Object(i32),
    Vector([f32; 3]),
    Color([f32; 4]),
    Struct(Properties),
    Array(Vec<PropValue>),
    Other,
}

pub type Properties = HashMap<String, PropValue>;

struct TypeNode {
    name: String,
    children: Vec<TypeNode>,
}

impl TypeNode {
    fn read(pkg: &Package, r: &mut Reader, depth: usize) -> Result<TypeNode> {
        if depth > 16 {
            return Err("property type name is nested too deeply".into());
        }
        let name = pkg.read_name(r)?;
        let count = r.count(8)?;
        let children = (0..count).map(|_| TypeNode::read(pkg, r, depth + 1)).collect::<Result<_>>()?;
        Ok(TypeNode { name, children })
    }

    fn child(&self) -> &str {
        self.children.first().map_or("None", |c| c.name.as_str())
    }
}

#[derive(Default)]
struct Tag {
    name: String,
    ty: String,
    size: usize,
    array_index: i32,
    bool_value: bool,
    enum_name: String,
    struct_name: String,
    inner_type: String,
    inner_struct: Option<String>,
}

const TAG_HAS_ARRAY_INDEX: u8 = 0x01;
const TAG_HAS_PROPERTY_GUID: u8 = 0x02;
const TAG_HAS_PROPERTY_EXTENSIONS: u8 = 0x04;
const TAG_BOOL_TRUE: u8 = 0x10;
const EXTENSION_OVERRIDABLE_INFORMATION: u8 = 0x02;

fn read_tag_extensions(r: &mut Reader) -> Result<()> {
    if r.u8()? & EXTENSION_OVERRIDABLE_INFORMATION != 0 {
        r.u8()?;
        r.bool32()?;
    }
    Ok(())
}

fn read_tag(pkg: &Package, r: &mut Reader) -> Result<Option<Tag>> {
    let name = pkg.read_name(r)?;
    if name == "None" {
        return Ok(None);
    }
    let mut tag = Tag { name, ..Tag::default() };
    if pkg.complete_type_names() {
        let ty = TypeNode::read(pkg, r, 0)?;
        tag.size = r.count(0)?;
        let flags = r.u8()?;
        if flags & TAG_HAS_ARRAY_INDEX != 0 {
            tag.array_index = r.i32()?;
        }
        if flags & TAG_HAS_PROPERTY_GUID != 0 {
            r.skip(16)?;
        }
        if flags & TAG_HAS_PROPERTY_EXTENSIONS != 0 {
            read_tag_extensions(r)?;
        }
        tag.bool_value = flags & TAG_BOOL_TRUE != 0;
        match ty.name.as_str() {
            "StructProperty" => tag.struct_name = ty.child().to_string(),
            "ByteProperty" => tag.enum_name = ty.child().to_string(),
            "EnumProperty" => tag.enum_name = ty.child().to_string(),
            "ArrayProperty" | "SetProperty" | "OptionalProperty" => {
                tag.inner_type = ty.child().to_string();
                tag.inner_struct = ty.children.first().map(|c| c.child().to_string());
            }
            _ => {}
        }
        tag.ty = ty.name;
        return Ok(Some(tag));
    }
    tag.ty = pkg.read_name(r)?;
    tag.size = r.count(0)?;
    tag.array_index = r.i32()?;
    match tag.ty.as_str() {
        "StructProperty" => {
            tag.struct_name = pkg.read_name(r)?;
            r.skip(16)?;
        }
        "BoolProperty" => tag.bool_value = r.u8()? != 0,
        "ByteProperty" | "EnumProperty" => tag.enum_name = pkg.read_name(r)?,
        "ArrayProperty" | "SetProperty" | "OptionalProperty" => tag.inner_type = pkg.read_name(r)?,
        "MapProperty" => {
            pkg.read_name(r)?;
            pkg.read_name(r)?;
        }
        _ => {}
    }
    if r.u8()? != 0 {
        r.skip(16)?;
    }
    if pkg.property_tag_extensions() {
        read_tag_extensions(r)?;
    }
    Ok(Some(tag))
}

pub fn read_tagged_properties(pkg: &Package, r: &mut Reader) -> Result<Properties> {
    let mut props = Properties::new();
    while let Some(tag) = read_tag(pkg, r)? {
        let value_start = r.pos;
        let value = match tag.ty.as_str() {
            "BoolProperty" => PropValue::Bool(tag.bool_value),
            "ByteProperty" if tag.size == 1 && tag.enum_name == "None" => PropValue::Int(r.u8()? as i64),
            "StructProperty" => read_struct_value(pkg, r, &tag.struct_name).unwrap_or(PropValue::Other),
            "ArrayProperty" => read_array_value(pkg, r, &tag.inner_type, tag.inner_struct.as_deref()).unwrap_or(PropValue::Other),
            _ => read_simple_value(pkg, r, &tag.ty, tag.size).unwrap_or(PropValue::Other),
        };
        r.pos = value_start;
        r.skip(tag.size)?;
        let key = if tag.array_index > 0 { format!("{}[{}]", tag.name, tag.array_index) } else { tag.name };
        props.insert(key, value);
    }
    Ok(props)
}

fn read_simple_value(pkg: &Package, r: &mut Reader, ty: &str, size: usize) -> Result<PropValue> {
    Ok(match ty {
        "IntProperty" => PropValue::Int(r.i32()? as i64),
        "UInt32Property" => PropValue::Int(r.u32()? as i64),
        "FloatProperty" => PropValue::Float(r.f32()?),
        "ObjectProperty" | "SoftObjectProperty" if size == 4 => PropValue::Object(r.i32()?),
        "ObjectProperty" => PropValue::Object(r.i32()?),
        "SoftObjectProperty" | "SoftClassProperty" => PropValue::Name(read_soft_object_path(pkg, r)?),
        "NameProperty" => PropValue::Name(pkg.read_name(r)?),
        "StrProperty" => PropValue::Name(r.fstring()?),
        "ByteProperty" | "EnumProperty" if size == 8 => PropValue::Name(pkg.read_name(r)?),
        "ByteProperty" if size == 1 => PropValue::Int(r.u8()? as i64),
        _ => PropValue::Other,
    })
}

fn read_soft_object_path(pkg: &Package, r: &mut Reader) -> Result<String> {
    let mut path = pkg.read_name(r)?;
    if pkg.is_ue5 && pkg.ue5_version >= UE5_SOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES {
        let asset = pkg.read_name(r)?;
        if asset != "None" {
            path = format!("{}.{}", path, asset);
        }
    }
    let sub = r.fstring()?;
    if !sub.is_empty() {
        path = format!("{}:{}", path, sub);
    }
    Ok(path)
}

const UE5_SOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES: i32 = 1007;

fn read_real(pkg: &Package, r: &mut Reader) -> Result<f32> {
    if pkg.large_world_coordinates() {
        Ok(f64::from_le_bytes(r.bytes(8)?.try_into().unwrap()) as f32)
    } else {
        r.f32()
    }
}

fn read_struct_value(pkg: &Package, r: &mut Reader, struct_name: &str) -> Result<PropValue> {
    Ok(match struct_name {
        "Vector" | "Rotator" => PropValue::Vector([read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?]),
        "Vector4" | "Quat" => {
            PropValue::Color([read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?])
        }
        "LinearColor" => PropValue::Color([r.f32()?, r.f32()?, r.f32()?, r.f32()?]),
        "Color" => {
            let b = r.bytes(4)?;
            PropValue::Color([b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0, b[3] as f32 / 255.0])
        }
        "SoftObjectPath" | "SoftClassPath" => PropValue::Name(read_soft_object_path(pkg, r)?),
        "Guid" | "IntPoint" | "Vector2D" | "Box" | "Box2D" | "PerPlatformFloat"
        | "PerPlatformInt" | "FrameNumber" | "Timespan" | "DateTime" => PropValue::Other,
        _ => PropValue::Struct(read_tagged_properties(pkg, r)?),
    })
}

fn read_array_value(pkg: &Package, r: &mut Reader, inner_type: &str, inner_struct: Option<&str>) -> Result<PropValue> {
    let count = r.count(1)?;
    let mut items = Vec::with_capacity(count);
    if inner_type == "StructProperty" {
        let struct_name = match inner_struct {
            Some(name) if pkg.complete_type_names() => name.to_string(),
            _ => {
                pkg.read_name(r)?;
                pkg.read_name(r)?;
                r.i32()?;
                r.i32()?;
                let struct_name = pkg.read_name(r)?;
                r.skip(16)?;
                if r.u8()? != 0 {
                    r.skip(16)?;
                }
                struct_name
            }
        };
        for _ in 0..count {
            items.push(read_struct_value(pkg, r, &struct_name)?);
        }
    } else {
        let size = match inner_type {
            "ByteProperty" => 1,
            "NameProperty" => 8,
            _ => 4,
        };
        for _ in 0..count {
            items.push(read_simple_value(pkg, r, inner_type, size)?);
        }
    }
    Ok(PropValue::Array(items))
}

pub fn read_object_header(pkg: &Package, export: &ObjectExport, r: &mut Reader) -> Result<Properties> {
    let props = if pkg.unversioned_properties {
        let mappings = pkg
            .mappings
            .as_ref()
            .ok_or("this package uses unversioned properties; drop the game's .usmap mappings file together with it")?;
        let class = pkg.export_class_name(export).ok_or("export has no class")?;
        read_unversioned_properties(pkg, mappings, r, class)?
    } else {
        if pkg.property_tag_extensions() && r.u8()? & EXTENSION_OVERRIDABLE_INFORMATION != 0 {
            r.u8()?;
        }
        read_tagged_properties(pkg, r)?
    };
    if r.bool32()? {
        r.skip(16)?;
    }
    Ok(props)
}

pub fn prop_bool(props: &Properties, name: &str) -> bool {
    matches!(props.get(name), Some(PropValue::Bool(true)))
}

pub fn prop_name(props: &Properties, name: &str) -> Option<String> {
    match props.get(name) {
        Some(PropValue::Name(n)) => Some(n.clone()),
        _ => None,
    }
}

pub fn prop_vector(props: &Properties, name: &str) -> Option<[f32; 3]> {
    match props.get(name) {
        Some(PropValue::Vector(v)) => Some(*v),
        _ => None,
    }
}

pub fn prop_object(props: &Properties, name: &str) -> i32 {
    match props.get(name) {
        Some(PropValue::Object(i)) => *i,
        _ => 0,
    }
}
