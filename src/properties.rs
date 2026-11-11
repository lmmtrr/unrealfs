use crate::package::{soft_path_text, ObjectExport, Package};
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
    Bytes(Vec<u8>),
    Map(Vec<(PropValue, PropValue)>),
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
    value_type: String,
    value_struct: Option<String>,
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
            "MapProperty" => {
                tag.inner_type = ty.child().to_string();
                tag.inner_struct = ty.children.first().map(|c| c.child().to_string());
                tag.value_type = ty.children.get(1).map_or_else(String::new, |c| c.name.clone());
                tag.value_struct = ty.children.get(1).map(|c| c.child().to_string());
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
            tag.inner_type = pkg.read_name(r)?;
            tag.value_type = pkg.read_name(r)?;
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
    r.nested(|r| read_tagged_body(pkg, r))
}

fn read_tagged_body(pkg: &Package, r: &mut Reader) -> Result<Properties> {
    let mut props = Properties::new();
    while let Some(tag) = read_tag(pkg, r)? {
        let value_start = r.pos;
        let value = match tag.ty.as_str() {
            "BoolProperty" => PropValue::Bool(tag.bool_value),
            "ByteProperty" if tag.size == 1 && tag.enum_name == "None" => PropValue::Int(r.u8()? as i64),
            "StructProperty" => read_struct_value(pkg, r, &tag.struct_name).unwrap_or(PropValue::Other),
            "ArrayProperty" => read_array_value(pkg, r, &tag.inner_type, tag.inner_struct.as_deref()).unwrap_or(PropValue::Other),
            "SetProperty" => read_set_value(pkg, r, &tag.inner_type, tag.inner_struct.as_deref()).unwrap_or(PropValue::Other),
            "MapProperty" => read_map_value(pkg, r, &tag).unwrap_or(PropValue::Other),
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
        "BoolProperty" => PropValue::Bool(r.u8()? != 0),
        "Int8Property" => PropValue::Int(r.u8()? as i8 as i64),
        "Int16Property" => PropValue::Int(r.i16()? as i64),
        "UInt16Property" => PropValue::Int(r.u16()? as i64),
        "IntProperty" => PropValue::Int(r.i32()? as i64),
        "UInt32Property" => PropValue::Int(r.u32()? as i64),
        "Int64Property" | "UInt64Property" => PropValue::Int(r.i64()?),
        "FloatProperty" => PropValue::Float(r.f32()?),
        "DoubleProperty" => PropValue::Float(r.f64()? as f32),
        "ObjectProperty" | "WeakObjectProperty" | "InterfaceProperty" => PropValue::Object(r.i32()?),
        "SoftObjectProperty" | "SoftClassProperty" | "AssetObjectProperty" => PropValue::Name(read_soft_object_path(pkg, r)?),
        "NameProperty" => PropValue::Name(pkg.read_name(r)?),
        "StrProperty" => PropValue::Name(r.fstring()?),
        "TextProperty" => PropValue::Name(read_text(pkg, r)?),
        "ByteProperty" | "EnumProperty" if size == 8 => PropValue::Name(pkg.read_name(r)?),
        "ByteProperty" if size == 1 => PropValue::Int(r.u8()? as i64),
        _ => PropValue::Other,
    })
}

pub(crate) fn read_soft_object_path(pkg: &Package, r: &mut Reader) -> Result<String> {
    if pkg.soft_object_path_list {
        let index = r.i32()?;
        return usize::try_from(index)
            .ok()
            .and_then(|i| pkg.soft_object_paths.get(i).cloned())
            .ok_or_else(|| format!("soft object path {} out of range", index));
    }
    let package = pkg.read_name(r)?;
    let asset = if pkg.soft_paths_have_asset_names() { pkg.read_name(r)? } else { String::new() };
    Ok(soft_path_text(package, asset, r.fstring()?))
}

fn read_format_value(pkg: &Package, r: &mut Reader) -> Result<String> {
    Ok(match r.u8()? {
        0 => r.i64()?.to_string(),
        1 => (r.i64()? as u64).to_string(),
        2 => r.f32()?.to_string(),
        3 => r.f64()?.to_string(),
        4 => read_text(pkg, r)?,
        5 => r.u8()?.to_string(),
        kind => return Err(format!("unsupported FText argument type {}", kind)),
    })
}

fn read_number_text(pkg: &Package, r: &mut Reader) -> Result<String> {
    let value = read_format_value(pkg, r)?;
    if r.bool32()? {
        r.skip(FORMATTING_OPTIONS_SIZE)?;
    }
    r.fstring()?;
    Ok(value)
}

