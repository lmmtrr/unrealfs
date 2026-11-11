use crate::package::Package;
use crate::properties::{read_soft_object_path, read_text, PropValue, Properties};
use crate::reader::{Reader, Result};
use crate::usmap::{Mappings, PropType};

struct Fragment {
    skip: usize,
    values: usize,
    has_zeroes: bool,
}

fn read_header(r: &mut Reader) -> Result<(Vec<Fragment>, Vec<bool>)> {
    let mut fragments = Vec::new();
    loop {
        let packed = r.u16()?;
        fragments.push(Fragment {
            skip: (packed & 0x7f) as usize,
            has_zeroes: packed & 0x80 != 0,
            values: (packed >> 9) as usize,
        });
        if packed & 0x100 != 0 {
            break;
        }
    }
    let bits: usize = fragments.iter().filter(|f| f.has_zeroes).map(|f| f.values).sum();
    let words: Vec<u32> = match bits {
        0 => Vec::new(),
        1..=8 => vec![r.u8()? as u32],
        9..=16 => vec![r.u16()? as u32],
        _ => (0..bits.div_ceil(32)).map(|_| r.u32()).collect::<Result<_>>()?,
    };
    let zeroes = (0..bits).map(|i| (words[i / 32] >> (i % 32)) & 1 != 0).collect();
    Ok((fragments, zeroes))
}

pub fn read_unversioned_properties(pkg: &Package, mappings: &Mappings, r: &mut Reader, struct_name: &str) -> Result<Properties> {
    r.nested(|r| read_unversioned_body(pkg, mappings, r, struct_name))
}

fn read_unversioned_body(pkg: &Package, mappings: &Mappings, r: &mut Reader, struct_name: &str) -> Result<Properties> {
    if mappings.find(struct_name).is_none() {
        return Err(format!("{} is not in the .usmap mappings", struct_name));
    }
    let (fragments, zeroes) = read_header(r)?;
    let mut zeroes = zeroes.into_iter();
    let mut props = Properties::new();
    let mut index = 0;
    for fragment in fragments {
        index += fragment.skip;
        for _ in 0..fragment.values {
            let is_zero = fragment.has_zeroes && zeroes.next().unwrap_or(false);
            let (schema, element) = mappings
                .property_at(struct_name, index)
                .ok_or_else(|| format!("{} has no property #{} in the .usmap mappings", struct_name, index))?;
            let key = if element > 0 { format!("{}[{}]", schema.name, element) } else { schema.name.clone() };
            if is_zero {
                props.insert(key, zero_value(mappings, &schema.ty));
            } else {
                let value = read_value(pkg, mappings, r, &schema.ty)
                    .map_err(|e| format!("{}.{}: {}", struct_name, schema.name, e))?;
                props.insert(key, value);
            }
            index += 1;
        }
    }
    Ok(props)
}

fn zero_value(mappings: &Mappings, ty: &PropType) -> PropValue {
    match ty {
        PropType::Bool => PropValue::Bool(false),
        PropType::Byte
        | PropType::Int8
        | PropType::Int16
        | PropType::UInt16
        | PropType::Int
        | PropType::UInt32
        | PropType::Int64
        | PropType::UInt64 => PropValue::Int(0),
        PropType::Float | PropType::Double => PropValue::Float(0.0),
        PropType::Object | PropType::Interface | PropType::WeakObject => PropValue::Object(0),
        PropType::Name => PropValue::Name("None".into()),
        PropType::Str | PropType::Utf8Str | PropType::AnsiStr | PropType::Text => PropValue::Name(String::new()),
        PropType::Array(_) | PropType::Set(_) => PropValue::Array(Vec::new()),
        PropType::Enum(_, name) => match mappings.enum_name(name, 0) {
            Some(entry) => PropValue::Name(entry.to_string()),
            None => PropValue::Int(0),
        },
        PropType::Struct(name) => match name.as_str() {
            "Vector" | "Rotator" | "Vector3f" | "Rotator3f" => PropValue::Vector([0.0; 3]),
            "LinearColor" | "Color" | "Vector4" | "Vector4f" | "Quat" | "Quat4f" | "Plane" | "Plane4f" => {
                PropValue::Color([0.0; 4])
            }
            _ => PropValue::Struct(Properties::new()),
        },
        _ => PropValue::Other,
    }
}

