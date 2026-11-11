use crate::reader::{Reader, Result};
use crate::usmap::Mappings;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const PACKAGE_TAG: u32 = 0x9E2A83C1;
const PKG_FILTER_EDITOR_ONLY: u32 = 0x8000_0000;
const PKG_UNVERSIONED_PROPERTIES: u32 = 0x2000;
const UE5_NAMES_REFERENCED_FROM_EXPORT_DATA: i32 = 1001;
const UE5_PAYLOAD_TOC: i32 = 1002;
const UE5_LARGE_WORLD_COORDINATES: i32 = 1004;
const UE5_SOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES: i32 = 1007;
const UE5_ADD_SOFTOBJECTPATH_LIST: i32 = 1008;
const UE5_DATA_RESOURCES: i32 = 1009;
const UE5_PROPERTY_TAG_EXTENSION: i32 = 1011;
const UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME: i32 = 1012;
const UE5_METADATA_SERIALIZATION_OFFSET: i32 = 1014;
const UE5_VERSE_CELLS: i32 = 1015;
const UE5_IMPORT_TYPE_HIERARCHIES: i32 = 1018;
const MIN_NAME_ENTRY_SIZE: usize = 8;

pub struct ObjectImport {
    pub class_name: String,
    pub outer_index: i32,
    pub object_name: String,
}

pub struct ObjectExport {
    pub class_index: i32,
    pub outer_index: i32,
    pub object_name: String,
    pub serial_size: i64,
    pub serial_offset: i64,
}

pub struct DataResource {
    pub serial_offset: i64,
    pub serial_size: i64,
    pub legacy_flags: u32,
}

pub struct Package {
    pub path: String,
    pub names: Vec<String>,
    pub imports: Vec<ObjectImport>,
    pub exports: Vec<ObjectExport>,
    pub header_size: usize,
    pub bulk_data_start_offset: i64,
    pub engine_version: Option<(u16, u16, u16)>,
    pub is_ue5: bool,
    pub ue5_version: i32,
    pub unversioned_properties: bool,
    pub soft_object_path_list: bool,
    pub soft_object_paths: Vec<String>,
    pub mappings: Option<Arc<Mappings>>,
    pub data_resources: Option<Vec<DataResource>>,
    pub uexp: Vec<u8>,
    pub ubulk: Option<Vec<u8>>,
    pub uptnl: Option<Vec<u8>>,
}

struct Summary {
    is_ue5: bool,
    ue5_version: i32,
    header_size: usize,
    package_flags: u32,
    name_count: i32,
    name_offset: i32,
    soft_object_path_count: i32,
    soft_object_path_offset: i32,
    export_count: i32,
    export_offset: i32,
    import_count: i32,
    import_offset: i32,
    depends_offset: i32,
    saved_by: (u16, u16, u16),
    bulk_data_start_offset: i64,
    data_resource_offset: Option<i32>,
}

fn read_engine_version(r: &mut Reader) -> Result<(u16, u16, u16)> {
    let major = r.u16()?;
    let minor = r.u16()?;
    let patch = r.u16()?;
    let _changelist = r.u32()?;
    let _branch = r.fstring()?;
    Ok((major, minor, patch))
}

