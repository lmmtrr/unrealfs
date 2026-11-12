use crate::pak::{KEY_ERROR, aligned, current_key, decompress, decrypt};
use crate::reader::{Reader, Result};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

const TOC_MAGIC: &[u8; 16] = b"-==--==--==--==-";
const VERSION_DIRECTORY_INDEX: u8 = 2;
const VERSION_PARTITION_SIZE: u8 = 3;
const VERSION_PERFECT_HASH: u8 = 4;
const VERSION_PERFECT_HASH_WITH_OVERFLOW: u8 = 5;
const VERSION_ENCRYPTION_METHOD: u8 = 10;
const FLAG_ENCRYPTED: u8 = 1 << 1;
const FLAG_SIGNED: u8 = 1 << 2;
const FLAG_INDEXED: u8 = 1 << 3;
const NO_ENTRY: u32 = u32::MAX;
const BLOCK_ENTRY_SIZE: usize = 12;
const SHA_SIZE: usize = 20;
pub const NOT_UTOC_ERROR: &str = "not an IoStore .utoc file";
const WRONG_KEY_ERROR: &str = "could not read the .utoc directory index; the AES key may be wrong";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkId {
    pub id: u64,
    pub index: u16,
    pub kind: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    ExportBundleData,
    BulkData,
    OptionalBulkData,
    MemoryMappedBulkData,
    ScriptObjects,
    ContainerHeader,
    LoaderInitialLoadMeta,
    LoaderGlobalNames,
    LoaderGlobalNameHashes,
}

impl ChunkKind {
    pub fn code(self, ue5: bool) -> u8 {
        match (self, ue5) {
            (ChunkKind::ExportBundleData, true) => 1,
            (ChunkKind::BulkData, true) => 2,
            (ChunkKind::OptionalBulkData, true) => 3,
            (ChunkKind::MemoryMappedBulkData, true) => 4,
            (ChunkKind::ScriptObjects, _) => 5,
            (ChunkKind::ContainerHeader, true) => 6,
            (ChunkKind::ExportBundleData, false) => 2,
            (ChunkKind::BulkData, false) => 3,
            (ChunkKind::OptionalBulkData, false) => 4,
            (ChunkKind::MemoryMappedBulkData, false) => 5,
            (ChunkKind::LoaderInitialLoadMeta, _) => 7,
            (ChunkKind::LoaderGlobalNames, _) => 8,
            (ChunkKind::LoaderGlobalNameHashes, _) => 9,
            (ChunkKind::ContainerHeader, false) => 10,
        }
    }
}

struct Block {
    offset: u64,
    compressed: usize,
    uncompressed: usize,
    method: usize,
}

pub struct IoFile {
    pub path: String,
    pub chunk: usize,
}

pub struct IoStore {
    pub path: PathBuf,
    cas: Vec<Mmap>,
    key: Option<[u8; 32]>,
    encrypted: bool,
    pub ue5: bool,
    pub container_id: u64,
    block_size: u64,
    partition_size: u64,
    pub chunks: Vec<ChunkId>,
    ranges: Vec<(u64, u64)>,
    blocks: Vec<Block>,
    methods: Vec<String>,
    lookup: HashMap<ChunkId, usize>,
    pub mount_point: String,
    pub files: Vec<IoFile>,
}

pub fn is_utoc(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("utoc"))
}

fn partition_path(utoc: &Path, partition: usize) -> PathBuf {
    if partition == 0 {
        return utoc.with_extension("ucas");
    }
    let stem = utoc.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    utoc.with_file_name(format!("{}_s{}.ucas", stem, partition))
}

fn map_file(path: &Path) -> Result<Mmap> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {}", path.display(), e))
}

fn chunk_id(r: &mut Reader) -> Result<ChunkId> {
    let id = r.i64()? as u64;
    let index = r.bytes(2)?;
    let index = u16::from_be_bytes([index[0], index[1]]);
    r.u8()?;
    let kind = r.u8()?;
    Ok(ChunkId { id, index, kind })
}

fn be40(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |acc, &v| (acc << 8) | v as u64)
}

