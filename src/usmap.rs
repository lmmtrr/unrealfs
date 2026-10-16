use crate::reader::{Reader, Result};
use std::collections::HashMap;
use std::io::Read;

const MAGIC: u16 = 0x30C4;
const VERSION_PACKAGE_VERSIONING: u8 = 1;
const VERSION_LONG_FNAME: u8 = 2;
const VERSION_LARGE_ENUMS: u8 = 3;
const VERSION_EXPLICIT_ENUM_VALUES: u8 = 4;

#[derive(Debug, Clone)]
pub enum PropType {
    Byte,
    Bool,
    Int8,
    Int16,
    Int,
    Int64,
    UInt16,
    UInt32,
    UInt64,
    Float,
    Double,
    Object,
    Name,
    Delegate,
    MulticastDelegate,
    Array(Box<PropType>),
    Set(Box<PropType>),
    Map(Box<PropType>, Box<PropType>),
    Optional(Box<PropType>),
    Struct(String),
    Enum(Box<PropType>, String),
    Str,
    Utf8Str,
    AnsiStr,
    Text,
    Interface,
    WeakObject,
    LazyObject,
    SoftObject,
    FieldPath,
    Unknown,
}

#[derive(Debug)]
pub struct PropSchema {
    pub name: String,
    pub schema_index: u16,
    pub array_size: u8,
    pub ty: PropType,
}

#[derive(Debug)]
pub struct StructSchema {
    pub super_name: Option<String>,
    pub prop_count: u16,
    pub props: Vec<PropSchema>,
}

#[derive(Default)]
pub struct Mappings {
    pub structs: HashMap<String, StructSchema>,
    pub enums: HashMap<String, Vec<(i64, String)>>,
}

impl Mappings {
    pub fn parse(data: &[u8]) -> Result<Mappings> {
        let mut r = Reader::new(data);
        if r.u16()? != MAGIC {
            return Err("not a .usmap file (bad magic)".into());
        }
        let version = r.u8()?;
        if version > VERSION_EXPLICIT_ENUM_VALUES {
            return Err(format!("unsupported .usmap version {}", version));
        }
        if version >= VERSION_PACKAGE_VERSIONING && r.bool32()? {
            r.skip(8)?;
            let custom_versions = r.count(20)?;
            r.skip(custom_versions * 20 + 4)?;
        }
        let method = r.u8()?;
        let compressed_size = r.u32()? as usize;
        let size = r.u32()? as usize;
        let payload = r.bytes(compressed_size)?;
        let body = match method {
            0 => payload.to_vec(),
            3 => {
                let mut out = Vec::with_capacity(size);
                ruzstd::decoding::StreamingDecoder::new(payload)
                    .map_err(|e| format!("invalid ZStandard data in .usmap: {}", e))?
                    .read_to_end(&mut out)
                    .map_err(|e| format!("invalid ZStandard data in .usmap: {}", e))?;
                out
            }
            1 => return Err("Oodle-compressed .usmap files are not supported; export it uncompressed or with ZStandard".into()),
            2 => return Err("Brotli-compressed .usmap files are not supported; export it uncompressed or with ZStandard".into()),
            m => return Err(format!("unknown .usmap compression method {}", m)),
        };
        if body.len() != size {
            return Err(format!(".usmap payload is {} bytes, expected {}", body.len(), size));
        }
        Body { r: Reader::new(&body), version, names: Vec::new() }.read()
    }

    pub fn find(&self, name: &str) -> Option<&StructSchema> {
        self.structs.get(name)
    }

    pub fn enum_name(&self, enum_name: &str, value: i64) -> Option<&str> {
        self.enums.get(enum_name)?.iter().find(|(v, _)| *v == value).map(|(_, n)| n.as_str())
    }

