use crate::reader::{Reader, Result};
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use aes::Aes256;
use memmap2::Mmap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

const MAGIC: u32 = 0x5A6F_12E1;
const HASH_SIZE: usize = 20;
const NAME_SLOT_SIZE: usize = 32;
const AES_BLOCK: usize = 16;
const VERSION_RELATIVE_OFFSETS: u32 = 5;
const VERSION_PATH_HASH_INDEX: u32 = 10;
const COMPRESSION_BLOCK_UNIT_BITS: u32 = 11;
const FROZEN_FOOTER_SIZE: usize = 222;
const BYTE_COMPRESSION_FOOTER_SIZE: usize = 189;
const LZ4_MAX_RATIO: usize = 255;
pub const NOT_PAK_ERROR: &str = "not an Unreal .pak file";
pub const KEY_ERROR: &str = "this .pak is encrypted; set its AES key in Settings";
pub const WRONG_KEY_ERROR: &str = "could not read the .pak index; the AES key may be wrong";

struct FooterLayout {
    size: usize,
    magic_at: usize,
    versions: (u32, u32),
    name_slots: usize,
}

const FOOTERS: [FooterLayout; 6] = [
    FooterLayout { size: FROZEN_FOOTER_SIZE, magic_at: 17, versions: (9, 9), name_slots: 5 },
    FooterLayout { size: 221, magic_at: 17, versions: (8, 11), name_slots: 5 },
    FooterLayout { size: BYTE_COMPRESSION_FOOTER_SIZE, magic_at: 17, versions: (8, 8), name_slots: 4 },
    FooterLayout { size: 61, magic_at: 17, versions: (7, 7), name_slots: 0 },
    FooterLayout { size: 45, magic_at: 1, versions: (4, 6), name_slots: 0 },
    FooterLayout { size: 44, magic_at: 0, versions: (1, 3), name_slots: 0 },
];

static AES_KEY: RwLock<Option<[u8; 32]>> = RwLock::new(None);

pub fn set_aes_key(text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        *AES_KEY.write().unwrap() = None;
        return Ok(());
    }
    *AES_KEY.write().unwrap() = Some(parse_key(text)?);
    Ok(())
}

fn parse_key(text: &str) -> Result<[u8; 32]> {
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the AES key must be 64 hexadecimal digits".to_string());
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(key)
}

pub(crate) fn current_key() -> Option<[u8; 32]> {
    *AES_KEY.read().unwrap()
}

