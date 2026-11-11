use crate::bulk::read_bulk_data;
use crate::package::Package;
use crate::properties::read_object_header;
use crate::reader::{Reader, Result};

const TEXTURE_2D: &str = "Texture2D";
const TEXTURE_CUBE: &str = "TextureCube";
const TEXTURE_ARRAY: &str = "Texture2DArray";

pub struct DecodedTexture {
    pub width: usize,
    pub height: usize,
    pub format: String,
    pub rgba: Vec<u8>,
}

struct Mip<'a> {
    width: usize,
    height: usize,
    data: Option<&'a [u8]>,
}

fn decode_blocks(
    data: &[u8],
    width: usize,
    height: usize,
    block_bytes: usize,
    decode: impl Fn(&[u8], usize, usize, &mut [u32]) -> std::result::Result<(), &'static str>,
) -> Result<Vec<u8>> {
    decode_sized_blocks(data, width, height, (4, 4), block_bytes, decode)
}

fn decode_sized_blocks(
    data: &[u8],
    width: usize,
    height: usize,
    block: (usize, usize),
    block_bytes: usize,
    decode: impl Fn(&[u8], usize, usize, &mut [u32]) -> std::result::Result<(), &'static str>,
) -> Result<Vec<u8>> {
    let aw = width.div_ceil(block.0) * block.0;
    let ah = height.div_ceil(block.1) * block.1;
    let needed = (aw / block.0) * (ah / block.1) * block_bytes;
    if data.len() < needed {
        return Err(format!("texture data too short ({} < {})", data.len(), needed));
    }
    let mut out = vec![0u32; aw * ah];
    decode(&data[..needed], aw, ah, &mut out).map_err(|e| e.to_string())?;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for &p in &out[y * aw..y * aw + width] {
            let [b, g, r, a] = p.to_le_bytes();
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    Ok(rgba)
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10) & 0x1f;
    let frac = (h & 0x3ff) as f32;
    match exp {
        0 => sign * frac * (1.0 / (1u32 << 24) as f32),
        31 => if frac == 0.0 { sign * f32::INFINITY } else { f32::NAN },
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp as i32 - 15),
    }
}