    pub fn property_at(&self, struct_name: &str, mut index: usize) -> Option<(&PropSchema, usize)> {
        let mut chain = Vec::new();
        let mut current = Some(struct_name);
        while let Some(name) = current {
            let Some(schema) = self.structs.get(name) else {
                if chain.is_empty() {
                    return None;
                }
                break;
            };
            chain.push(schema);
            current = schema.super_name.as_deref();
        }
        for schema in chain {
            let count = schema.prop_count as usize;
            if index < count {
                return schema.props.iter().find_map(|p| {
                    let start = p.schema_index as usize;
                    (index >= start && index < start + p.array_size.max(1) as usize).then_some((p, index - start))
                });
            }
            index -= count;
        }
        None
    }
}

struct Body<'a> {
    r: Reader<'a>,
    version: u8,
    names: Vec<String>,
}

impl Body<'_> {
    fn name(&mut self) -> Result<String> {
        let index = self.r.i32()?;
        if index < 0 {
            return Ok(String::new());
        }
        self.names.get(index as usize).cloned().ok_or_else(|| format!(".usmap name index {} out of range", index))
    }

    fn prop_type(&mut self) -> Result<PropType> {
        let id = self.r.u8()?;
        Ok(match id {
            0 => PropType::Byte,
            1 => PropType::Bool,
            2 => PropType::Int,
            3 => PropType::Float,
            4 => PropType::Object,
            5 => PropType::Name,
            6 => PropType::Delegate,
            7 => PropType::Double,
            8 => PropType::Array(Box::new(self.prop_type()?)),
            9 => PropType::Struct(self.name()?),
            10 => PropType::Str,
            11 => PropType::Text,
            12 => PropType::Interface,
            13 => PropType::MulticastDelegate,
            14 => PropType::WeakObject,
            15 => PropType::LazyObject,
            16 | 17 => PropType::SoftObject,
            18 => PropType::UInt64,
            19 => PropType::UInt32,
            20 => PropType::UInt16,
            21 => PropType::Int64,
            22 => PropType::Int16,
            23 => PropType::Int8,
            24 => {
                let key = self.prop_type()?;
                PropType::Map(Box::new(key), Box::new(self.prop_type()?))
            }
            25 => PropType::Set(Box::new(self.prop_type()?)),
            26 => {
                let inner = self.prop_type()?;
                PropType::Enum(Box::new(inner), self.name()?)
            }
            27 => PropType::FieldPath,
            28 => PropType::Optional(Box::new(self.prop_type()?)),
            29 => PropType::Utf8Str,
            30 => PropType::AnsiStr,
            255 => PropType::Unknown,
            _ => return Err(format!("unknown .usmap property type {}", id)),
        })
    }

    fn read(mut self) -> Result<Mappings> {
        let name_count = self.r.u32()? as usize;
        for _ in 0..name_count {
            let len = if self.version >= VERSION_LONG_FNAME { self.r.u16()? as usize } else { self.r.u8()? as usize };
            self.names.push(String::from_utf8_lossy(self.r.bytes(len)?).into_owned());
        }
        let mut mappings = Mappings::default();
        let enum_count = self.r.u32()?;
        for _ in 0..enum_count {
            let name = self.name()?;
            let entries = if self.version >= VERSION_LARGE_ENUMS { self.r.u16()? as usize } else { self.r.u8()? as usize };
            let mut values = Vec::with_capacity(entries);
            for i in 0..entries {
                let value = if self.version >= VERSION_EXPLICIT_ENUM_VALUES { self.r.i64()? } else { i as i64 };
                let entry = self.name()?;
                values.push((value, entry.rsplit("::").next().unwrap_or(&entry).to_string()));
            }
            mappings.enums.insert(name, values);
        }
        let struct_count = self.r.u32()?;
        for _ in 0..struct_count {
            let name = self.name()?;
            let super_name = Some(self.name()?).filter(|s| !s.is_empty());
            let prop_count = self.r.u16()?;
            let serializable = self.r.u16()?;
            let mut props = Vec::with_capacity(serializable as usize);
            for _ in 0..serializable {
                let schema_index = self.r.u16()?;
                let array_size = self.r.u8()?;
                let name = self.name()?;
                props.push(PropSchema { name, schema_index, array_size, ty: self.prop_type()? });
            }
            mappings.structs.insert(name, StructSchema { super_name, prop_count, props });
        }
        Ok(mappings)
    }
}