pub(crate) fn decrypt(data: &mut [u8], key: &[u8; 32]) {
    let cipher = Aes256::new(&(*key).into());
    for chunk in data.chunks_exact_mut(AES_BLOCK) {
        let mut block = aes::Block::try_from(&*chunk).expect("chunk is one AES block");
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
}

pub(crate) fn aligned(size: usize) -> usize {
    size.div_ceil(AES_BLOCK) * AES_BLOCK
}

struct Footer {
    version: u32,
    encrypted: bool,
    index_offset: u64,
    index_size: u64,
    names: Vec<String>,
    byte_compression: bool,
}

fn read_footer(data: &[u8]) -> Result<Footer> {
    for layout in &FOOTERS {
        let Some(start) = data.len().checked_sub(layout.size) else { continue };
        let mut r = Reader::new(data);
        r.pos = start + layout.magic_at;
        if r.u32().ok() != Some(MAGIC) {
            continue;
        }
        let version = r.u32()?;
        if version < layout.versions.0 || version > layout.versions.1 {
            continue;
        }
        let encrypted = layout.magic_at > 0 && data[start + layout.magic_at - 1] != 0;
        let index_offset = r.i64()? as u64;
        let index_size = r.i64()? as u64;
        r.skip(HASH_SIZE)?;
        if layout.size == FROZEN_FOOTER_SIZE && r.u8()? != 0 {
            return Err("frozen .pak indexes are not supported".to_string());
        }
        let mut names = Vec::with_capacity(layout.name_slots);
        for _ in 0..layout.name_slots {
            let slot = r.bytes(NAME_SLOT_SIZE)?;
            let end = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
            names.push(String::from_utf8_lossy(&slot[..end]).to_ascii_lowercase());
        }
        return Ok(Footer { version, encrypted, index_offset, index_size, names, byte_compression: layout.size == BYTE_COMPRESSION_FOOTER_SIZE });
    }
    Err(NOT_PAK_ERROR.to_string())
}

pub struct PakEntry {
    pub path: String,
    offset: u64,
    uncompressed: u64,
    compression: u32,
    blocks: Vec<(u64, u64)>,
    encrypted: bool,
    block_size: u32,
}

pub struct PakFile<D> {
    data: D,
    key: Option<[u8; 32]>,
    footer: Footer,
    pub mount_point: String,
    pub entries: Vec<PakEntry>,
}

fn serialized_size(footer: &Footer, compression: u32, blocks: usize) -> u64 {
    let mut size = 24 + if footer.byte_compression { 1 } else { 4 } + HASH_SIZE;
    if footer.version == 1 {
        size += 8;
    }
    if footer.version >= 3 {
        if compression != 0 {
            size += 4 + blocks * 16;
        }
        size += 5;
    }
    size as u64
}

fn read_entry(r: &mut Reader, footer: &Footer, path: String) -> Result<PakEntry> {
    let offset = r.i64()? as u64;
    r.skip(8)?;
    let uncompressed = r.i64()? as u64;
    let compression = if footer.byte_compression { r.u8()? as u32 } else { r.u32()? };
    if footer.version == 1 {
        r.skip(8)?;
    }
    r.skip(HASH_SIZE)?;
    let (mut blocks, mut encrypted, mut block_size) = (Vec::new(), false, 0);
    if footer.version >= 3 {
        if compression != 0 {
            let count = r.count(16)?;
            blocks = (0..count).map(|_| Ok((r.i64()? as u64, r.i64()? as u64))).collect::<Result<_>>()?;
        }
        encrypted = r.u8()? != 0;
        block_size = r.u32()?;
    }
    Ok(PakEntry { path, offset, uncompressed, compression, blocks, encrypted, block_size })
}

fn read_encoded_entry(encoded: &[u8], at: usize, footer: &Footer, path: String) -> Result<PakEntry> {
    let mut r = Reader::new(encoded);
    r.pos = at;
    let bits = r.u32()?;
    let compression = (bits >> 23) & 0x3f;
    let encrypted = bits & (1 << 22) != 0;
    let block_count = ((bits >> 6) & 0xffff) as usize;
    let block_size = match bits & 0x3f {
        0x3f => r.u32()?,
        n => n << COMPRESSION_BLOCK_UNIT_BITS,
    };
    let mut var_int = |bit: u32| -> Result<u64> {
        if bits & (1 << bit) != 0 { Ok(r.u32()? as u64) } else { Ok(r.i64()? as u64) }
    };
    let offset = var_int(31)?;
    let uncompressed = var_int(30)?;
    let compressed = if compression == 0 { uncompressed } else { var_int(29)? };
    let base = serialized_size(footer, compression, block_count);
    let mut blocks = Vec::with_capacity(block_count);
    if block_count == 1 && !encrypted {
        blocks.push((base, base.saturating_add(compressed)));
    } else {
        let mut start = base;
        for _ in 0..block_count {
            let size = r.u32()? as u64;
            blocks.push((start, start.saturating_add(size)));
            start = start.saturating_add(if encrypted { aligned(size as usize) as u64 } else { size });
        }
    }
    Ok(PakEntry { path, offset, uncompressed, compression, blocks, encrypted, block_size })
}

fn load_range(data: &[u8], offset: u64, size: u64, encrypted: bool, key: Option<&[u8; 32]>) -> Result<Vec<u8>> {
    let start = usize::try_from(offset).ok();
    let size = usize::try_from(size).ok();
    let slice = start.zip(size).and_then(|(start, size)| data.get(start..start.checked_add(size)?)).ok_or("the .pak index lies outside the file")?;
    let mut out = slice.to_vec();
    if encrypted {
        decrypt(&mut out, key.ok_or(KEY_ERROR)?);
    }
    Ok(out)
}

fn read_entries(data: &[u8], footer: &Footer, key: Option<&[u8; 32]>) -> Result<(String, Vec<PakEntry>)> {
    let index = load_range(data, footer.index_offset, footer.index_size, footer.encrypted, key)?;
    let mut r = Reader::new(&index);
    let mount = r.fstring()?;
    let count = r.i32()?;
    if footer.version < VERSION_PATH_HASH_INDEX {
        let mut entries = Vec::new();
        for _ in 0..count {
            let path = r.fstring()?;
            entries.push(read_entry(&mut r, footer, path)?);
        }
        return Ok((mount, entries));
    }
    r.skip(8)?;
    if r.bool32()? {
        r.skip(16 + HASH_SIZE)?;
    }
    if !r.bool32()? {
        return Err("this .pak has no full directory index".to_string());
    }
    let directory_offset = r.i64()? as u64;
    let directory_size = r.i64()? as u64;
    r.skip(HASH_SIZE)?;
    let encoded_size = r.count(1)?;
    let encoded = r.bytes(encoded_size)?;
    let unencoded_count = r.count(1)?;
    let mut unencoded = (0..unencoded_count).map(|_| read_entry(&mut r, footer, String::new()).map(Some)).collect::<Result<Vec<_>>>()?;
    let directory = load_range(data, directory_offset, directory_size, footer.encrypted, key)?;
    let mut d = Reader::new(&directory);
    let mut entries = Vec::new();
    for _ in 0..d.count(8)? {
        let dir = d.fstring()?;
        for _ in 0..d.count(8)? {
            let name = d.fstring()?;
            let path = format!("{}{}", dir.trim_start_matches('/'), name);
            let location = d.i32()?;
            if location >= 0 {
                entries.push(read_encoded_entry(encoded, location as usize, footer, path)?);
            } else {
                let slot = unencoded.get_mut(-(location + 1) as usize).ok_or("the .pak directory index is out of range")?;
                let mut entry = slot.take().ok_or("the .pak directory index repeats an entry")?;
                entry.path = path;
                entries.push(entry);
            }
        }
    }
    Ok((mount, entries))
}

impl PakFile<Mmap> {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        let data = unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {}", path.display(), e))?;
        PakFile::parse(data, current_key())
    }
}