fn read_int(r: &mut Reader, ty: &PropType) -> Result<i64> {
    Ok(match ty {
        PropType::Byte => r.u8()? as i64,
        PropType::Int8 => r.u8()? as i8 as i64,
        PropType::Int16 => r.i16()? as i64,
        PropType::UInt16 => r.u16()? as i64,
        PropType::Int => r.i32()? as i64,
        PropType::UInt32 => r.u32()? as i64,
        PropType::Int64 | PropType::UInt64 => r.i64()?,
        other => return Err(format!("unexpected enum storage {:?}", other)),
    })
}

fn read_reals(pkg: &Package, r: &mut Reader, n: usize, double: bool) -> Result<Vec<f32>> {
    (0..n)
        .map(|_| if double && pkg.is_ue5 { Ok(f64::from_le_bytes(r.bytes(8)?.try_into().unwrap()) as f32) } else { r.f32() })
        .collect()
}

fn read_native_struct(pkg: &Package, r: &mut Reader, name: &str) -> Result<Option<PropValue>> {
    let vec3 = |r: &mut Reader, double| -> Result<PropValue> {
        let v = read_reals(pkg, r, 3, double)?;
        Ok(PropValue::Vector([v[0], v[1], v[2]]))
    };
    let vec4 = |r: &mut Reader, double| -> Result<PropValue> {
        let v = read_reals(pkg, r, 4, double)?;
        Ok(PropValue::Color([v[0], v[1], v[2], v[3]]))
    };
    let skip = |r: &mut Reader, n| -> Result<PropValue> {
        r.skip(n)?;
        Ok(PropValue::Other)
    };
    Ok(Some(match name {
        "Vector" | "Rotator" => vec3(r, true)?,
        "Vector3f" | "Rotator3f" => vec3(r, false)?,
        "Vector4" | "Quat" | "Plane" => vec4(r, true)?,
        "Vector4f" | "Quat4f" | "Plane4f" | "LinearColor" => vec4(r, false)?,
        "Vector2D" => {
            read_reals(pkg, r, 2, true)?;
            PropValue::Other
        }
        "Box" => {
            read_reals(pkg, r, 6, true)?;
            skip(r, 1)?
        }
        "Box2D" => {
            read_reals(pkg, r, 4, true)?;
            skip(r, 1)?
        }
        "Matrix" => {
            read_reals(pkg, r, 16, true)?;
            PropValue::Other
        }
        "Color" => {
            let b = r.bytes(4)?;
            PropValue::Color([b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0, b[3] as f32 / 255.0])
        }
        "Vector2f" | "IntPoint" | "DateTime" | "Timespan" => skip(r, 8)?,
        "IntVector" => skip(r, 12)?,
        "Guid" | "IntVector4" => skip(r, 16)?,
        "Box2f" => skip(r, 17)?,
        "Box3f" => skip(r, 25)?,
        "FrameNumber" | "NavAgentSelector" => skip(r, 4)?,
        "RichCurveKey" => skip(r, 27)?,
        "SimpleCurveKey" => skip(r, 8)?,
        "SmartName" => PropValue::Name(pkg.read_name(r)?),
        "SoftObjectPath" | "SoftClassPath" => PropValue::Name(read_soft_object_path(pkg, r)?),
        "PerPlatformFloat" => {
            r.bool32()?;
            PropValue::Float(r.f32()?)
        }
        "PerPlatformInt" => {
            r.bool32()?;
            PropValue::Int(r.i32()? as i64)
        }
        "PerPlatformBool" => {
            r.bool32()?;
            PropValue::Bool(r.bool32()?)
        }
        "PerQualityLevelInt" | "PerQualityLevelFloat" => {
            r.bool32()?;
            let value = r.bytes(4)?;
            let overrides = r.count(8)?;
            r.skip(overrides * 8)?;
            if name == "PerQualityLevelInt" {
                PropValue::Int(i32::from_le_bytes(value.try_into().unwrap()) as i64)
            } else {
                PropValue::Float(f32::from_le_bytes(value.try_into().unwrap()))
            }
        }
        "SkeletalMeshSamplingLODBuiltData" => {
            let probabilities = r.count(4)?;
            r.skip(probabilities * 4)?;
            let aliases = r.count(4)?;
            r.skip(aliases * 4 + 4)?;
            PropValue::Other
        }
        "GameplayTagContainer" => {
            let n = r.count(8)?;
            r.skip(n * 8)?;
            PropValue::Other
        }
        _ => return Ok(None),
    }))
}