fn unit_to_u8(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

fn astc_block_size(format: &str) -> Option<(usize, usize)> {
    let size = format.strip_prefix("PF_ASTC_")?.trim_end_matches("_HDR").trim_end_matches("_UNORM");
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn unorm16(c: &[u8]) -> u8 {
    c[1]
}

fn decode_r11g11b10(c: &[u8]) -> [u8; 4] {
    let packed = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    let small_float = |bits: u32, mantissa_bits: u32| {
        let exponent = bits >> mantissa_bits;
        let mantissa = (bits & ((1 << mantissa_bits) - 1)) as f32 / (1u32 << mantissa_bits) as f32;
        match exponent {
            0 => mantissa * 2f32.powi(-14),
            _ => (1.0 + mantissa) * 2f32.powi(exponent as i32 - 15),
        }
    };
    [
        unit_to_u8(small_float(packed & 0x7ff, 6)),
        unit_to_u8(small_float((packed >> 11) & 0x7ff, 6)),
        unit_to_u8(small_float(packed >> 22, 5)),
        255,
    ]
}

fn decode_pixels(format: &str, data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let pixels = width.checked_mul(height).ok_or("texture is too large")?;
    let raw = |bpp: usize| -> Result<&[u8]> {
        pixels.checked_mul(bpp).and_then(|n| data.get(..n)).ok_or_else(|| "texture data too short".to_string())
    };
    if let Some(block) = astc_block_size(format) {
        return decode_sized_blocks(data, width, height, block, 16, |d, w, h, out| texture2ddecoder::decode_astc(d, w, h, block.0, block.1, out));
    }
    match format {
        "PF_ETC1" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_etc1),
        "PF_ETC2_RGB" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_etc2_rgb),
        "PF_ETC2_RGBA" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_etc2_rgba8),
        "PF_ETC2_R11_EAC" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_eacr),
        "PF_ETC2_RG11_EAC" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_eacrg),
        "PF_ATC_RGB" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_atc_rgb4),
        "PF_ATC_RGBA_E" | "PF_ATC_RGBA_I" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_atc_rgba8),
        "PF_PVRTC2" | "PF_PVRTC4" => {
            let two_bpp = format == "PF_PVRTC2";
            let needed = width.div_ceil(if two_bpp { 8 } else { 4 }) * height.div_ceil(4) * 8;
            let bytes = data.get(..needed).ok_or("texture data too short")?;
            let mut out = vec![0u32; pixels];
            texture2ddecoder::decode_pvrtc(bytes, width, height, &mut out, two_bpp).map_err(|e| e.to_string())?;
            Ok(out.iter().flat_map(|p| {
                let [b, g, r, a] = p.to_le_bytes();
                [r, g, b, a]
            }).collect())
        }
        "PF_R8" | "PF_R8_UINT" => Ok(raw(1)?.iter().flat_map(|&v| [v, v, v, 255]).collect()),
        "PF_R8G8" | "PF_V8U8" => Ok(raw(2)?.chunks_exact(2).flat_map(|c| [c[0], c[1], 0, 255]).collect()),
        "PF_R8G8B8A8_UINT" | "PF_R8G8B8A8_SNORM" => Ok(raw(4)?.to_vec()),
        "PF_R5G6B5_UNORM" => Ok(raw(2)?
            .chunks_exact(2)
            .flat_map(|c| {
                let v = u16::from_le_bytes([c[0], c[1]]);
                let (r, g, b) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                [(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8, 255]
            })
            .collect()),
        "PF_B5G5R5A1_UNORM" => Ok(raw(2)?
            .chunks_exact(2)
            .flat_map(|c| {
                let v = u16::from_le_bytes([c[0], c[1]]);
                let channel = |shift: u16| (((v >> shift) & 31) * 255 / 31) as u8;
                [channel(10), channel(5), channel(0), if v >> 15 != 0 { 255 } else { 0 }]
            })
            .collect()),
        "PF_A2B10G10R10" => Ok(raw(4)?
            .chunks_exact(4)
            .flat_map(|c| {
                let v = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let channel = |shift: u32| (((v >> shift) & 1023) >> 2) as u8;
                [channel(0), channel(10), channel(20), ((v >> 30) * 85) as u8]
            })
            .collect()),
        "PF_A16B16G16R16" | "PF_R16G16B16A16_UNORM" | "PF_R16G16B16A16_UINT" => Ok(raw(8)?.chunks_exact(2).map(unorm16).collect()),
        "PF_G16R16" | "PF_R16G16_UINT" => Ok(raw(4)?.chunks_exact(4).flat_map(|c| [c[1], c[3], 0, 255]).collect()),
        "PF_R16_UINT" | "PF_R16_SINT" => Ok(raw(2)?.chunks_exact(2).flat_map(|c| [c[1], c[1], c[1], 255]).collect()),
        "PF_G16R16F" | "PF_G16R16F_FILTER" => Ok(raw(4)?
            .chunks_exact(4)
            .flat_map(|c| {
                let channel = |i: usize| unit_to_u8(f16_to_f32(u16::from_le_bytes([c[i], c[i + 1]])));
                [channel(0), channel(2), 0, 255]
            })
            .collect()),
        "PF_G32R32F" => Ok(raw(8)?
            .chunks_exact(8)
            .flat_map(|c| {
                let channel = |i: usize| unit_to_u8(f32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]));
                [channel(0), channel(4), 0, 255]
            })
            .collect()),
        "PF_R32G32B32F" => Ok(raw(12)?
            .chunks_exact(12)
            .flat_map(|c| {
                let channel = |i: usize| unit_to_u8(f32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]));
                [channel(0), channel(4), channel(8), 255]
            })
            .collect()),
        "PF_FloatRGB" | "PF_FloatR11G11B10" => Ok(raw(4)?.chunks_exact(4).flat_map(decode_r11g11b10).collect()),
        "PF_DXT1" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_bc1),
        "PF_DXT3" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_bc2),
        "PF_DXT5" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_bc3),
        "PF_BC4" => decode_blocks(data, width, height, 8, texture2ddecoder::decode_bc4),
        "PF_BC7" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_bc7),
        "PF_BC6H" => decode_blocks(data, width, height, 16, texture2ddecoder::decode_bc6_unsigned),
        "PF_BC5" => {
            let mut rgba = decode_blocks(data, width, height, 16, texture2ddecoder::decode_bc5)?;
            for px in rgba.chunks_exact_mut(4) {
                let x = px[0] as f32 / 127.5 - 1.0;
                let y = px[1] as f32 / 127.5 - 1.0;
                let z = (1.0 - x * x - y * y).max(0.0).sqrt();
                px[2] = ((z + 1.0) * 127.5).round() as u8;
                px[3] = 255;
            }
            Ok(rgba)
        }
        "PF_B8G8R8A8" => Ok(raw(4)?.chunks_exact(4).flat_map(|c| [c[2], c[1], c[0], c[3]]).collect()),
        "PF_R8G8B8A8" => Ok(raw(4)?.to_vec()),
        "PF_G8" | "PF_L8" | "PF_A8" => Ok(raw(1)?.iter().flat_map(|&v| [v, v, v, 255]).collect()),
        "PF_G16" => Ok(raw(2)?.chunks_exact(2).flat_map(|c| [c[1], c[1], c[1], 255]).collect()),
        "PF_R16F" => Ok(raw(2)?
            .chunks_exact(2)
            .flat_map(|c| {
                let v = unit_to_u8(f16_to_f32(u16::from_le_bytes([c[0], c[1]])));
                [v, v, v, 255]
            })
            .collect()),
        "PF_R32_FLOAT" => Ok(raw(4)?
            .chunks_exact(4)
            .flat_map(|c| {
                let v = unit_to_u8(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                [v, v, v, 255]
            })
            .collect()),
        "PF_FloatRGBA" => Ok(raw(8)?
            .chunks_exact(2)
            .map(|c| unit_to_u8(f16_to_f32(u16::from_le_bytes([c[0], c[1]]))))
            .collect()),
        "PF_A32B32G32R32F" => Ok(raw(16)?
            .chunks_exact(4)
            .map(|c| unit_to_u8(f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
            .collect()),
        _ => Err(format!("unsupported pixel format {}", format)),
    }
}

pub fn read_texture(pkg: &Package) -> Result<DecodedTexture> {
    let export = pkg
        .exports
        .iter()
        .find(|e| matches!(pkg.export_class_name(e), Some(TEXTURE_2D | TEXTURE_CUBE | TEXTURE_ARRAY)))
        .ok_or("package has no texture export")?;
    let data = pkg.export_data(export)?;
    let mut r = Reader::new(data);
    read_object_header(pkg, export, &mut r)?;
    r.skip(2)?;
    r.skip(2)?;
    if !r.bool32()? {
        return Err("texture is not cooked".into());
    }
    let start = r.pos;
    let gaps: &[usize] = if pkg.is_ue5 { &[4, 0] } else { &[0] };
    let pixel_format = gaps
        .iter()
        .find_map(|&gap| {
            r.pos = start + gap;
            pkg.read_name(&mut r).ok().filter(|n| n.starts_with("PF_") || n == "None")
        })
        .ok_or("could not find the texture pixel format")?;
    if pixel_format == "None" {
        return Err("texture has no platform data".into());
    }
    r.i64()?;
    if pkg.is_ue5 {
        r.skip(16)?;
    }
    r.i32()?;
    r.i32()?;
    let packed = r.u32()?;
    let format = r.fstring()?;
    if packed & (1 << 30) != 0 {
        r.skip(8)?;
    }
    r.i32()?;
    let mip_count = r.count(20)?;
    let mut mips = Vec::with_capacity(mip_count);
    for _ in 0..mip_count {
        if !pkg.is_ue5 {
            r.bool32()?;
        }
        let payload = read_bulk_data(pkg, &mut r).unwrap_or(None);
        let width = r.i32()?;
        let height = r.i32()?;
        r.i32()?;
        if width <= 0 || height <= 0 {
            return Err(format!("invalid mip size {}x{}", width, height));
        }
        mips.push(Mip { width: width as usize, height: height as usize, data: payload });
    }
    if mips.is_empty() {
        return Err("texture has no mips (virtual textures are not supported)".into());
    }
    let mip = mips
        .iter()
        .find(|m| m.data.is_some())
        .ok_or("no mip data available (is the .ubulk file missing?)")?;
    let slices = if packed >> 31 != 0 { 6 } else { (packed & 0x3fff_ffff).max(1) as usize };
    let data = mip.data.unwrap();
    let rgba = decode_pixels(&format, &data[..data.len() / slices], mip.width, mip.height)?;
    Ok(DecodedTexture { width: mip.width, height: mip.height, format, rgba })
}