impl<D: AsRef<[u8]>> PakFile<D> {
    pub fn parse(data: D, key: Option<[u8; 32]>) -> Result<Self> {
        let footer = read_footer(data.as_ref())?;
        let (mount_point, entries) = read_entries(data.as_ref(), &footer, key.as_ref()).map_err(|e| {
            if footer.encrypted && e != KEY_ERROR { WRONG_KEY_ERROR.to_string() } else { e }
        })?;
        Ok(PakFile { data, key, footer, mount_point, entries })
    }

    pub fn package_entries(&self) -> Vec<&PakEntry> {
        self.entries
            .iter()
            .filter(|e| crate::is_unreal_file(&e.path) && self.relative_path(e).is_some())
            .collect()
    }

    pub fn relative_path(&self, entry: &PakEntry) -> Option<PathBuf> {
        let joined = format!("{}{}", self.mount_point.trim_start_matches("../").trim_start_matches('/'), entry.path);
        let path = Path::new(&joined);
        path.components().all(|c| matches!(c, Component::Normal(_))).then(|| path.to_path_buf())
    }

    fn range(&self, start: u64, size: usize) -> Result<&[u8]> {
        usize::try_from(start)
            .ok()
            .and_then(|s| self.data.as_ref().get(s..s.checked_add(size)?))
            .ok_or_else(|| "a .pak entry lies outside the file".to_string())
    }

    fn encrypted_range(&self, start: u64, size: usize, entry: &PakEntry) -> Result<Vec<u8>> {
        if !entry.encrypted {
            return self.range(start, size).map(<[u8]>::to_vec);
        }
        let mut bytes = self.range(start, aligned(size))?.to_vec();
        decrypt(&mut bytes, self.key.as_ref().ok_or(KEY_ERROR)?);
        bytes.truncate(size);
        Ok(bytes)
    }

    fn codec(&self, compression: u32) -> Result<&str> {
        if self.footer.names.is_empty() {
            return match compression {
                c if c & 1 != 0 => Ok("zlib"),
                c if c & 2 != 0 => Ok("gzip"),
                c => Err(format!("unsupported .pak compression flags {}", c)),
            };
        }
        self.footer
            .names
            .get(compression as usize - 1)
            .map(String::as_str)
            .ok_or_else(|| format!("unknown .pak compression {}", compression))
    }

    pub fn read(&self, entry: &PakEntry) -> Result<Vec<u8>> {
        let uncompressed = usize::try_from(entry.uncompressed).map_err(|_| "a .pak entry is too large")?;
        if entry.compression == 0 {
            let header = serialized_size(&self.footer, 0, 0);
            return self.encrypted_range(entry.offset.saturating_add(header), uncompressed, entry);
        }
        let codec = self.codec(entry.compression)?;
        let base = if self.footer.version >= VERSION_RELATIVE_OFFSETS { entry.offset } else { 0 };
        let mut out = Vec::new();
        for &(start, end) in &entry.blocks {
            let size = end.checked_sub(start).and_then(|s| usize::try_from(s).ok()).ok_or("a .pak block has an invalid range")?;
            let block = self.encrypted_range(base.saturating_add(start), size, entry)?;
            let remaining = uncompressed.saturating_sub(out.len());
            let limit = if entry.block_size == 0 { remaining } else { remaining.min(entry.block_size as usize) };
            out.extend(decompress(codec, &block, limit)?);
        }
        if out.len() != uncompressed {
            return Err(format!("{}: decompressed {} bytes, expected {}", entry.path, out.len(), uncompressed));
        }
        Ok(out)
    }
}