fn read_directory_index(index: &[u8], chunk_count: usize) -> Result<(String, Vec<IoFile>)> {
    let mut r = Reader::new(index);
    let mount_point = r.fstring()?;
    let dir_count = r.count(16)?;
    let dirs = (0..dir_count).map(|_| Ok([r.u32()?, r.u32()?, r.u32()?, r.u32()?])).collect::<Result<Vec<_>>>()?;
    let file_count = r.count(12)?;
    let files = (0..file_count).map(|_| Ok([r.u32()?, r.u32()?, r.u32()?])).collect::<Result<Vec<_>>>()?;
    let string_count = r.count(4)?;
    let strings = (0..string_count).map(|_| r.fstring()).collect::<Result<Vec<_>>>()?;
    let name = |i: u32| -> Result<&str> { strings.get(i as usize).map(String::as_str).ok_or_else(|| "the .utoc directory index is out of range".to_string()) };
    let mut out = Vec::with_capacity(file_count);
    if dirs.is_empty() {
        return Ok((mount_point, out));
    }
    let mut pending = vec![(0u32, String::new())];
    let mut visited = 0;
    while let Some((dir, parent)) = pending.pop() {
        visited += 1;
        if visited > dirs.len() {
            return Err("the .utoc directory index has a cycle".to_string());
        }
        let entry = dirs.get(dir as usize).ok_or("the .utoc directory index is out of range")?;
        let mut path = parent.clone();
        if entry[0] != NO_ENTRY {
            let dir_name = name(entry[0])?;
            if !dir_name.is_empty() {
                path = format!("{}{}/", path, dir_name);
            }
        }
        if entry[2] != NO_ENTRY {
            pending.push((entry[2], parent));
        }
        if entry[1] != NO_ENTRY {
            pending.push((entry[1], path.clone()));
        }
        let mut file = entry[3];
        let mut steps = 0;
        while file != NO_ENTRY {
            steps += 1;
            let f = files.get(file as usize).filter(|_| steps <= files.len()).ok_or("the .utoc directory index is out of range")?;
            if (f[2] as usize) < chunk_count {
                out.push(IoFile { path: format!("{}{}", path, name(f[0])?), chunk: f[2] as usize });
            }
            file = f[1];
        }
    }
    Ok((mount_point, out))
}