fn generations_follow(r: &Reader, export_count: i32, name_count: i32) -> bool {
    let at = |i: usize| r.data.get(r.pos + i * 4..r.pos + i * 4 + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    match at(0) {
        Some(n) if n > 0 && n < 1024 => {
            let last = 1 + (n as usize - 1) * 2;
            at(last) == Some(export_count) && at(last + 1) == Some(name_count)
        }
        _ => false,
    }
}

fn read_summary(r: &mut Reader, assume_soft_object_paths: bool) -> Result<Summary> {
    if r.u32()? != PACKAGE_TAG {
        return Err("not an Unreal package (bad tag)".into());
    }
    let legacy_version = r.i32()?;
    if !(-9..=-4).contains(&legacy_version) {
        return Err(format!("unsupported legacy file version {}", legacy_version));
    }
    if legacy_version != -4 {
        r.i32()?;
    }
    let file_version_ue4 = r.i32()?;
    let is_ue5 = legacy_version <= -8;
    let file_version_ue5 = if is_ue5 { r.i32()? } else { 0 };
    r.i32()?;
    let unversioned = file_version_ue4 == 0;
    let saved_hash = legacy_version <= -9;
    let has_ue5 = |version: i32, if_unversioned: bool| {
        is_ue5 && if unversioned { if_unversioned } else { file_version_ue5 >= version }
    };
    let mut header_size = 0;
    if saved_hash {
        r.skip(20)?;
        header_size = r.i32()? as usize;
    }
    let custom_versions = r.count(20)?;
    r.skip(custom_versions * 20)?;
    if !saved_hash {
        header_size = r.i32()? as usize;
    }
    r.fstring()?;
    let package_flags = r.u32()?;
    let name_count = r.i32()?;
    let name_offset = r.i32()?;
    let mut soft_object_path_count = 0;
    let mut soft_object_path_offset = 0;
    if has_ue5(UE5_ADD_SOFTOBJECTPATH_LIST, assume_soft_object_paths) {
        soft_object_path_count = r.i32()?;
        soft_object_path_offset = r.i32()?;
    }
    if package_flags & PKG_FILTER_EDITOR_ONLY == 0 && (unversioned || file_version_ue4 >= 516) {
        r.fstring()?;
    }
    r.skip(8)?;
    let export_count = r.i32()?;
    let export_offset = r.i32()?;
    let import_count = r.i32()?;
    let import_offset = r.i32()?;
    if has_ue5(UE5_VERSE_CELLS, saved_hash) {
        r.skip(16)?;
    }
    if has_ue5(UE5_METADATA_SERIALIZATION_OFFSET, saved_hash) {
        r.i32()?;
    }
    let depends_offset = r.i32()?;
    r.skip(16)?;
    if !saved_hash {
        r.skip(16)?;
    }
    let import_type_hierarchies = if unversioned {
        !generations_follow(r, export_count, name_count)
    } else {
        has_ue5(UE5_IMPORT_TYPE_HIERARCHIES, false)
    };
    if import_type_hierarchies {
        r.skip(8)?;
    }
    let generations = r.count(8)?;
    r.skip(generations * 8)?;
    let saved_by = read_engine_version(r)?;
    read_engine_version(r)?;
    let compression_flags = r.u32()?;
    let compressed_chunks = r.i32()?;
    if compression_flags != 0 || compressed_chunks != 0 {
        return Err("compressed packages are not supported".into());
    }
    r.u32()?;
    let additional = r.count(4)?;
    for _ in 0..additional {
        r.fstring()?;
    }
    if legacy_version > -7 {
        r.i32()?;
    }
    r.i32()?;
    let bulk_data_start_offset = r.i64()?;
    let mut data_resource_offset = None;
    if is_ue5 {
        r.i32()?;
        let chunk_ids = r.count(4)?;
        r.skip(chunk_ids * 4)?;
        r.skip(8)?;
        let tail = if unversioned {
            let remaining = name_offset as i64 - r.pos as i64;
            if ![0, 4, 12, 16].contains(&remaining) {
                return Err(format!("unexpected package summary layout ({} bytes before the name map)", remaining));
            }
            remaining as usize
        } else {
            [(UE5_NAMES_REFERENCED_FROM_EXPORT_DATA, 4), (UE5_PAYLOAD_TOC, 8), (UE5_DATA_RESOURCES, 4)]
                .iter()
                .filter(|(version, _)| file_version_ue5 >= *version)
                .map(|(_, size)| size)
                .sum()
        };
        r.skip(tail.min(12))?;
        if tail == 16 {
            data_resource_offset = Some(r.i32()?);
        }
    }
    Ok(Summary {
        is_ue5,
        ue5_version: file_version_ue5,
        header_size,
        package_flags,
        name_count,
        name_offset,
        soft_object_path_count,
        soft_object_path_offset,
        export_count,
        export_offset,
        import_count,
        import_offset,
        depends_offset,
        saved_by,
        bulk_data_start_offset,
        data_resource_offset,
    })
}

fn read_data_resources(r: &mut Reader, offset: i32) -> Result<Vec<DataResource>> {
    r.pos = offset as usize;
    let version = r.u32()?;
    if !(1..=2).contains(&version) {
        return Err(format!("unsupported data resource version {}", version));
    }
    let count = r.count(44)?;
    (0..count)
        .map(|_| {
            r.u32()?;
            if version >= 2 {
                r.u8()?;
            }
            let serial_offset = r.i64()?;
            r.i64()?;
            let serial_size = r.i64()?;
            r.i64()?;
            r.i32()?;
            let legacy_flags = r.u32()?;
            Ok(DataResource { serial_offset, serial_size, legacy_flags })
        })
        .collect()
}

pub fn soft_path_text(package: String, asset: String, sub_path: String) -> String {
    let mut path = package;
    if asset != "None" && !asset.is_empty() {
        path = format!("{}.{}", path, asset);
    }
    if !sub_path.is_empty() {
        path = format!("{}:{}", path, sub_path);
    }
    path
}

pub fn is_package_header(head: &[u8]) -> bool {
    head.len() >= 4 && u32::from_le_bytes([head[0], head[1], head[2], head[3]]) == PACKAGE_TAG
}

impl Package {
    pub fn parse_header(path: &str, uasset: &[u8]) -> Result<Package> {
        Package::parse(path, uasset, Vec::new(), None)
    }

    pub fn open(uasset: &Path) -> Result<Package> {
        let bytes = std::fs::read(uasset).map_err(|e| format!("{}: {}", uasset.display(), e))?;
        let uexp_path = uasset.with_extension("uexp");
        let ubulk = std::fs::read(uasset.with_extension("ubulk")).ok();
        let path = uasset.to_string_lossy();
        let mut pkg = match std::fs::read(&uexp_path) {
            Ok(uexp) => Package::parse(&path, &bytes, uexp, ubulk)?,
            Err(_) if Package::has_embedded_exports(&bytes) => Package::parse_embedded(&path, &bytes, ubulk)?,
            Err(e) => return Err(format!("{}: {}", uexp_path.display(), e)),
        };
        pkg.uptnl = std::fs::read(uasset.with_extension("uptnl")).ok();
        Ok(pkg)
    }

    pub fn has_embedded_exports(uasset: &[u8]) -> bool {
        Package::parse_header("", uasset).is_ok_and(|p| p.header_size > 0 && p.header_size < uasset.len())
    }

    pub fn parse_embedded(path: &str, uasset: &[u8], ubulk: Option<Vec<u8>>) -> Result<Package> {
        let mut pkg = Package::parse(path, uasset, Vec::new(), ubulk)?;
        pkg.uexp = uasset.get(pkg.header_size..).ok_or("package header is larger than the file")?.to_vec();
        Ok(pkg)
    }

    pub fn imported_packages(&self) -> Vec<&str> {
        self.imports
            .iter()
            .filter(|imp| imp.outer_index == 0 && imp.class_name == "Package")
            .map(|imp| imp.object_name.as_str())
            .collect()
    }

    pub fn asset_class_counts(&self) -> BTreeMap<String, u32> {
        let mut types = BTreeMap::new();
        for export in self.exports.iter().filter(|e| e.outer_index == 0) {
            if let Some(class) = self.export_class_name(export) {
                *types.entry(class.to_string()).or_insert(0) += 1;
            }
        }
        types
    }

    pub fn parse(path: &str, uasset: &[u8], uexp: Vec<u8>, ubulk: Option<Vec<u8>>) -> Result<Package> {
        let mut r = Reader::new(uasset);
        let summary = match read_summary(&mut r, true) {
            Ok(summary) => summary,
            Err(first) => {
                r.pos = 0;
                read_summary(&mut r, false).map_err(|_| first)?
            }
        };
        let name_count = usize::try_from(summary.name_count).ok().filter(|&n| n <= uasset.len() / MIN_NAME_ENTRY_SIZE);
        let name_count = name_count.ok_or_else(|| format!("invalid name count {}", summary.name_count))?;
        let mut names = Vec::with_capacity(name_count);
        r.pos = summary.name_offset as usize;
        for _ in 0..name_count {
            names.push(r.fstring()?);
            r.u32()?;
        }
        let read_name = |r: &mut Reader| -> Result<String> {
            let index = r.i32()?;
            let number = r.i32()?;
            let base = names
                .get(index as usize)
                .ok_or_else(|| format!("name index {} out of range", index))?;
            Ok(if number > 0 { format!("{}_{}", base, number - 1) } else { base.clone() })
        };
        r.pos = summary.soft_object_path_offset as usize;
        let soft_object_paths = (0..summary.soft_object_path_count)
            .map(|_| -> Result<String> {
                let package = read_name(&mut r)?;
                let asset = read_name(&mut r)?;
                Ok(soft_path_text(package, asset, r.fstring()?))
            })
            .collect::<Result<Vec<_>>>()
            .unwrap_or_default();
        let import_entry_size = match summary.export_offset.wrapping_sub(summary.import_offset) {
            span if summary.import_count > 0 && span > 0 && [28, 32, 36, 40].contains(&(span / summary.import_count)) => {
                span / summary.import_count
            }
            _ if summary.is_ue5 => 32,
            _ => 28,
        };
        let mut imports = Vec::new();
        for i in 0..summary.import_count {
            r.pos = summary.import_offset.wrapping_add(i.wrapping_mul(import_entry_size)) as usize;
            let _class_package = read_name(&mut r)?;
            let class_name = read_name(&mut r)?;
            let outer_index = r.i32()?;
            let object_name = read_name(&mut r)?;
            imports.push(ObjectImport { class_name, outer_index, object_name });
        }
        let export_entry_size = if summary.export_count > 0 {
            summary.depends_offset.wrapping_sub(summary.export_offset) / summary.export_count
        } else {
            0
        };
        if summary.export_count > 0 && export_entry_size <= 0 {
            return Err(format!("invalid export map layout ({} exports)", summary.export_count));
        }
        let mut exports = Vec::new();
        for i in 0..summary.export_count {
            r.pos = summary.export_offset.wrapping_add(i.wrapping_mul(export_entry_size)) as usize;
            let class_index = r.i32()?;
            r.i32()?;
            if summary.is_ue5 || export_entry_size >= 100 {
                r.i32()?;
            }
            let outer_index = r.i32()?;
            let object_name = read_name(&mut r)?;
            r.u32()?;
            let (serial_size, serial_offset) = if summary.is_ue5 || export_entry_size >= 96 {
                (r.i64()?, r.i64()?)
            } else {
                (r.i32()? as i64, r.i32()? as i64)
            };
            exports.push(ObjectExport { class_index, outer_index, object_name, serial_size, serial_offset });
        }
        let data_resources = match summary.data_resource_offset {
            Some(offset) if offset > 0 => Some(read_data_resources(&mut r, offset)?),
            Some(_) => Some(Vec::new()),
            None => None,
        };
        let saved_by = summary.saved_by;
        Ok(Package {
            path: path.to_string(),
            names,
            imports,
            exports,
            header_size: summary.header_size,
            bulk_data_start_offset: summary.bulk_data_start_offset,
            engine_version: if saved_by.0 > 0 { Some(saved_by) } else { None },
            is_ue5: summary.is_ue5,
            ue5_version: summary.ue5_version,
            unversioned_properties: summary.package_flags & PKG_UNVERSIONED_PROPERTIES != 0,
            soft_object_path_list: summary.soft_object_path_count > 0,
            soft_object_paths,
            mappings: None,
            data_resources,
            uexp,
            ubulk,
            uptnl: None,
        })
    }

    pub fn name(&self, index: i32, number: i32) -> Result<String> {
        let base = self
            .names
            .get(index as usize)
            .ok_or_else(|| format!("name index {} out of range", index))?;
        Ok(if number > 0 { format!("{}_{}", base, number - 1) } else { base.clone() })
    }

    pub fn read_name(&self, r: &mut Reader) -> Result<String> {
        let index = r.i32()?;
        let number = r.i32()?;
        self.name(index, number)
    }

    pub fn object_name(&self, index: i32) -> Option<&str> {
        if index < 0 {
            self.imports.get(-(index + 1) as usize).map(|i| i.object_name.as_str())
        } else if index > 0 {
            self.exports.get((index - 1) as usize).map(|e| e.object_name.as_str())
        } else {
            None
        }
    }

    pub fn import_path(&self, index: i32) -> Option<String> {
        if index >= 0 {
            return None;
        }
        let mut parts = Vec::new();
        let mut current = index;
        while current < 0 {
            if parts.len() >= self.imports.len() {
                return None;
            }
            let import = self.imports.get(-(current + 1) as usize)?;
            parts.push(import.object_name.clone());
            current = import.outer_index;
        }
        parts.reverse();
        Some(parts.join("."))
    }

    fn effective_ue5_version(&self) -> i32 {
        if self.ue5_version != 0 {
            return self.ue5_version;
        }
        self.mappings.as_ref().and_then(|m| m.package_version).map_or(0, |(_, ue5)| ue5)
    }

    pub fn large_world_coordinates(&self) -> bool {
        let version = self.effective_ue5_version();
        self.is_ue5 && (version == 0 || version >= UE5_LARGE_WORLD_COORDINATES)
    }

    pub fn property_tag_extensions(&self) -> bool {
        self.effective_ue5_version() >= UE5_PROPERTY_TAG_EXTENSION
    }

    pub fn complete_type_names(&self) -> bool {
        self.effective_ue5_version() >= UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME
    }

    pub fn soft_paths_have_asset_names(&self) -> bool {
        let version = self.effective_ue5_version();
        self.is_ue5 && (version == 0 || version >= UE5_SOFTOBJECTPATH_REMOVE_ASSET_PATH_FNAMES)
    }

    pub fn export_class_name(&self, export: &ObjectExport) -> Option<&str> {
        self.object_name(export.class_index)
    }

    pub fn export_data(&self, export: &ObjectExport) -> Result<&[u8]> {
        let start = usize::try_from(export.serial_offset.saturating_sub(self.header_size as i64)).ok();
        let size = usize::try_from(export.serial_size).ok();
        start
            .zip(size)
            .and_then(|(start, size)| self.uexp.get(start..start.checked_add(size)?))
            .ok_or_else(|| format!("export {} lies outside the .uexp", export.object_name))
    }
}