pub(crate) fn decompress(codec: &str, block: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let cap = limit as u64 + 1;
    let result = match codec {
        "zlib" => flate2::read::ZlibDecoder::new(block).take(cap).read_to_end(&mut out).map(|_| ()),
        "gzip" => flate2::read::GzDecoder::new(block).take(cap).read_to_end(&mut out).map(|_| ()),
        "zstd" | "zstandard" => {
            let decoder = ruzstd::decoding::StreamingDecoder::new(block).map_err(|e| e.to_string())?;
            decoder.take(cap).read_to_end(&mut out).map(|_| ())
        }
        "lz4" => return lz4_flex::block::decompress(block, limit.min(block.len().saturating_mul(LZ4_MAX_RATIO))).map_err(|e| e.to_string()),
        "oodle" => {
            out.resize(limit, 0);
            let written = oozextract::Extractor::new().read_from_slice(block, &mut out).map_err(|e| format!("Oodle: {}", e))?;
            out.truncate(written);
            return Ok(out);
        }
        other => return Err(format!("unsupported .pak compression {}", other)),
    };
    result.map_err(|e| e.to_string())?;
    if out.len() > limit {
        return Err("a .pak block decompressed past its size".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockCipherEncrypt;
    use std::io::Write;

    const KEY: [u8; 32] = [7; 32];

    fn fstring(s: &str) -> Vec<u8> {
        let mut out = (s.len() as i32 + 1).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out.push(0);
        out
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn encrypt(data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        out.resize(aligned(data.len()), 0);
        let cipher = Aes256::new(&KEY.into());
        for chunk in out.chunks_exact_mut(AES_BLOCK) {
            let mut block = aes::Block::try_from(&*chunk).unwrap();
            cipher.encrypt_block(&mut block);
            chunk.copy_from_slice(&block);
        }
        out
    }

    fn u64s(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn footer(prefix: &[u8], version: u32, offset: u64, size: u64, tail: &[u8]) -> Vec<u8> {
        let mut out = prefix.to_vec();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend(u64s(&[offset, size]));
        out.extend_from_slice(&[0; HASH_SIZE]);
        out.extend_from_slice(tail);
        out
    }

    fn names(first: &str) -> Vec<u8> {
        let mut out = first.as_bytes().to_vec();
        out.resize(NAME_SLOT_SIZE * 5, 0);
        out
    }

    #[test]
    fn legacy_uncompressed() {
        let body = b"hello pak";
        let mut record = u64s(&[0, body.len() as u64, body.len() as u64]);
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&[0; HASH_SIZE]);
        record.extend_from_slice(&[0; 5]);
        let mut pak = record.clone();
        pak.extend_from_slice(body);
        let offset = pak.len() as u64;
        let mut index = fstring("../../../Game/Content/");
        index.extend_from_slice(&1i32.to_le_bytes());
        index.extend(fstring("A/a.uasset"));
        index.extend_from_slice(&record);
        pak.extend_from_slice(&index);
        pak.extend(footer(&[], 3, offset, index.len() as u64, &[]));
        let parsed = PakFile::parse(pak, None).unwrap();
        assert_eq!(parsed.read(&parsed.entries[0]).unwrap(), body);
        assert_eq!(parsed.relative_path(&parsed.entries[0]).unwrap(), Path::new("Game/Content/A/a.uasset"));
    }

    #[test]
    fn named_zlib_blocks() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let parts: Vec<Vec<u8>> = body.chunks(65536).map(zlib).collect();
        let header = 24 + 4 + HASH_SIZE + 4 + parts.len() * 16 + 5;
        let mut blocks = Vec::new();
        let mut at = header as u64;
        for part in &parts {
            blocks.push((at, at + part.len() as u64));
            at += part.len() as u64;
        }
        let compressed = at - header as u64;
        let mut record = u64s(&[0, compressed, body.len() as u64]);
        record.extend_from_slice(&1u32.to_le_bytes());
        record.extend_from_slice(&[0; HASH_SIZE]);
        record.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        for (start, end) in &blocks {
            record.extend(u64s(&[*start, *end]));
        }
        record.push(0);
        record.extend_from_slice(&65536u32.to_le_bytes());
        let mut pak = record.clone();
        parts.iter().for_each(|p| pak.extend_from_slice(p));
        let offset = pak.len() as u64;
        let mut index = fstring("/");
        index.extend_from_slice(&1i32.to_le_bytes());
        index.extend(fstring("x.uexp"));
        index.extend_from_slice(&record);
        pak.extend_from_slice(&index);
        let mut prefix = vec![0; 16];
        prefix.push(0);
        let mut tail = vec![];
        tail.extend(names("Zlib"));
        pak.extend(footer(&prefix, 8, offset, index.len() as u64, &tail));
        let parsed = PakFile::parse(pak, None).unwrap();
        assert_eq!(parsed.read(&parsed.entries[0]).unwrap(), body);
        assert_eq!(parsed.relative_path(&parsed.entries[0]).unwrap(), Path::new("x.uexp"));
    }

    #[test]
    fn encrypted_path_hash_index() {
        let body = b"encrypted body with enough bytes to pad";
        let header = serialized_size_v11();
        let mut pak = vec![0; header];
        pak.extend(encrypt(body));
        let bits = (1u32 << 22) | (1 << 31) | (1 << 30) | (1 << 29);
        let mut encoded = bits.to_le_bytes().to_vec();
        encoded.extend_from_slice(&0u32.to_le_bytes());
        encoded.extend_from_slice(&(body.len() as u32).to_le_bytes());
        let mut directory = 1i32.to_le_bytes().to_vec();
        directory.extend(fstring("/Dir/"));
        directory.extend_from_slice(&1i32.to_le_bytes());
        directory.extend(fstring("e.ubulk"));
        directory.extend_from_slice(&0i32.to_le_bytes());
        let directory_cipher = encrypt(&directory);
        let directory_offset = pak.len() as u64;
        pak.extend_from_slice(&directory_cipher);
        let mut index = fstring("../../../Game/");
        index.extend_from_slice(&1i32.to_le_bytes());
        index.extend(u64s(&[0]));
        index.extend_from_slice(&0i32.to_le_bytes());
        index.extend_from_slice(&1i32.to_le_bytes());
        index.extend(u64s(&[directory_offset, directory_cipher.len() as u64]));
        index.extend_from_slice(&[0; HASH_SIZE]);
        index.extend_from_slice(&(encoded.len() as i32).to_le_bytes());
        index.extend_from_slice(&encoded);
        index.extend_from_slice(&0i32.to_le_bytes());
        let cipher = encrypt(&index);
        let offset = pak.len() as u64;
        pak.extend_from_slice(&cipher);
        let mut prefix = vec![0; 16];
        prefix.push(1);
        pak.extend(footer(&prefix, 11, offset, cipher.len() as u64, &names("")));
        assert_eq!(PakFile::parse(pak.clone(), None).err().unwrap(), KEY_ERROR);
        let parsed = PakFile::parse(pak, Some(KEY)).unwrap();
        assert_eq!(parsed.relative_path(&parsed.entries[0]).unwrap(), Path::new("Game/Dir/e.ubulk"));
        assert_eq!(parsed.read(&parsed.entries[0]).unwrap(), body);
    }

    fn serialized_size_v11() -> usize {
        24 + 4 + HASH_SIZE + 5
    }

    #[test]
    fn hostile_paths_are_skipped() {
        let body = b"x";
        let mut record = u64s(&[0, 1, 1]);
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&[0; HASH_SIZE]);
        record.extend_from_slice(&[0; 5]);
        let mut pak = record.clone();
        pak.extend_from_slice(body);
        let offset = pak.len() as u64;
        let mut index = fstring("/");
        index.extend_from_slice(&1i32.to_le_bytes());
        index.extend(fstring("../../evil.uasset"));
        index.extend_from_slice(&record);
        pak.extend_from_slice(&index);
        pak.extend(footer(&[], 3, offset, index.len() as u64, &[]));
        assert!(PakFile::parse(pak, None).unwrap().package_entries().is_empty());
    }

    #[test]
    fn key_parsing() {
        assert_eq!(parse_key(&format!("0x{}", "07".repeat(32))).unwrap(), KEY);
        assert!(parse_key("1234").is_err());
    }
}