impl IoStore {
    pub fn open(utoc: &Path) -> Result<IoStore> {
        let toc = std::fs::read(utoc).map_err(|e| format!("{}: {}", utoc.display(), e))?;
        if toc.len() < 144 || &toc[..16] != TOC_MAGIC {
            return Err(NOT_UTOC_ERROR.to_string());
        }
        let mut r = Reader::new(&toc);
        r.pos = 16;
        let version = r.u8()?;
        r.skip(3)?;
        let header_size = r.u32()? as usize;
        let entry_count = r.u32()? as usize;
        let block_count = r.u32()? as usize;
        let block_entry_size = r.u32()? as usize;
        let method_count = r.u32()? as usize;
        let method_length = r.u32()? as usize;
        let block_size = r.u32()? as u64;
        let directory_size = r.u32()? as usize;
        let mut partition_count = r.u32()? as usize;
        let container_id = r.i64()? as u64;
        r.skip(16)?;
        let flags = r.u8()?;
        let encryption_method = r.u8()?;
        r.skip(2)?;
        let seed_count = r.u32()? as usize;
        let mut partition_size = r.i64()? as u64;
        let without_hash_count = r.u32()? as usize;
        r.skip(4)?;
        let iv_count = if version >= VERSION_ENCRYPTION_METHOD { r.u32()? } else { 0 };
        if block_entry_size != BLOCK_ENTRY_SIZE || block_size == 0 {
            return Err(format!("unsupported .utoc layout (block entry size {})", block_entry_size));
        }
        if iv_count > 0 || (version >= VERSION_ENCRYPTION_METHOD && encryption_method > 1) {
            return Err("this .utoc uses an unsupported encryption method".to_string());
        }
        if version < VERSION_PARTITION_SIZE {
            partition_count = 1;
            partition_size = u64::MAX;
        }
        if partition_count == 0 || partition_size == 0 {
            return Err("this .utoc has no container partitions".to_string());
        }
        r.pos = header_size;
        if entry_count.saturating_mul(22) > r.remaining() {
            return Err(format!("invalid .utoc entry count {}", entry_count));
        }
        let chunks = (0..entry_count).map(|_| chunk_id(&mut r)).collect::<Result<Vec<_>>>()?;
        let ranges = (0..entry_count)
            .map(|_| {
                let b = r.bytes(10)?;
                Ok((be40(&b[..5]), be40(&b[5..])))
            })
            .collect::<Result<Vec<_>>>()?;
        let seeds = if version >= VERSION_PERFECT_HASH { seed_count } else { 0 };
        let without = if version >= VERSION_PERFECT_HASH_WITH_OVERFLOW { without_hash_count } else { 0 };
        r.skip(seeds.saturating_add(without).saturating_mul(4))?;
        if block_count.saturating_mul(BLOCK_ENTRY_SIZE) > r.remaining() {
            return Err(format!("invalid .utoc block count {}", block_count));
        }
        let blocks = (0..block_count)
            .map(|_| {
                let a = r.i64()? as u64;
                let b = r.u32()?;
                Ok(Block {
                    offset: a & ((1 << 40) - 1),
                    compressed: ((a >> 40) & 0xFF_FFFF) as usize,
                    uncompressed: (b & 0xFF_FFFF) as usize,
                    method: (b >> 24) as usize,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut methods = vec![String::new()];
        for _ in 0..method_count {
            let raw = r.bytes(method_length)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            methods.push(String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase());
        }
        if flags & FLAG_SIGNED != 0 {
            let hash_size = r.count(1)?;
            r.skip(hash_size.saturating_mul(2))?;
            r.skip(block_count.saturating_mul(SHA_SIZE))?;
        }
        let encrypted = flags & FLAG_ENCRYPTED != 0;
        let key = current_key();
        let mut mount_point = String::new();
        let mut files = Vec::new();
        if version >= VERSION_DIRECTORY_INDEX && flags & FLAG_INDEXED != 0 && directory_size > 0 {
            let mut index = toc.get(r.pos..r.pos.saturating_add(directory_size)).ok_or("the .utoc directory index lies outside the file")?.to_vec();
            if encrypted {
                let key = key.as_ref().ok_or(KEY_ERROR)?;
                let whole = index.len() / 16 * 16;
                decrypt(&mut index[..whole], key);
            }
            (mount_point, files) = read_directory_index(&index, entry_count).map_err(|e| if encrypted { WRONG_KEY_ERROR.to_string() } else { e })?;
        }
        let cas = (0..partition_count).map(|i| map_file(&partition_path(utoc, i))).collect::<Result<Vec<_>>>()?;
        let lookup = chunks.iter().enumerate().map(|(i, c)| (*c, i)).collect::<HashMap<_, _>>();
        let has = |id: u64, kind: u8| lookup.contains_key(&ChunkId { id, index: 0, kind });
        let ue5 = if has(container_id, 6) || has(0, 5) {
            true
        } else if has(container_id, 10) || has(0, 8) {
            false
        } else {
            version >= VERSION_PERFECT_HASH
        };
        Ok(IoStore {
            path: utoc.to_path_buf(),
            cas,
            key,
            encrypted,
            ue5,
            container_id,
            block_size,
            partition_size,
            chunks,
            ranges,
            blocks,
            methods,
            lookup,
            mount_point,
            files,
        })
    }

    pub fn find(&self, id: u64, kind: ChunkKind) -> Option<usize> {
        self.lookup.get(&ChunkId { id, index: 0, kind: kind.code(self.ue5) }).copied()
    }

    pub fn chunk_size(&self, chunk: usize) -> usize {
        self.ranges.get(chunk).map_or(0, |r| r.1 as usize)
    }

    pub fn relative_path(&self, path: &str) -> Option<PathBuf> {
        let mount = self.mount_point.trim_start_matches("../").trim_start_matches('/');
        let joined = format!("{}{}", mount, path.trim_start_matches('/'));
        let path = Path::new(&joined);
        path.components().all(|c| matches!(c, Component::Normal(_))).then(|| path.to_path_buf())
    }

    fn block(&self, index: usize) -> Result<Vec<u8>> {
        let block = self.blocks.get(index).ok_or("a .utoc chunk refers to a missing block")?;
        let raw_size = if self.encrypted { aligned(block.compressed) } else { block.compressed };
        let partition = (block.offset / self.partition_size) as usize;
        let start = (block.offset % self.partition_size) as usize;
        let cas = self.cas.get(partition).ok_or("a .utoc block lies in a missing .ucas partition")?;
        let mut raw = cas.get(start..start.saturating_add(raw_size)).ok_or("a .utoc block lies outside the .ucas")?.to_vec();
        if self.encrypted {
            decrypt(&mut raw, self.key.as_ref().ok_or(KEY_ERROR)?);
        }
        raw.truncate(block.compressed);
        if block.method == 0 {
            return Ok(raw);
        }
        let codec = self.methods.get(block.method).ok_or_else(|| format!("unknown .utoc compression method {}", block.method))?;
        let out = decompress(codec, &raw, block.uncompressed)?;
        if out.len() != block.uncompressed {
            return Err(format!("a .utoc block decompressed to {} bytes, expected {}", out.len(), block.uncompressed));
        }
        Ok(out)
    }

    pub fn read_range(&self, chunk: usize, start: usize, len: usize) -> Result<Vec<u8>> {
        let &(offset, size) = self.ranges.get(chunk).ok_or("a .utoc chunk index is out of range")?;
        let len = len.min((size as usize).saturating_sub(start));
        let mut out = Vec::with_capacity(len);
        if len == 0 {
            return Ok(out);
        }
        let begin = offset + start as u64;
        let first = (begin / self.block_size) as usize;
        let last = ((begin + len as u64 - 1) / self.block_size) as usize;
        let mut skip = (begin % self.block_size) as usize;
        for index in first..=last {
            let data = self.block(index)?;
            let take = (len - out.len()).min(data.len().saturating_sub(skip));
            out.extend_from_slice(data.get(skip..skip + take).ok_or("a .utoc block is shorter than expected")?);
            skip = 0;
        }
        if out.len() != len {
            return Err(format!("read {} bytes of a .utoc chunk, expected {}", out.len(), len));
        }
        Ok(out)
    }

    pub fn read(&self, chunk: usize) -> Result<Vec<u8>> {
        self.read_range(chunk, 0, usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fstring(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }

    #[test]
    fn reads_uncompressed_container() {
        let mut index = Vec::new();
        fstring(&mut index, "../../../");
        index.extend_from_slice(&2i32.to_le_bytes());
        for v in [NO_ENTRY, 1, NO_ENTRY, NO_ENTRY, 0, NO_ENTRY, NO_ENTRY, 0] {
            index.extend_from_slice(&v.to_le_bytes());
        }
        index.extend_from_slice(&1i32.to_le_bytes());
        for v in [1, NO_ENTRY, 0] {
            index.extend_from_slice(&v.to_le_bytes());
        }
        index.extend_from_slice(&2i32.to_le_bytes());
        fstring(&mut index, "Game");
        fstring(&mut index, "a.ubulk");
        let mut toc = TOC_MAGIC.to_vec();
        toc.extend_from_slice(&[VERSION_PERFECT_HASH_WITH_OVERFLOW, 0, 0, 0]);
        for v in [144u32, 1, 1, 12, 0, 32, 65536, index.len() as u32, 1] {
            toc.extend_from_slice(&v.to_le_bytes());
        }
        toc.extend_from_slice(&7u64.to_le_bytes());
        toc.extend_from_slice(&[0; 16]);
        toc.extend_from_slice(&[FLAG_INDEXED, 0, 0, 0]);
        toc.extend_from_slice(&0u32.to_le_bytes());
        toc.extend_from_slice(&u64::MAX.to_le_bytes());
        toc.extend_from_slice(&[0; 48]);
        assert_eq!(toc.len(), 144);
        toc.extend_from_slice(&42u64.to_le_bytes());
        toc.extend_from_slice(&[0, 0, 0, 2]);
        toc.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 5]);
        toc.extend_from_slice(&(5u64 << 40).to_le_bytes());
        toc.extend_from_slice(&5u32.to_le_bytes());
        toc.extend(index);
        let dir = std::env::temp_dir().join(format!("unrealfs-iostore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let utoc = dir.join("test.utoc");
        std::fs::write(&utoc, &toc).unwrap();
        std::fs::write(dir.join("test.ucas"), b"hello").unwrap();
        let store = IoStore::open(&utoc).unwrap();
        assert_eq!(store.files.len(), 1);
        assert_eq!(store.relative_path(&store.files[0].path).unwrap(), Path::new("Game/a.ubulk"));
        assert_eq!(store.read(store.files[0].chunk).unwrap(), b"hello");
        assert_eq!(store.read_range(0, 1, 3).unwrap(), b"ell");
        assert_eq!(store.find(42, ChunkKind::BulkData), Some(0));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