fn read_value(pkg: &Package, mappings: &Mappings, r: &mut Reader, ty: &PropType) -> Result<PropValue> {
    Ok(match ty {
        PropType::Bool => PropValue::Bool(r.u8()? != 0),
        PropType::Byte
        | PropType::Int8
        | PropType::Int16
        | PropType::UInt16
        | PropType::Int
        | PropType::UInt32
        | PropType::Int64
        | PropType::UInt64 => PropValue::Int(read_int(r, ty)?),
        PropType::Float => PropValue::Float(r.f32()?),
        PropType::Double => PropValue::Float(f64::from_le_bytes(r.bytes(8)?.try_into().unwrap()) as f32),
        PropType::Object | PropType::Interface | PropType::WeakObject => PropValue::Object(r.i32()?),
        PropType::LazyObject => {
            r.skip(16)?;
            PropValue::Other
        }
        PropType::SoftObject => PropValue::Name(read_soft_object_path(pkg, r)?),
        PropType::Name => PropValue::Name(pkg.read_name(r)?),
        PropType::Str => PropValue::Name(r.fstring()?),
        PropType::Utf8Str | PropType::AnsiStr => {
            let n = r.count(1)?;
            PropValue::Name(String::from_utf8_lossy(r.bytes(n)?).into_owned())
        }
        PropType::Text => PropValue::Name(read_text(pkg, r)?),
        PropType::Delegate => {
            r.i32()?;
            pkg.read_name(r)?;
            PropValue::Other
        }
        PropType::MulticastDelegate => {
            let n = r.count(12)?;
            r.skip(n * 12)?;
            PropValue::Other
        }
        PropType::FieldPath => {
            let n = r.count(8)?;
            r.skip(n * 8 + 4)?;
            PropValue::Other
        }
        PropType::Array(inner) if matches!(**inner, PropType::Byte) => {
            let n = r.count(1)?;
            PropValue::Bytes(r.bytes(n)?.to_vec())
        }
        PropType::Array(inner) => {
            let n = r.count(1)?;
            PropValue::Array((0..n).map(|_| read_value(pkg, mappings, r, inner)).collect::<Result<_>>()?)
        }
        PropType::Set(inner) => {
            for _ in 0..r.count(1)? {
                read_value(pkg, mappings, r, inner)?;
            }
            let n = r.count(1)?;
            PropValue::Array((0..n).map(|_| read_value(pkg, mappings, r, inner)).collect::<Result<_>>()?)
        }
        PropType::Map(key, value) => {
            for _ in 0..r.count(1)? {
                read_value(pkg, mappings, r, key)?;
            }
            let n = r.count(1)?;
            let entries = (0..n)
                .map(|_| {
                    let k = read_value(pkg, mappings, r, key)?;
                    Ok((k, read_value(pkg, mappings, r, value)?))
                })
                .collect::<Result<_>>()?;
            PropValue::Map(entries)
        }
        PropType::Optional(inner) => {
            if r.bool32()? {
                read_value(pkg, mappings, r, inner)?
            } else {
                PropValue::Other
            }
        }
        PropType::Struct(name) => match read_native_struct(pkg, r, name)? {
            Some(value) => value,
            None => PropValue::Struct(read_unversioned_properties(pkg, mappings, r, name)?),
        },
        PropType::Enum(inner, enum_name) => {
            let value = read_int(r, inner)?;
            match mappings.enum_name(enum_name, value) {
                Some(name) => PropValue::Name(name.to_string()),
                None => PropValue::Int(value),
            }
        }
        PropType::Unknown => return Err("property type is unknown in the .usmap mappings".into()),
    })
}
