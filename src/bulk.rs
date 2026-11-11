use crate::package::Package;
use crate::reader::{Reader, Result};

const BULKDATA_PAYLOAD_AT_END_OF_FILE: u32 = 1 << 0;
const BULKDATA_SERIALIZE_COMPRESSED_ZLIB: u32 = 1 << 1;
const BULKDATA_COMPRESSED_LZO: u32 = 1 << 4;
const BULKDATA_UNUSED: u32 = 1 << 5;
const BULKDATA_FORCE_INLINE_PAYLOAD: u32 = 1 << 6;
const BULKDATA_PAYLOAD_IN_SEPARATE_FILE: u32 = 1 << 8;
const BULKDATA_OPTIONAL_PAYLOAD: u32 = 1 << 11;
const BULKDATA_SIZE_64BIT: u32 = 1 << 13;
const BULKDATA_NO_OFFSET_FIX_UP: u32 = 1 << 16;

pub fn read_bulk_data<'a>(pkg: &'a Package, r: &mut Reader<'a>) -> Result<Option<&'a [u8]>> {
    let (flags, count, size, mut offset) = match &pkg.data_resources {
        Some(resources) => {
            let index = r.i32()?;
            if index < 0 {
                return Ok(None);
            }
            let resource = resources
                .get(index as usize)
                .ok_or_else(|| format!("data resource {} out of range", index))?;
            let size = resource.serial_size;
            (resource.legacy_flags, size, size, resource.serial_offset)
        }
        None => {
            let flags = r.u32()?;
            let (count, size) = if flags & BULKDATA_SIZE_64BIT != 0 {
                (r.i64()?, r.i64()?)
            } else {
                (r.i32()? as i64, r.i32()? as i64)
            };
            (flags, count, size, r.i64()?)
        }
    };
    if count <= 0 || flags & BULKDATA_UNUSED != 0 {
        return Ok(None);
    }
    if flags & (BULKDATA_SERIALIZE_COMPRESSED_ZLIB | BULKDATA_COMPRESSED_LZO) != 0 {
        return Err("compressed bulk data is not supported".into());
    }
    let size = usize::try_from(size).map_err(|_| format!("invalid bulk data size {}", size))?;
    if flags & BULKDATA_OPTIONAL_PAYLOAD == 0 && (flags & BULKDATA_FORCE_INLINE_PAYLOAD != 0 || flags & (BULKDATA_PAYLOAD_AT_END_OF_FILE | BULKDATA_PAYLOAD_IN_SEPARATE_FILE) == 0) {
        return Ok(Some(r.bytes(size)?));
    }
    if flags & BULKDATA_NO_OFFSET_FIX_UP == 0 {
        offset = offset.saturating_add(pkg.bulk_data_start_offset);
    }
    if flags & (BULKDATA_PAYLOAD_IN_SEPARATE_FILE | BULKDATA_OPTIONAL_PAYLOAD) != 0 {
        let (file, ext) = if flags & BULKDATA_OPTIONAL_PAYLOAD != 0 { (&pkg.uptnl, "uptnl") } else { (&pkg.ubulk, "ubulk") };
        let data = file.as_deref().ok_or_else(|| format!("{} needs its .{} file", pkg.path, ext))?;
        return [offset, offset.saturating_sub(pkg.bulk_data_start_offset)]
            .into_iter()
            .find_map(|start| range(data, start, size))
            .map(Some)
            .ok_or_else(|| format!("bulk data at {} lies outside the .{}", offset, ext));
    }
    range(&pkg.uexp, offset.saturating_sub(pkg.header_size as i64), size)
        .map(Some)
        .ok_or_else(|| format!("bulk data at {} lies outside the .uexp", offset))
}

fn range(data: &[u8], start: i64, size: usize) -> Option<&[u8]> {
    let start = usize::try_from(start).ok()?;
    data.get(start..start.checked_add(size)?)
}