pub(crate) fn read_text(pkg: &Package, r: &mut Reader) -> Result<String> {
    r.nested(|r| {
        r.u32()?;
        let history = r.u8()? as i8;
        match history {
            -1 => Ok(if r.bool32()? { r.fstring()? } else { String::new() }),
            0 => {
                r.fstring()?;
                r.fstring()?;
                r.fstring()
            }
            1 => {
                let format = read_text(pkg, r)?;
                for _ in 0..r.count(5)? {
                    r.fstring()?;
                    read_format_value(pkg, r)?;
                }
                Ok(format)
            }
            2 => {
                let format = read_text(pkg, r)?;
                for _ in 0..r.count(1)? {
                    read_format_value(pkg, r)?;
                }
                Ok(format)
            }
            3 => {
                let format = read_text(pkg, r)?;
                for _ in 0..r.count(5)? {
                    r.fstring()?;
                    read_format_value(pkg, r)?;
                }
                Ok(format)
            }
            4 | 5 => read_number_text(pkg, r),
            6 => {
                r.fstring()?;
                read_number_text(pkg, r)
            }
            10 => {
                let text = read_text(pkg, r)?;
                r.u8()?;
                Ok(text)
            }
            11 => {
                pkg.read_name(r)?;
                r.fstring()
            }
            _ => Err(format!("unsupported FText history type {}", history)),
        }
    })
}

const FORMATTING_OPTIONS_SIZE: usize = 25;

fn read_real(pkg: &Package, r: &mut Reader) -> Result<f32> {
    if pkg.large_world_coordinates() {
        Ok(r.f64()? as f32)
    } else {
        r.f32()
    }
}

fn read_struct_value(pkg: &Package, r: &mut Reader, struct_name: &str) -> Result<PropValue> {
    Ok(match struct_name {
        "Vector" | "Rotator" => PropValue::Vector([read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?]),
        "Vector3f" | "Rotator3f" => PropValue::Vector([r.f32()?, r.f32()?, r.f32()?]),
        "Vector4" | "Quat" | "Plane" => {
            PropValue::Color([read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?, read_real(pkg, r)?])
        }
        "LinearColor" | "Vector4f" | "Quat4f" | "Plane4f" => PropValue::Color([r.f32()?, r.f32()?, r.f32()?, r.f32()?]),
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
    if inner_type == "ByteProperty" {
        return Ok(PropValue::Bytes(r.bytes(count)?.to_vec()));
    }
    let mut items = Vec::with_capacity(count);
    if inner_type == "StructProperty" {
        let struct_name = match inner_struct {
            Some(name) if pkg.complete_type_names() => name.to_string(),
            _ => read_tag(pkg, r)?.ok_or("array of structs has no inner tag")?.struct_name,
        };
        for _ in 0..count {
            items.push(read_struct_value(pkg, r, &struct_name)?);
        }
    } else {
        for _ in 0..count {
            items.push(read_element(pkg, r, inner_type, None)?);
        }
    }
    Ok(PropValue::Array(items))
}

fn read_element(pkg: &Package, r: &mut Reader, ty: &str, struct_name: Option<&str>) -> Result<PropValue> {
    if ty == "StructProperty" {
        return read_struct_value(pkg, r, struct_name.unwrap_or_default());
    }
    let size = match ty {
        "ByteProperty" => 1,
        "NameProperty" | "EnumProperty" => 8,
        _ => 4,
    };
    read_simple_value(pkg, r, ty, size)
}

fn read_known_element(pkg: &Package, r: &mut Reader, ty: &str, struct_name: Option<&str>) -> Result<PropValue> {
    match read_element(pkg, r, ty, struct_name)? {
        PropValue::Other => Err(format!("unsupported container element {}", ty)),
        value => Ok(value),
    }
}

fn read_set_value(pkg: &Package, r: &mut Reader, inner_type: &str, inner_struct: Option<&str>) -> Result<PropValue> {
    for _ in 0..r.count(1)? {
        read_known_element(pkg, r, inner_type, inner_struct)?;
    }
    let count = r.count(1)?;
    (0..count).map(|_| read_known_element(pkg, r, inner_type, inner_struct)).collect::<Result<_>>().map(PropValue::Array)
}

fn read_map_value(pkg: &Package, r: &mut Reader, tag: &Tag) -> Result<PropValue> {
    let key_struct = tag.inner_struct.as_deref();
    for _ in 0..r.count(1)? {
        read_known_element(pkg, r, &tag.inner_type, key_struct)?;
    }
    let count = r.count(1)?;
    (0..count)
        .map(|_| {
            let key = read_known_element(pkg, r, &tag.inner_type, key_struct)?;
            Ok((key, read_known_element(pkg, r, &tag.value_type, tag.value_struct.as_deref())?))
        })
        .collect::<Result<_>>()
        .map(PropValue::Map)
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
