use crate::iostore::{ChunkKind, IoStore, is_utoc};
use crate::reader::{Reader, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const NAME_HASH_VERSION: u64 = 0xC164_0000;
const CONTAINER_HEADER_SIGNATURE: u32 = 0x496F_436E;
const CONTAINER_OPTIONAL_SEGMENTS: u32 = 2;
const CONTAINER_NO_EXPORT_INFO: u32 = 3;
const INDEX_MASK: u64 = (1 << 62) - 1;
const NULL_INDEX: u64 = u64::MAX;
const TYPE_EXPORT: u64 = 0;
const TYPE_SCRIPT: u64 = 1;
const TYPE_PACKAGE: u64 = 2;
const EXPORT_ENTRY_SIZE: usize = 72;
const BULK_ENTRY_SIZE: usize = 32;
const COMMAND_SERIALIZE: u32 = 1;
const UE5_VERSE_CELLS: i32 = 1015;
const UE5_SUMMARY_SIZE: usize = 52;
const UE5_LEGACY_SUMMARY_SIZE: usize = 44;
const UE4_SUMMARY_SIZE: usize = 64;
const PACKAGE_TAG: u32 = 0x9E2A83C1;
const PKG_FILTER_EDITOR_ONLY: u32 = 0x8000_0000;
const UE5_EXPORT_SIZE: usize = 44;
const UE4_EXPORT_SIZE: usize = 96;
const MAX_OUTER_DEPTH: usize = 64;
const GLOBAL_ERROR: &str = "IoStore packages need global.utoc/global.ucas from the same folder";

#[derive(Clone, Copy)]
struct MappedName {
    index: u32,
    number: u32,
    global: bool,
}

struct ScriptObject {
    name: MappedName,
    outer: u64,
    cdo_class: u64,
}

struct GlobalData {
    names: Vec<String>,
    scripts: HashMap<u64, ScriptObject>,
}

struct StoreEntry {
    bundle_count: usize,
    imported: Vec<u64>,
}

struct ZenExport {
    cooked_offset: u64,
    size: u64,
    name: MappedName,
    outer: u64,
    class: u64,
    super_index: u64,
    template: u64,
    key: u64,
    flags: u32,
}

struct BulkEntry {
    offset: i64,
    size: i64,
    flags: u32,
}

struct ZenHeader {
    ue5: bool,
    name: String,
    names: Vec<String>,
    flags: u32,
    cooked_header_size: u64,
    data_start: usize,
    imports: Vec<u64>,
    hashes: Vec<u64>,
    imported_packages: Vec<u64>,
    exports: Vec<ZenExport>,
    bundles: Option<Vec<(u32, u32)>>,
    bundle_entries: Vec<(u32, u32)>,
    bulk: Option<Vec<BulkEntry>>,
}

struct PackageRef {
    store: usize,
    header: ZenHeader,
}

pub struct IoStoreSet {
    pub stores: Vec<IoStore>,
    global: GlobalData,
    entries: HashMap<u64, StoreEntry>,
    packages: HashMap<u64, PackageRef>,
    global_exports: HashMap<u64, (u64, usize)>,
}

pub struct IoEntry {
    pub store: usize,
    pub path: PathBuf,
    pub chunk: usize,
}

fn mapped(r: &mut Reader) -> Result<MappedName> {
    let index = r.u32()?;
    let number = r.u32()?;
    Ok(MappedName { index: index & 0x3FFF_FFFF, number, global: index >> 30 != 0 })
}

fn u64s(r: &mut Reader, count: usize) -> Result<Vec<u64>> {
    if count.saturating_mul(8) > r.remaining() {
        return Err(format!("invalid IoStore array count {} at {}", count, r.pos));
    }
    (0..count).map(|_| r.i64().map(|v| v as u64)).collect()
}

fn span(from: usize, to: usize, size: usize) -> Result<usize> {
    to.checked_sub(from).map(|n| n / size).ok_or_else(|| format!("invalid IoStore package layout ({} > {})", from, to))
}

fn name_text(base: &str, number: u32) -> String {
    if number > 0 { format!("{}_{}", base, number - 1) } else { base.to_string() }
}

fn name_header(b: &[u8]) -> (bool, usize) {
    (b[0] & 0x80 != 0, (((b[0] & 0x7F) as usize) << 8) | b[1] as usize)
}

fn name_string(r: &mut Reader, wide: bool, len: usize) -> Result<String> {
    if wide {
        let units: Vec<u16> = r.bytes(len * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(String::from_utf16_lossy(&units))
    } else {
        Ok(r.bytes(len)?.iter().map(|&b| b as char).collect())
    }
}

fn is_name_batch(d: &[u8], at: usize) -> bool {
    let read = |i: usize, n: usize| d.get(at + i..at + i + n);
    let count = read(0, 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let version = read(8, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    count.is_some_and(|c| c > 0) && version == Some(NAME_HASH_VERSION)
}

fn load_name_batch(r: &mut Reader) -> Result<Vec<String>> {
    let count = r.count(10)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let string_bytes = r.u32()? as usize;
    if r.i64()? as u64 != NAME_HASH_VERSION {
        return Err("unsupported IoStore name batch".to_string());
    }
    r.skip(count * 8)?;
    let headers: Vec<(bool, usize)> = r.bytes(count * 2)?.chunks_exact(2).map(name_header).collect();
    let packed: usize = headers.iter().map(|&(wide, len)| if wide { len * 2 } else { len }).sum();
    let base = r.pos;
    let mut names = Vec::with_capacity(count);
    for &(wide, len) in &headers {
        if wide && packed != string_bytes && (r.pos - base) % 2 == 1 {
            r.skip(1)?;
        }
        names.push(name_string(r, wide, len)?);
    }
    Ok(names)
}

fn load_name_batch_old(r: &mut Reader, count: usize) -> Result<Vec<String>> {
    if count.saturating_mul(2) > r.remaining() {
        return Err(format!("invalid IoStore name count {}", count));
    }
    (0..count)
        .map(|_| {
            let (wide, len) = name_header(r.bytes(2)?);
            if wide && r.pos % 2 == 1 {
                r.skip(1)?;
            }
            name_string(r, wide, len)
        })
        .collect()
}

fn read_script_objects(r: &mut Reader) -> Result<HashMap<u64, ScriptObject>> {
    let count = r.count(32)?;
    let mut scripts = HashMap::with_capacity(count);
    for _ in 0..count {
        let name = mapped(r)?;
        let global = r.i64()? as u64;
        let outer = r.i64()? as u64;
        let cdo_class = r.i64()? as u64;
        scripts.insert(global, ScriptObject { name, outer, cdo_class });
    }
    Ok(scripts)
}

impl GlobalData {
    fn load(store: &IoStore) -> Result<GlobalData> {
        if store.ue5 {
            let chunk = store.find(0, ChunkKind::ScriptObjects).ok_or(GLOBAL_ERROR)?;
            let data = store.read(chunk)?;
            let mut r = Reader::new(&data);
            let names = load_name_batch(&mut r)?;
            let scripts = read_script_objects(&mut r)?;
            return Ok(GlobalData { names, scripts });
        }
        let hashes = store.find(0, ChunkKind::LoaderGlobalNameHashes).ok_or(GLOBAL_ERROR)?;
        let count = (store.chunk_size(hashes) / 8).saturating_sub(1);
        let names_data = store.read(store.find(0, ChunkKind::LoaderGlobalNames).ok_or(GLOBAL_ERROR)?)?;
        let names = load_name_batch_old(&mut Reader::new(&names_data), count)?;
        let meta = store.read(store.find(0, ChunkKind::LoaderInitialLoadMeta).ok_or(GLOBAL_ERROR)?)?;
        let scripts = read_script_objects(&mut Reader::new(&meta))?;
        Ok(GlobalData { names, scripts })
    }

    fn name(&self, name: MappedName) -> String {
        self.names.get(name.index as usize).map_or_else(|| "None".to_string(), |base| name_text(base, name.number))
    }
}

fn array_view(data: &[u8], at: usize) -> Result<Vec<u64>> {
    let mut r = Reader::new(data);
    r.pos = at;
    let count = r.u32()? as usize;
    let offset = r.u32()? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    r.pos = at.saturating_add(offset);
    u64s(&mut r, count)
}

fn read_container_header(data: &[u8], ue5: bool, out: &mut HashMap<u64, StoreEntry>) -> Result<()> {
    let mut r = Reader::new(data);
    let version = if ue5 {
        if r.u32()? != CONTAINER_HEADER_SIGNATURE {
            return Err("unsupported IoStore container header".to_string());
        }
        Some(r.u32()?)
    } else {
        None
    };
    r.skip(8)?;
    if version.is_none_or(|v| v < CONTAINER_OPTIONAL_SEGMENTS) {
        r.u32()?;
    }
    if version.is_none() {
        for _ in 0..2 {
            let size = r.count(1)?;
            r.skip(size)?;
        }
    }
    let count = r.count(8)?;
    let ids = u64s(&mut r, count)?;
    let size = r.count(1)?;
    let store = r.bytes(size)?;
    let (entry_size, counts) = match version {
        Some(v) if v < CONTAINER_NO_EXPORT_INFO => (24, Some(0)),
        Some(_) => (16, None),
        None => (32, Some(8)),
    };
    for (i, id) in ids.into_iter().enumerate() {
        let at = i * entry_size;
        let mut e = Reader::new(store);
        let bundle_count = match counts {
            Some(skip) => {
                e.pos = at + skip + 4;
                e.i32()?.max(0) as usize
            }
            None => 1,
        };
        let view = at + counts.map_or(0, |skip| skip + 8 + if version.is_none() { 8 } else { 0 });
        let imported = array_view(store, view)?;
        out.insert(id, StoreEntry { bundle_count, imported });
    }
    Ok(())
}

fn header_length(prefix: &[u8], ue5: bool) -> Option<usize> {
    let at = |i: usize| prefix.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if ue5 { at(4) } else { Some(at(52)?.saturating_add(at(56)?)) }
}

fn read_export(r: &mut Reader) -> Result<ZenExport> {
    let start = r.pos;
    let cooked_offset = r.i64()? as u64;
    let size = r.i64()? as u64;
    let name = mapped(r)?;
    let outer = r.i64()? as u64;
    let class = r.i64()? as u64;
    let super_index = r.i64()? as u64;
    let template = r.i64()? as u64;
    let key = r.i64()? as u64;
    let flags = r.u32()?;
    r.pos = start + EXPORT_ENTRY_SIZE;
    Ok(ZenExport { cooked_offset, size, name, outer, class, super_index, template, key, flags })
}

fn read_exports(r: &mut Reader, at: usize, count: usize) -> Result<Vec<ZenExport>> {
    r.pos = at;
    if count.saturating_mul(EXPORT_ENTRY_SIZE) > r.remaining() {
        return Err(format!("invalid IoStore export count {}", count));
    }
    (0..count).map(|_| read_export(r)).collect()
}

fn read_bundle_entries(r: &mut Reader, count: usize) -> Result<Vec<(u32, u32)>> {
    if count.saturating_mul(8) > r.remaining() {
        return Err(format!("invalid IoStore export bundle count {}", count));
    }
    (0..count).map(|_| Ok((r.u32()?, r.u32()?))).collect()
}

fn bulk_map_at(d: &[u8], p: usize, end: usize) -> Option<usize> {
    let at = |i: usize| d.get(i..i + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize);
    let fits = |start: usize| at(start).and_then(|size| start.checked_add(8)?.checked_add(size)).is_some_and(|stop| stop <= end && end - stop < 8);
    if fits(p) {
        return Some(p);
    }
    let pad = at(p)?;
    let q = p.checked_add(8)?.checked_add(pad)?;
    (pad < 256 && fits(q)).then_some(q)
}

fn summary_layout(d: &[u8], has_versioning: bool) -> Option<(bool, usize)> {
    for (modern, size) in [(true, UE5_SUMMARY_SIZE), (false, UE5_LEGACY_SUMMARY_SIZE)] {
        let mut r = Reader::new(d);
        r.pos = size;
        let mut cells = None;
        if has_versioning {
            let read = |r: &mut Reader| -> Result<i32> {
                r.skip(8)?;
                let ue5 = r.i32()?;
                r.skip(4)?;
                let custom = r.count(20)?;
                r.skip(custom * 20)?;
                Ok(ue5)
            };
            match read(&mut r) {
                Ok(ue5) => cells = Some(ue5 >= UE5_VERSE_CELLS),
                Err(_) => continue,
            }
        }
        for with_cells in [false, true] {
            let at = r.pos + if with_cells { 8 } else { 0 };
            if cells.is_none_or(|c| c == with_cells) && is_name_batch(d, at) {
                return Some((modern, at));
            }
        }
    }
    None
}

fn parse_ue5(d: &[u8], entry: Option<&StoreEntry>) -> Result<ZenHeader> {
    let mut r = Reader::new(d);
    let has_versioning = r.u32()? != 0;
    let data_start = r.u32()? as usize;
    let name = mapped(&mut r)?;
    let flags = r.u32()?;
    let cooked_header_size = r.u32()? as u64;
    let hashes_at = r.i32()? as usize;
    let imports_at = r.i32()? as usize;
    let exports_at = r.i32()? as usize;
    let entries_at = r.i32()? as usize;
    let graph_at = r.i32()? as usize;
    let (modern, names_at) = summary_layout(d, has_versioning).ok_or("unsupported IoStore package summary")?;
    r.pos = names_at;
    let names = load_name_batch(&mut r)?;
    let bulk = match bulk_map_at(d, r.pos, hashes_at) {
        Some(at) if r.pos != hashes_at => {
            r.pos = at;
            let size = r.i64()? as usize;
            let count = size / BULK_ENTRY_SIZE;
            if count.saturating_mul(BULK_ENTRY_SIZE) > r.remaining() {
                return Err(format!("invalid IoStore bulk data map size {}", size));
            }
            let entries = (0..count)
                .map(|_| {
                    let offset = r.i64()?;
                    r.i64()?;
                    let size = r.i64()?;
                    let flags = r.u32()?;
                    r.skip(4)?;
                    Ok(BulkEntry { offset, size, flags })
                })
                .collect::<Result<Vec<_>>>()?;
            Some(entries)
        }
        _ => None,
    };
    r.pos = hashes_at;
    let hashes = u64s(&mut r, span(hashes_at, imports_at, 8)?)?;
    let imports = u64s(&mut r, span(imports_at, exports_at, 8)?)?;
    let exports = read_exports(&mut r, exports_at, span(exports_at, entries_at, EXPORT_ENTRY_SIZE)?)?;
    let (bundles, bundle_entries) = if modern {
        (None, Vec::new())
    } else {
        r.pos = graph_at;
        let count = entry.map_or(1, |e| e.bundle_count);
        if count.saturating_mul(16) > r.remaining() {
            return Err(format!("invalid IoStore export bundle count {}", count));
        }
        let headers = (0..count)
            .map(|_| {
                r.skip(8)?;
                Ok((r.u32()?, r.u32()?))
            })
            .collect::<Result<Vec<_>>>()?;
        r.pos = entries_at;
        (Some(headers), read_bundle_entries(&mut r, exports.len() * 2)?)
    };
    let name = if name.global { "None".to_string() } else { names.get(name.index as usize).map_or_else(|| "None".to_string(), |b| name_text(b, name.number)) };
    Ok(ZenHeader {
        ue5: true,
        name,
        names,
        flags,
        cooked_header_size,
        data_start,
        imports,
        hashes,
        imported_packages: entry.map(|e| e.imported.clone()).unwrap_or_default(),
        exports,
        bundles,
        bundle_entries,
        bulk,
    })
}

fn parse_ue4(d: &[u8]) -> Result<ZenHeader> {
    let mut r = Reader::new(d);
    let name = mapped(&mut r)?;
    r.skip(8)?;
    let flags = r.u32()?;
    let cooked_header_size = r.u32()? as u64;
    let names_at = r.i32()? as usize;
    r.i32()?;
    r.i32()?;
    let hashes_size = r.i32()?.max(0) as usize;
    let imports_at = r.i32()? as usize;
    let exports_at = r.i32()? as usize;
    let bundles_at = r.i32()? as usize;
    let graph_at = r.i32()? as usize;
    let graph_size = r.i32()?.max(0) as usize;
    r.pos = names_at;
    let names = load_name_batch_old(&mut r, (hashes_size / 8).saturating_sub(1))?;
    r.pos = imports_at;
    let imports = u64s(&mut r, span(imports_at, exports_at, 8)?)?;
    let exports = read_exports(&mut r, exports_at, span(exports_at, bundles_at, EXPORT_ENTRY_SIZE)?)?;
    r.pos = bundles_at;
    let mut remaining = span(bundles_at, graph_at, 8)?;
    let mut found = 0usize;
    let mut headers = Vec::new();
    while found < remaining {
        remaining -= 1;
        let first = r.u32()?;
        let count = r.u32()?;
        found = found.saturating_add(count as usize);
        headers.push((first, count));
    }
    if found != remaining {
        return Err("invalid IoStore export bundles".to_string());
    }
    let bundle_entries = read_bundle_entries(&mut r, found)?;
    r.pos = graph_at;
    let package_count = r.count(12)?;
    let mut imported_packages = Vec::with_capacity(package_count);
    for _ in 0..package_count {
        imported_packages.push(r.i64()? as u64);
        let arcs = r.count(8)?;
        r.skip(arcs * 8)?;
    }
    let name = if name.global { "None".to_string() } else { names.get(name.index as usize).map_or_else(|| "None".to_string(), |b| name_text(b, name.number)) };
    Ok(ZenHeader {
        ue5: false,
        name,
        names,
        flags,
        cooked_header_size,
        data_start: graph_at.saturating_add(graph_size),
        imports,
        hashes: Vec::new(),
        imported_packages,
        exports,
        bundles: Some(headers),
        bundle_entries,
        bulk: None,
    })
}

fn parse_header(d: &[u8], ue5: bool, entry: Option<&StoreEntry>) -> Result<ZenHeader> {
    if d.len() < UE4_SUMMARY_SIZE {
        return Err("IoStore package header is truncated".to_string());
    }
    if ue5 { parse_ue5(d, entry) } else { parse_ue4(d) }
}

impl ZenHeader {
    fn export_name(&self, global: &GlobalData, index: usize) -> String {
        self.exports.get(index).map_or_else(|| "None".to_string(), |e| self.name_of(global, e.name))
    }

    fn name_of(&self, global: &GlobalData, name: MappedName) -> String {
        if name.global {
            return global.name(name);
        }
        self.names.get(name.index as usize).map_or_else(|| "None".to_string(), |b| name_text(b, name.number))
    }

    fn layout(&self) -> Vec<Option<(usize, u64)>> {
        let chs = self.cooked_header_size;
        let mut out: Vec<Option<(usize, u64)>> = vec![None; self.exports.len()];
        let Some(bundles) = &self.bundles else {
            for (slot, e) in out.iter_mut().zip(&self.exports) {
                *slot = Some((self.data_start.saturating_add(e.cooked_offset as usize), chs.saturating_add(e.cooked_offset)));
            }
            return out;
        };
        let mut pos = self.data_start;
        for &(first, count) in bundles {
            for i in first as usize..(first as usize).saturating_add(count as usize) {
                let Some(&(index, command)) = self.bundle_entries.get(i) else { break };
                let Some(e) = self.exports.get(index as usize).filter(|_| command == COMMAND_SERIALIZE) else { continue };
                let dest = if e.cooked_offset >= chs { e.cooked_offset } else { chs + (pos - self.data_start) as u64 };
                out[index as usize] = Some((pos, dest));
                pos = pos.saturating_add(e.size as usize);
            }
        }
        out
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }

    fn slot(&mut self) -> usize {
        self.i32(0);
        self.0.len() - 4
    }

    fn patch(&mut self, at: usize) {
        let here = self.0.len() as i32;
        self.0[at..at + 4].copy_from_slice(&here.to_le_bytes());
    }

    fn fstring(&mut self, s: &str) {
        if s.is_empty() {
            self.i32(0);
        } else if s.chars().all(|c| (c as u32) < 0x100) {
            self.i32(s.chars().count() as i32 + 1);
            self.0.extend(s.chars().map(|c| c as u8));
            self.0.push(0);
        } else {
            let units: Vec<u16> = s.encode_utf16().collect();
            self.i32(-(units.len() as i32 + 1));
            units.iter().for_each(|u| self.0.extend_from_slice(&u.to_le_bytes()));
            self.0.extend_from_slice(&[0, 0]);
        }
    }

    fn name(&mut self, name: (i32, u32)) {
        self.i32(name.0);
        self.u32(name.1);
    }

    fn engine_version(&mut self) {
        self.zeros(10);
        self.i32(0);
    }
}

struct Import {
    class: (i32, u32),
    outer: i32,
    name: (i32, u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Object(u64),
    Export(u64, usize),
    Package(u64),
}

struct Builder<'a> {
    set: &'a IoStoreSet,
    pkg: &'a ZenHeader,
    names: Vec<String>,
    interned: HashMap<String, i32>,
    imports: Vec<Option<Import>>,
    slots: HashMap<Key, usize>,
}

impl<'a> Builder<'a> {
    fn intern(&mut self, text: &str) -> (i32, u32) {
        if let Some(&i) = self.interned.get(text) {
            return (i, 0);
        }
        self.names.push(text.to_string());
        let i = self.names.len() as i32 - 1;
        self.interned.insert(text.to_string(), i);
        (i, 0)
    }

    fn local(&mut self, name: MappedName) -> (i32, u32) {
        if !name.global && (name.index as usize) < self.pkg.names.len() {
            return (name.index as i32, name.number);
        }
        let text = self.pkg.name_of(&self.set.global, name);
        self.intern(&text)
    }

    fn unknown(&mut self) -> Import {
        Import { class: self.intern("Object"), outer: 0, name: self.intern("None") }
    }

    fn import(&mut self, key: Key, depth: usize) -> i32 {
        let slot = match self.slots.get(&key) {
            Some(&s) if self.imports[s].is_some() => return -(s as i32) - 1,
            Some(&s) => s,
            None => {
                self.imports.push(None);
                let s = self.imports.len() - 1;
                self.slots.insert(key, s);
                s
            }
        };
        let import = if depth > MAX_OUTER_DEPTH { self.unknown() } else { self.build(key, depth + 1) };
        self.imports[slot] = Some(import);
        -(slot as i32) - 1
    }

    fn build(&mut self, key: Key, depth: usize) -> Import {
        let set = self.set;
        match key {
            Key::Package(id) => {
                let name = set.packages.get(&id).map_or("None", |p| p.header.name.as_str());
                Import { class: self.intern("Package"), outer: 0, name: self.intern(name) }
            }
            Key::Export(id, index) => {
                let Some(target) = set.packages.get(&id).map(|p| &p.header) else { return self.unknown() };
                let Some(export) = target.exports.get(index) else { return self.unknown() };
                let name = target.export_name(&set.global, index);
                let class = set.object_name(target, export.class);
                let outer = match (export.outer, export.outer >> 62) {
                    (NULL_INDEX, _) => self.import(Key::Package(id), depth),
                    (outer, TYPE_EXPORT) => self.import(Key::Export(id, (outer & INDEX_MASK) as usize), depth),
                    _ => self.import(Key::Package(id), depth),
                };
                Import { class: self.intern(&class), outer, name: self.intern(&name) }
            }
            Key::Object(NULL_INDEX) => self.unknown(),
            Key::Object(index) => match index >> 62 {
                TYPE_SCRIPT => {
                    let Some(script) = set.global.scripts.get(&index) else { return self.unknown() };
                    let name = set.global.name(script.name);
                    let (class, outer) = if script.outer == NULL_INDEX {
                        ("Package".to_string(), 0)
                    } else {
                        let class = if script.cdo_class == NULL_INDEX { "Class".to_string() } else { set.script_name(script.cdo_class) };
                        (class, self.import(Key::Object(script.outer), depth))
                    };
                    Import { class: self.intern(&class), outer, name: self.intern(&name) }
                }
                TYPE_PACKAGE => match set.resolve_import(self.pkg, index) {
                    Some((id, export)) => self.build(Key::Export(id, export), depth),
                    None => self.unknown(),
                },
                _ => self.unknown(),
            },
        }
    }

    fn index(&mut self, index: u64) -> i32 {
        match (index, index >> 62) {
            (NULL_INDEX, _) => 0,
            (index, TYPE_EXPORT) => (index & INDEX_MASK) as i32 + 1,
            (index, _) => self.import(Key::Object(index), 0),
        }
    }
}

impl IoStoreSet {
    fn script_name(&self, index: u64) -> String {
        self.global.scripts.get(&index).map_or_else(|| "None".to_string(), |s| self.global.name(s.name))
    }

    fn resolve_import(&self, pkg: &ZenHeader, index: u64) -> Option<(u64, usize)> {
        if !pkg.ue5 {
            return self.global_exports.get(&index).copied();
        }
        let package = *pkg.imported_packages.get(((index & INDEX_MASK) >> 32) as usize)?;
        let hash = *pkg.hashes.get(index as u32 as usize)?;
        let target = &self.packages.get(&package)?.header;
        target.exports.iter().position(|e| e.key == hash).map(|export| (package, export))
    }

    fn object_name(&self, pkg: &ZenHeader, index: u64) -> String {
        match (index, index >> 62) {
            (NULL_INDEX, _) => "None".to_string(),
            (index, TYPE_EXPORT) => pkg.export_name(&self.global, (index & INDEX_MASK) as usize),
            (index, TYPE_SCRIPT) => self.script_name(index),
            (index, TYPE_PACKAGE) => self
                .resolve_import(pkg, index)
                .and_then(|(id, export)| self.packages.get(&id).map(|p| p.header.export_name(&self.global, export)))
                .unwrap_or_else(|| "None".to_string()),
            _ => "None".to_string(),
        }
    }

    pub fn open(utocs: &[PathBuf]) -> Result<IoStoreSet> {
        let mut paths = utocs.to_vec();
        for dir in utocs.iter().filter_map(|p| p.parent()) {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for path in entries.flatten().map(|e| e.path()) {
                let global = path.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("global"));
                if global && is_utoc(&path) && !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths.dedup();
        let stores = paths.iter().map(|p| IoStore::open(p)).collect::<Result<Vec<_>>>()?;
        let global = stores.iter().find_map(|s| GlobalData::load(s).ok()).ok_or(GLOBAL_ERROR)?;
        let mut entries = HashMap::new();
        for store in &stores {
            if let Some(data) = store.find(store.container_id, ChunkKind::ContainerHeader).and_then(|chunk| store.read(chunk).ok()) {
                let _ = read_container_header(&data, store.ue5, &mut entries);
            }
        }
        let mut packages = HashMap::new();
        for (index, store) in stores.iter().enumerate() {
            let kind = ChunkKind::ExportBundleData.code(store.ue5);
            for (chunk, id) in store.chunks.iter().enumerate().filter(|(_, c)| c.kind == kind && c.index == 0) {
                let header = store
                    .read_range(chunk, 0, UE4_SUMMARY_SIZE)
                    .ok()
                    .and_then(|prefix| header_length(&prefix, store.ue5))
                    .and_then(|len| store.read_range(chunk, 0, len.max(UE4_SUMMARY_SIZE)).ok())
                    .and_then(|data| parse_header(&data, store.ue5, entries.get(&id.id)).ok());
                if let Some(header) = header {
                    packages.insert(id.id, PackageRef { store: index, header });
                }
            }
        }
        let mut global_exports = HashMap::new();
        for (&id, package) in packages.iter().filter(|(_, p)| !p.header.ue5) {
            for (index, export) in package.header.exports.iter().enumerate().filter(|(_, e)| e.key != NULL_INDEX) {
                global_exports.insert(export.key, (id, index));
            }
        }
        Ok(IoStoreSet { stores, global, entries, packages, global_exports })
    }

    pub fn package_count(&self) -> usize {
        self.packages.len()
    }

    pub fn entries(&self, store: usize) -> Vec<IoEntry> {
        let Some(s) = self.stores.get(store) else { return Vec::new() };
        let package_kind = ChunkKind::ExportBundleData.code(s.ue5);
        let wanted = |chunk: usize| s.chunks.get(chunk).is_some_and(|c| c.kind != package_kind || c.index == 0);
        if !s.files.is_empty() {
            return s
                .files
                .iter()
                .filter(|f| crate::is_unreal_file(&f.path) && wanted(f.chunk))
                .filter_map(|f| Some(IoEntry { store, path: s.relative_path(&f.path)?, chunk: f.chunk }))
                .collect();
        }
        let mut out = Vec::new();
        for (&id, package) in self.packages.iter().filter(|(_, p)| p.store == store) {
            let base = package.header.name.trim_start_matches('/');
            if base.is_empty() || base == "None" || !Path::new(base).components().all(|c| matches!(c, std::path::Component::Normal(_))) {
                continue;
            }
            for (kind, ext) in [(ChunkKind::ExportBundleData, "uasset"), (ChunkKind::BulkData, "ubulk"), (ChunkKind::OptionalBulkData, "uptnl")] {
                if let Some(chunk) = s.find(id, kind) {
                    out.push(IoEntry { store, path: PathBuf::from(format!("{}.{}", base, ext)), chunk });
                }
            }
        }
        out
    }

    pub fn extract(&self, entry: &IoEntry) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let store = self.stores.get(entry.store).ok_or("invalid IoStore container")?;
        let data = store.read(entry.chunk)?;
        let id = store.chunks.get(entry.chunk).ok_or("invalid IoStore chunk")?;
        if id.kind != ChunkKind::ExportBundleData.code(store.ue5) {
            return Ok(vec![(entry.path.clone(), data)]);
        }
        let header = parse_header(&data, store.ue5, self.entries.get(&id.id)).map_err(|e| format!("{}: {}", entry.path.display(), e))?;
        let (uasset, uexp) = self.convert(&header, &data);
        let uasset_path = if entry.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("umap")) { entry.path.clone() } else { entry.path.with_extension("uasset") };
        Ok(vec![(entry.path.with_extension("uexp"), uexp), (uasset_path, uasset)])
    }

    fn convert(&self, pkg: &ZenHeader, data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut b = Builder {
            set: self,
            pkg,
            names: pkg.names.clone(),
            interned: pkg.names.iter().enumerate().rev().map(|(i, n)| (n.clone(), i as i32)).collect(),
            imports: (0..pkg.imports.len()).map(|_| None).collect(),
            slots: HashMap::new(),
        };
        for (slot, &index) in pkg.imports.iter().enumerate() {
            if index != NULL_INDEX {
                b.slots.entry(Key::Object(index)).or_insert(slot);
            }
        }
        for slot in 0..pkg.imports.len() {
            if b.imports[slot].is_none() {
                let import = b.build(Key::Object(pkg.imports[slot]), 0);
                b.imports[slot] = Some(import);
            }
        }
        let layout = pkg.layout();
        let chs = pkg.cooked_header_size;
        let mut uexp = Vec::new();
        let mut source_end = pkg.data_start;
        let mut exports = Vec::with_capacity(pkg.exports.len());
        for (export, place) in pkg.exports.iter().zip(&layout) {
            let class = b.index(export.class);
            let super_index = b.index(export.super_index);
            let template = b.index(export.template);
            let outer = b.index(export.outer);
            let name = b.local(export.name);
            let mut offset = chs as i64;
            let mut size = 0i64;
            if let Some((src, dest)) = *place {
                let bytes = data.get(src..src.saturating_add(export.size as usize)).unwrap_or_default();
                let at = (dest - chs) as usize;
                if uexp.len() < at + bytes.len() {
                    uexp.resize(at + bytes.len(), 0);
                }
                uexp[at..at + bytes.len()].copy_from_slice(bytes);
                source_end = source_end.max(src + bytes.len());
                offset = dest as i64;
                size = bytes.len() as i64;
            }
            exports.push((class, super_index, template, outer, name, export.flags, size, offset));
        }
        let bulk_start = chs as i64 + uexp.len() as i64;
        uexp.extend_from_slice(data.get(source_end..).unwrap_or_default());
        let imports: Vec<Import> = b.imports.drain(..).map(|i| i.unwrap_or(Import { class: (0, 0), outer: 0, name: (0, 0) })).collect();
        let class_package = b.intern("/Script/CoreUObject");
        let ue5 = pkg.ue5;
        let mut w = Writer(Vec::new());
        w.u32(PACKAGE_TAG);
        w.i32(if ue5 { -8 } else { -7 });
        w.i32(864);
        w.i32(0);
        if ue5 {
            w.i32(0);
        }
        w.i32(0);
        w.i32(0);
        w.i32(chs as i32);
        w.fstring("None");
        w.u32(pkg.flags);
        w.i32(b.names.len() as i32);
        let names_at = w.slot();
        if ue5 {
            w.zeros(8);
        }
        if pkg.flags & PKG_FILTER_EDITOR_ONLY == 0 {
            w.fstring("");
        }
        w.zeros(8);
        w.i32(exports.len() as i32);
        let exports_at = w.slot();
        w.i32(imports.len() as i32);
        let imports_at = w.slot();
        let depends_at = w.slot();
        w.zeros(32);
        w.i32(1);
        w.i32(exports.len() as i32);
        w.i32(b.names.len() as i32);
        w.engine_version();
        w.engine_version();
        w.zeros(20);
        w.i64(bulk_start);
        let mut resources_at = None;
        if ue5 {
            w.zeros(28);
            if pkg.bulk.is_some() {
                resources_at = Some(w.slot());
            }
        }
        w.patch(names_at);
        for name in &b.names {
            w.fstring(name);
            w.u32(0);
        }
        w.patch(imports_at);
        for import in &imports {
            w.name(class_package);
            w.name(import.class);
            w.i32(import.outer);
            w.name(import.name);
        }
        w.patch(exports_at);
        for &(class, super_index, template, outer, name, flags, size, offset) in &exports {
            let start = w.0.len();
            w.i32(class);
            w.i32(super_index);
            if ue5 {
                w.i32(template);
            }
            w.i32(outer);
            w.name(name);
            w.u32(flags);
            w.i64(size);
            w.i64(offset);
            let entry = if ue5 { UE5_EXPORT_SIZE } else { UE4_EXPORT_SIZE };
            w.zeros(entry - (w.0.len() - start));
        }
        w.patch(depends_at);
        if let (Some(at), Some(bulk)) = (resources_at, &pkg.bulk) {
            w.patch(at);
            w.u32(1);
            w.i32(bulk.len() as i32);
            for entry in bulk {
                w.u32(0);
                w.i64(entry.offset);
                w.i64(entry.offset);
                w.i64(entry.size);
                w.i64(entry.size);
                w.i32(0);
                w.u32(entry.flags);
            }
        }
        (w.0, uexp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::Package;

    const ENGINE: u64 = (1 << 62) | 4;
    const STATIC_MESH: u64 = (1 << 62) | 5;

    fn set() -> IoStoreSet {
        let mut scripts = HashMap::new();
        scripts.insert(ENGINE, ScriptObject { name: MappedName { index: 0, number: 0, global: true }, outer: NULL_INDEX, cdo_class: NULL_INDEX });
        scripts.insert(STATIC_MESH, ScriptObject { name: MappedName { index: 1, number: 0, global: true }, outer: ENGINE, cdo_class: NULL_INDEX });
        let global = GlobalData { names: vec!["/Script/Engine".to_string(), "StaticMesh".to_string()], scripts };
        IoStoreSet { stores: Vec::new(), global, entries: HashMap::new(), packages: HashMap::new(), global_exports: HashMap::new() }
    }

    fn zen_ue5() -> Vec<u8> {
        let names = ["/Game/Mesh", "Mesh", "None"];
        let mut batch = Vec::new();
        batch.extend_from_slice(&(names.len() as i32).to_le_bytes());
        batch.extend_from_slice(&(names.iter().map(|n| n.len()).sum::<usize>() as u32).to_le_bytes());
        batch.extend_from_slice(&NAME_HASH_VERSION.to_le_bytes());
        batch.extend(std::iter::repeat_n(0u8, names.len() * 8));
        for n in names {
            batch.extend_from_slice(&[(n.len() >> 8) as u8, n.len() as u8]);
        }
        for n in names {
            batch.extend_from_slice(n.as_bytes());
        }
        let mut bulk = Vec::new();
        bulk.extend_from_slice(&32i64.to_le_bytes());
        bulk.extend_from_slice(&0i64.to_le_bytes());
        bulk.extend_from_slice(&0i64.to_le_bytes());
        bulk.extend_from_slice(&16i64.to_le_bytes());
        bulk.extend_from_slice(&0x100u32.to_le_bytes());
        bulk.extend_from_slice(&[0; 4]);
        let hashes_at = UE5_SUMMARY_SIZE + batch.len() + bulk.len();
        let imports_at = hashes_at;
        let exports_at = imports_at + 8;
        let entries_at = exports_at + EXPORT_ENTRY_SIZE;
        let mut d = Vec::new();
        for v in [0u32, entries_at as u32, 0, 0, 0x8000_2000, 1000, hashes_at as u32, imports_at as u32, exports_at as u32, entries_at as u32, 0, 0, 0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend(batch);
        d.extend(bulk);
        d.extend_from_slice(&STATIC_MESH.to_le_bytes());
        for v in [0u64, 4] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        for v in [NULL_INDEX, STATIC_MESH, NULL_INDEX, NULL_INDEX, 0x1234] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&[0; 8]);
        d.extend_from_slice(&[1, 2, 3, 4]);
        d
    }

    #[test]
    fn converts_ue5_zen_package() {
        let data = zen_ue5();
        let header = parse_header(&data, true, None).unwrap();
        assert_eq!(header.name, "/Game/Mesh");
        let (uasset, uexp) = set().convert(&header, &data);
        let pkg = Package::parse("Mesh.uasset", &uasset, uexp, None).unwrap();
        assert!(pkg.is_ue5 && pkg.unversioned_properties);
        assert_eq!(pkg.header_size, 1000);
        let export = &pkg.exports[0];
        assert_eq!(export.object_name, "Mesh");
        assert_eq!(pkg.export_class_name(export), Some("StaticMesh"));
        assert_eq!(pkg.import_path(export.class_index).as_deref(), Some("/Script/Engine.StaticMesh"));
        assert_eq!(pkg.export_data(export).unwrap(), &[1, 2, 3, 4]);
        let resources = pkg.data_resources.as_ref().unwrap();
        assert_eq!((resources[0].serial_size, resources[0].legacy_flags), (16, 0x100));
    }

    #[test]
    fn converts_ue4_zen_package() {
        let export = |cooked_offset, size, index| ZenExport {
            cooked_offset,
            size,
            name: MappedName { index, number: 0, global: false },
            outer: NULL_INDEX,
            class: STATIC_MESH,
            super_index: NULL_INDEX,
            template: NULL_INDEX,
            key: NULL_INDEX,
            flags: 0,
        };
        let header = ZenHeader {
            ue5: false,
            name: "/Game/Mesh".to_string(),
            names: vec!["/Game/Mesh".to_string(), "A".to_string(), "B".to_string()],
            flags: 0x8000_2000,
            cooked_header_size: 500,
            data_start: 4,
            imports: vec![STATIC_MESH],
            hashes: Vec::new(),
            imported_packages: Vec::new(),
            exports: vec![export(503, 2, 1), export(500, 3, 2)],
            bundles: Some(vec![(0, 4)]),
            bundle_entries: vec![(0, 0), (1, 0), (1, 1), (0, 1)],
            bulk: None,
        };
        let data = [0, 0, 0, 0, 7, 8, 9, 5, 6];
        let (uasset, uexp) = set().convert(&header, &data);
        let pkg = Package::parse("Mesh.uasset", &uasset, uexp, None).unwrap();
        assert!(!pkg.is_ue5 && pkg.data_resources.is_none());
        assert_eq!(pkg.export_data(&pkg.exports[0]).unwrap(), &[5, 6]);
        assert_eq!(pkg.export_data(&pkg.exports[1]).unwrap(), &[7, 8, 9]);
        assert_eq!(pkg.export_class_name(&pkg.exports[1]), Some("StaticMesh"));
    }
}
