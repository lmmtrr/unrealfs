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
    if flags & BULKDATA_OPTIONAL_PAYLOAD != 0 {
        return Err("optional (.uptnl) bulk data is not supported".into());
    }
    let size = size as usize;
    if flags & BULKDATA_FORCE_INLINE_PAYLOAD != 0 || flags & (BULKDATA_PAYLOAD_AT_END_OF_FILE | BULKDATA_PAYLOAD_IN_SEPARATE_FILE) == 0 {
        return Ok(Some(r.bytes(size)?));
    }
    if flags & BULKDATA_NO_OFFSET_FIX_UP == 0 {
        offset += pkg.bulk_data_start_offset;
    }
    if flags & BULKDATA_PAYLOAD_IN_SEPARATE_FILE != 0 {
        let ubulk = pkg
            .ubulk
            .as_ref()
            .ok_or_else(|| format!("{} needs its .ubulk file", pkg.path))?;
        for start in [offset, offset - pkg.bulk_data_start_offset] {
            if start >= 0 && start as usize + size <= ubulk.len() {
                return Ok(Some(&ubulk[start as usize..start as usize + size]));
            }
        }
        return Err(format!("bulk data at {} lies outside the .ubulk", offset));
    }
    let start = offset - pkg.header_size as i64;
    if start < 0 || start as usize + size > pkg.uexp.len() {
        return Err(format!("bulk data at {} lies outside the .uexp", offset));
    }
    Ok(Some(&pkg.uexp[start as usize..start as usize + size]))
}
