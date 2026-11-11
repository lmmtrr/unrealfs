use crate::acl::{decode_scalar_tracks, decode_transform_tracks, num_frames as acl_num_frames};
use crate::bulk::read_bulk_data;
use crate::package::Package;
use crate::properties::{read_object_header, read_tagged_properties, PropValue};
use crate::reader::{Reader, Result};
use crate::skeletal_mesh::{convert_vector, read_reference_skeleton, UNIT_SCALE};
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct AnimInfo {
    pub name: String,
    pub skeleton: Option<String>,
    pub num_frames: u32,
    pub length: f32,
    pub additive: Option<String>,
}

pub struct SampledAnimation {
    pub info: AnimInfo,
    pub bones: Vec<String>,
    pub channels: Vec<[bool; 3]>,
    pub samples: Vec<f32>,
    pub curves: Vec<String>,
    pub curve_samples: Vec<f32>,
}

const ACF_NONE: u8 = 0;
const ACF_FLOAT96_NO_W: u8 = 1;
const ACF_FIXED48_NO_W: u8 = 2;
const ACF_INTERVAL_FIXED32_NO_W: u8 = 3;
const ACF_FIXED32_NO_W: u8 = 4;
const ACF_FLOAT32_NO_W: u8 = 5;
const ACF_IDENTITY: u8 = 6;
const AKF_CONSTANT_KEY_LERP: u8 = 0;
const AKF_VARIABLE_KEY_LERP: u8 = 1;
const AKF_PER_TRACK_COMPRESSION: u8 = 2;

fn find_export<'a>(pkg: &'a Package, class: &str) -> Result<&'a crate::package::ObjectExport> {
    pkg.exports
        .iter()
        .find(|e| pkg.export_class_name(e) == Some(class))
        .ok_or_else(|| format!("package has no {} export", class))
}

pub fn read_anim_info(pkg: &Package) -> Result<AnimInfo> {
    let export = find_export(pkg, "AnimSequence")?;
    let props = read_object_header(pkg, export, &mut Reader::new(pkg.export_data(export)?))?;
    let mut info = info_from_props(pkg, &export.object_name, &props);
    if info.num_frames == 0 {
        if let Ok((with_keys, _, compressed)) = read_compressed(pkg) {
            info.num_frames = if compressed.acl { acl_num_frames(&compressed.stream).unwrap_or(0) } else { with_keys.num_frames };
        }
    }
    Ok(info)
}

fn info_from_props(pkg: &Package, name: &str, props: &crate::properties::Properties) -> AnimInfo {
    let num_frames = match props.get("NumFrames") {
        Some(PropValue::Int(n)) => *n as u32,
        _ => 0,
    };
    let length = match props.get("SequenceLength") {
        Some(PropValue::Float(v)) => *v,
        _ => 0.0,
    };
    let skeleton = match props.get("Skeleton") {
        Some(PropValue::Object(i)) => pkg.import_path(*i).map(|p| p.split('.').next().unwrap_or(&p).to_string()),
        _ => None,
    };
    let additive = match props.get("AdditiveAnimType") {
        Some(PropValue::Name(n)) if !n.ends_with("AAT_None") => Some(n.rsplit("::").next().unwrap_or(n).to_string()),
        _ => None,
    };
    AnimInfo { name: name.to_string(), skeleton, num_frames, length, additive }
}

pub fn read_skeleton_bones(pkg: &Package) -> Result<Vec<String>> {
    let export = find_export(pkg, "Skeleton")?;
    let mut r = Reader::new(pkg.export_data(export)?);
    read_object_header(pkg, export, &mut r)?;
    Ok(read_reference_skeleton(pkg, &mut r)?.into_iter().map(|b| b.name).collect())
}

struct Compressed {
    key_format: u8,
    trans_format: u8,
    rot_format: u8,
    scale_format: u8,
    track_offsets: Vec<i32>,
    scale_offsets: Vec<i32>,
    scale_strip_size: i32,
    stream: Vec<u8>,
    curves: CompressedCurves,
    acl: bool,
    num_keys: u32,
}

fn read_serialized_stream<'a>(pkg: &'a Package, r: &mut Reader<'a>) -> Result<Vec<u8>> {
    let num_bytes = r.count(0)?;
    if r.bool32()? {
        let data = read_bulk_data(pkg, r)?.unwrap_or(&[]);
        data.get(..num_bytes).map(|d| d.to_vec()).ok_or_else(|| "animation bulk data is too short".into())
    } else {
        Ok(r.bytes(num_bytes)?.to_vec())
    }
}

fn size_field(r: &mut Reader) -> Result<usize> {
    let at = r.pos;
    let n = r.i32()?;
    usize::try_from(n).map_err(|_| format!("invalid size {} at {}", n, at))
}

struct CodecHeader {
    formats: [u8; 4],
    num_track_offsets: usize,
    num_scale_offsets: usize,
    scale_strip_size: i32,
    num_bytes: usize,
}

impl CodecHeader {
    fn valid(&self, stream_len: usize) -> bool {
        self.formats[0] <= AKF_PER_TRACK_COMPRESSION
            && self.formats[1..].iter().all(|&f| f <= ACF_IDENTITY)
            && self.num_track_offsets * 4 + self.num_scale_offsets * 4 + self.num_bytes == stream_len
    }
}

#[derive(Default)]
struct CompressedCurves {
    names: Vec<String>,
    codec: String,
    data: Vec<u8>,
}

fn read_curve_stream(r: &mut Reader) -> Result<(String, Vec<u8>)> {
    let codec = r.fstring()?;
    let n = r.count(1)?;
    Ok((codec, r.bytes(n)?.to_vec()))
}

struct LayoutData {
    header: Option<CodecHeader>,
    acl: bool,
    num_keys: u32,
    stream: Vec<u8>,
    curve_codec: String,
    curve_data: Vec<u8>,
}

fn read_layout_4_23<'a>(pkg: &'a Package, r: &mut Reader<'a>) -> Result<LayoutData> {
    let formats = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
    r.i32()?;
    let num_track_offsets = size_field(r)?;
    let num_scale_offsets = size_field(r)?;
    let scale_strip_size = r.i32()?;
    let num_bytes = size_field(r)?;
    let header = CodecHeader { formats, num_track_offsets, num_scale_offsets, scale_strip_size, num_bytes };
    let stream = read_serialized_stream(pkg, r)?;
    let (curve_codec, curve_data) = read_curve_stream(r)?;
    Ok(LayoutData { header: Some(header), acl: false, num_keys: 0, stream, curve_codec, curve_data })
}

fn read_layout_4_25<'a>(pkg: &'a Package, r: &mut Reader<'a>) -> Result<LayoutData> {
    let stream = read_serialized_stream(pkg, r)?;
    let bone_codec = r.fstring()?;
    let (curve_codec, curve_data) = read_curve_stream(r)?;
    if bone_codec.is_empty() && stream.is_empty() {
        return Ok(LayoutData { header: None, acl: false, num_keys: 0, stream, curve_codec, curve_data });
    }
    if bone_codec.to_ascii_lowercase().contains("acl") {
        if stream.is_empty() {
            return Err(format!("ACL animations streamed from a database are not supported ({})", bone_codec));
        }
        let num_keys = r.i32()?.max(0) as u32;
        r.bool32()?;
        return Ok(LayoutData { header: None, acl: true, num_keys, stream, curve_codec, curve_data });
    }
    let mut num_keys = 0;
    let codec = (|| -> Result<CodecHeader> {
        num_keys = r.i32()?.max(0) as u32;
        let formats = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
        let num_bytes = size_field(r)?;
        let num_track_offsets = size_field(r)?;
        let num_scale_offsets = size_field(r)?;
        let scale_strip_size = r.i32()?;
        Ok(CodecHeader { formats, num_track_offsets, num_scale_offsets, scale_strip_size, num_bytes })
    })();
    let header = codec.map_err(|e| format!("{} (codec {:?})", e, bone_codec))?;
    Ok(LayoutData { header: Some(header), acl: false, num_keys, stream, curve_codec, curve_data })
}

fn split_stream(header: &CodecHeader, stream: &[u8]) -> Compressed {
    let ints = |bytes: &[u8]| bytes.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<_>>();
    let tracks_end = header.num_track_offsets * 4;
    let scale_end = tracks_end + header.num_scale_offsets * 4;
    Compressed {
        key_format: header.formats[0],
        trans_format: header.formats[1],
        rot_format: header.formats[2],
        scale_format: header.formats[3],
        track_offsets: ints(&stream[..tracks_end]),
        scale_offsets: ints(&stream[tracks_end..scale_end]),
        scale_strip_size: header.scale_strip_size,
        stream: stream[scale_end..].to_vec(),
        curves: CompressedCurves::default(),
        acl: false,
        num_keys: 0,
    }
}

fn empty_compressed() -> Compressed {
    Compressed {
        key_format: AKF_CONSTANT_KEY_LERP,
        trans_format: ACF_NONE,
        rot_format: ACF_NONE,
        scale_format: ACF_NONE,
        track_offsets: Vec::new(),
        scale_offsets: Vec::new(),
        scale_strip_size: 0,
        stream: Vec::new(),
        curves: CompressedCurves::default(),
        acl: false,
        num_keys: 0,
    }
}

fn read_i32_array(r: &mut Reader) -> Result<Vec<i32>> {
    let n = r.count(4)?;
    (0..n).map(|_| r.i32()).collect()
}

fn read_layout_4_23_plus<'a>(pkg: &'a Package, r: &mut Reader<'a>) -> Result<(Vec<i32>, Compressed)> {
    r.i32()?;
    let track_map = read_i32_array(r)?;
    let num_curves = r.count(8)?;
    let curve_names = (0..num_curves).map(|_| pkg.read_name(r)).collect::<Result<Vec<_>>>()?;
    let start = r.pos;
    let mut errors = Vec::new();
    for layout in [read_layout_4_23, read_layout_4_25] {
        r.pos = start;
        match layout(pkg, r) {
            Ok(found) => {
                let mut compressed = match &found.header {
                    _ if found.acl => Compressed { stream: found.stream, acl: true, ..empty_compressed() },
                    Some(header) if header.valid(found.stream.len()) => split_stream(header, &found.stream),
                    Some(_) => {
                        errors.push("inconsistent compressed data sizes".to_string());
                        continue;
                    }
                    None if track_map.is_empty() => empty_compressed(),
                    None => {
                        errors.push("bone tracks without bone data".to_string());
                        continue;
                    }
                };
                compressed.curves = CompressedCurves { names: curve_names, codec: found.curve_codec, data: found.curve_data };
                compressed.num_keys = found.num_keys;
                return Ok((track_map, compressed));
            }
            Err(e) => errors.push(e),
        }
    }
    Err(errors.iter().find(|e| e.contains("ACL")).cloned().unwrap_or_else(|| errors.join("; ")))
}

fn read_layout_4_21(pkg: &Package, r: &mut Reader, num_frames: u32, is_4_22: bool) -> Result<(Vec<i32>, Compressed)> {
    let formats = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
    if formats[0] > AKF_PER_TRACK_COMPRESSION || formats[1..].iter().any(|&f| f > ACF_IDENTITY) {
        return Err("invalid compression formats".into());
    }
    let track_offsets = read_i32_array(r)?;
    let scale_offsets = read_i32_array(r)?;
    let scale_strip_size = r.i32()?;
    if r.count(16)? > 0 {
        return Err("animations with compressed segments are not supported".into());
    }
    let track_map = read_i32_array(r)?;
    let mut curve_names = Vec::new();
    if is_4_22 {
        let num_curves = r.count(8)?;
        curve_names = (0..num_curves).map(|_| pkg.read_name(r)).collect::<Result<Vec<_>>>()?;
    } else {
        read_tagged_properties(pkg, r)?;
    }
    r.i32()?;
    if is_4_22 {
        r.i32()?;
    }
    let peek = r.data.get(r.pos..r.pos + 4).ok_or("truncated animation")?;
    let peek = i32::from_le_bytes([peek[0], peek[1], peek[2], peek[3]]);
    if let Some(name) = usize::try_from(peek).ok().and_then(|i| pkg.names.get(i)) {
        if name.starts_with("ACL") {
            return Err(format!("ACL-compressed animations are not supported ({})", name));
        }
        if name.starts_with("AKF_") {
            r.skip(8)?;
        }
    }
    let num_bytes = r.count(1)?;
    let mut stream = r.bytes(num_bytes)?.to_vec();
    let mut curves = CompressedCurves::default();
    if is_4_22 {
        let (codec, data) = read_curve_stream(r)?;
        curves = CompressedCurves { names: curve_names, codec, data };
    }
    if r.remaining() > 8 {
        return Err("unexpected data after the compressed animation".into());
    }
    let per_track = if formats[0] == AKF_PER_TRACK_COMPRESSION { 2 } else { 4 };
    if track_offsets.len() != track_map.len() * per_track {
        return Err("track offsets do not match the track count".into());
    }
    let mut compressed = Compressed {
        key_format: formats[0],
        trans_format: formats[1],
        rot_format: formats[2],
        scale_format: formats[3],
        track_offsets,
        scale_offsets,
        scale_strip_size,
        stream: Vec::new(),
        curves,
        acl: false,
        num_keys: 0,
    };
    if compressed.key_format == AKF_PER_TRACK_COMPRESSION && !compressed.scale_offsets.is_empty() {
        stream = transfer_per_track_data(&compressed, &stream, num_frames)?;
    }
    compressed.stream = stream;
    Ok((track_map, compressed))
}

fn transfer_per_track_data(c: &Compressed, src: &[u8], num_frames: u32) -> Result<Vec<u8>> {
    const COMPONENTS: [usize; 8] = [3, 1, 1, 2, 1, 2, 2, 3];
    let mut dst = vec![0u8; src.len()];
    let mut src_offset = 0usize;
    let strip = c.scale_strip_size.max(1) as usize;
    let mut copy = |size: usize, src_offset: &mut usize, dst_offset: &mut usize| -> Result<()> {
        let s = src.get(*src_offset..*src_offset + size).ok_or("per-track data out of range")?;
        dst.get_mut(*dst_offset..*dst_offset + size).ok_or("per-track data out of range")?.copy_from_slice(s);
        *src_offset += size;
        *dst_offset += size;
        Ok(())
    };
    for track in 0..c.track_offsets.len() / 2 {
        for kind in 0..3 {
            let offset = match kind {
                0 => c.track_offsets[track * 2],
                1 => c.track_offsets[track * 2 + 1],
                _ => c.scale_offsets.get(track * strip).copied().unwrap_or(-1),
            };
            if offset < 0 {
                continue;
            }
            let mut dst_offset = offset as usize;
            let header = src.get(src_offset..src_offset + 4).ok_or("per-track data out of range")?;
            let packed = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            copy(4, &mut src_offset, &mut dst_offset)?;
            let format = (packed >> 28) as u8;
            let mask = ((packed >> 24) & 0xf) as usize;
            let num_keys = (packed & 0x00ff_ffff) as usize;
            let components = COMPONENTS[mask & 7];
            if format == ACF_INTERVAL_FIXED32_NO_W {
                copy(4 * components * 2, &mut src_offset, &mut dst_offset)?;
            }
            match format {
                ACF_FLOAT96_NO_W => copy(4 * components * num_keys, &mut src_offset, &mut dst_offset)?,
                ACF_FIXED48_NO_W => copy(2 * components * num_keys, &mut src_offset, &mut dst_offset)?,
                ACF_INTERVAL_FIXED32_NO_W | ACF_FIXED32_NO_W | ACF_FLOAT32_NO_W => {
                    copy(4 * num_keys, &mut src_offset, &mut dst_offset)?
                }
                _ => {}
            }
            if mask & 8 != 0 {
                let pad = dst_offset.div_ceil(4) * 4 - dst_offset;
                copy(pad, &mut src_offset, &mut dst_offset)?;
                copy(if num_frames < 256 { 1 } else { 2 } * num_keys, &mut src_offset, &mut dst_offset)?;
            }
            let pad = dst_offset.div_ceil(4) * 4 - dst_offset;
            copy(pad, &mut src_offset, &mut dst_offset)?;
        }
    }
    Ok(dst)
}

pub fn read_anim_contents(pkg: &Package) -> Option<(bool, Vec<String>)> {
    let (_, track_map, compressed) = read_compressed(pkg).ok()?;
    let curves = if sample_curves(&compressed.curves, 1, 30.0).is_ok() { compressed.curves.names } else { Vec::new() };
    Some((!track_map.is_empty(), curves))
}

#[derive(Clone, Copy)]
struct CurveKey {
    time: f32,
    value: f32,
    arrive: f32,
    leave: f32,
    arrive_weight: f32,
    leave_weight: f32,
    weight_mode: u8,
    mode: u8,
}

const RCTWM_ARRIVE: u8 = 1;
const RCTWM_LEAVE: u8 = 2;
const RCTWM_BOTH: u8 = 3;

const RCCF_EMPTY: u8 = 0;
const RCCF_CONSTANT: u8 = 1;
const RCCF_LINEAR: u8 = 2;
const RCCF_CUBIC: u8 = 3;
const RCCF_MIXED: u8 = 4;
const RCCF_WEIGHTED: u8 = 5;

fn align(offset: usize, to: usize) -> usize {
    offset.div_ceil(to) * to
}

fn decode_rich_curves(curves: &CompressedCurves) -> Result<Vec<Vec<CurveKey>>> {
    if curves.names.is_empty() {
        return Ok(Vec::new());
    }
    let data = &curves.data;
    let f = |at: usize| -> Result<f32> {
        data.get(at..at + 4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| "curve data out of range".to_string())
    };
    let byte = |at: usize| -> Result<u8> { data.get(at).copied().ok_or_else(|| "curve data out of range".to_string()) };
    let mut out = Vec::with_capacity(curves.names.len());
    for i in 0..curves.names.len() {
        let d = i * 12;
        let format = byte(d)?;
        let time_format = byte(d + 1)?;
        let num_or_value = f(d + 4)?;
        let num_keys = i32::from_le_bytes(data.get(d + 4..d + 8).ok_or("curve data out of range")?.try_into().unwrap());
        let base = i32::from_le_bytes(data.get(d + 8..d + 12).ok_or("curve data out of range")?.try_into().unwrap());
        if format == RCCF_EMPTY || format == RCCF_CONSTANT {
            out.push(vec![CurveKey { time: 0.0, value: num_or_value, arrive: 0.0, leave: 0.0, arrive_weight: 0.0, leave_weight: 0.0, weight_mode: 0, mode: RCCF_CONSTANT }]);
            continue;
        }
        if format > RCCF_WEIGHTED || time_format > 1 || num_keys <= 0 || base < 0 {
            return Err(format!("unsupported curve format {} / {}", format, time_format));
        }
        let base = base as usize;
        let n = num_keys as usize;
        let modes_size = match format {
            RCCF_MIXED => n,
            RCCF_WEIGHTED => 2 * n,
            _ => 0,
        };
        let times_at = base + if time_format == 0 { align(modes_size, 2) } else { align(modes_size, 4) };
        let (times, keys_at): (Vec<f32>, usize) = if time_format == 0 {
            let range_at = base + align(times_at - base + n * 2, 4);
            let (min, delta) = (f(range_at)?, f(range_at + 4)?);
            let times = (0..n)
                .map(|k| {
                    let b = data.get(times_at + k * 2..times_at + k * 2 + 2).ok_or("curve data out of range")?;
                    Ok(u16::from_le_bytes([b[0], b[1]]) as f32 / 65535.0 * delta + min)
                })
                .collect::<Result<Vec<_>>>()?;
            (times, range_at + 8)
        } else {
            let times = (0..n).map(|k| f(times_at + k * 4)).collect::<Result<Vec<_>>>()?;
            (times, base + align(times_at - base + n * 4, 4))
        };
        let stride = match format {
            RCCF_LINEAR => 1,
            RCCF_WEIGHTED => 5,
            _ => 3,
        };
        let mut keys = Vec::with_capacity(n);
        for (k, &time) in times.iter().enumerate() {
            let at = keys_at + k * stride * 4;
            let mode = match format {
                RCCF_MIXED | RCCF_WEIGHTED => match byte(base + if format == RCCF_WEIGHTED { k * 2 } else { k })? {
                    0 => RCCF_LINEAR,
                    2 => RCCF_CUBIC,
                    _ => RCCF_CONSTANT,
                },
                _ => format,
            };
            let (arrive, leave) = if stride >= 3 { (f(at + 4)?, f(at + 8)?) } else { (0.0, 0.0) };
            let (arrive_weight, leave_weight, weight_mode) = if format == RCCF_WEIGHTED { (f(at + 12)?, f(at + 16)?, byte(base + k * 2 + 1)?) } else { (0.0, 0.0, 0) };
            keys.push(CurveKey { time, value: f(at)?, arrive, leave, arrive_weight, leave_weight, weight_mode, mode });
        }
        out.push(keys);
    }
    Ok(out)
}

fn eval_curve(keys: &[CurveKey], time: f32) -> f32 {
    let Some(first) = keys.first() else { return 0.0 };
    if keys.len() == 1 || time <= first.time {
        return first.value;
    }
    let last = keys[keys.len() - 1];
    if time >= last.time {
        return last.value;
    }
    let i = keys.iter().rposition(|k| k.time <= time).unwrap_or(0);
    let (a, b) = (keys[i], keys[i + 1]);
    let dt = b.time - a.time;
    if dt <= 0.0 {
        return b.value;
    }
    let u = (time - a.time) / dt;
    match a.mode {
        RCCF_CONSTANT => a.value,
        RCCF_LINEAR => a.value + (b.value - a.value) * u,
        RCCF_CUBIC if matches!(a.weight_mode, RCTWM_LEAVE | RCTWM_BOTH) || matches!(b.weight_mode, RCTWM_ARRIVE | RCTWM_BOTH) => eval_weighted(&a, &b, time),
        RCCF_CUBIC => {
            let p1 = a.value + a.leave * dt / 3.0;
            let p2 = b.value - b.arrive * dt / 3.0;
            let v = 1.0 - u;
            v * v * v * a.value + 3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u * b.value
        }
        _ => a.value,
    }
}

fn bezier(p0: f32, p1: f32, p2: f32, p3: f32, u: f32) -> f32 {
    let v = 1.0 - u;
    v * v * v * p0 + 3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u * p3
}

fn eval_weighted(a: &CurveKey, b: &CurveKey, time: f32) -> f32 {
    let dt = b.time - a.time;
    let handle = |tangent: f32, weight: f32, weighted: bool| {
        let (sin, cos) = tangent.atan().sin_cos();
        let length = if weighted { weight } else { dt * (1.0 + tangent * tangent).sqrt() / 3.0 };
        (cos * length, sin * length)
    };
    let (leave_x, leave_y) = handle(a.leave, a.leave_weight, matches!(a.weight_mode, RCTWM_LEAVE | RCTWM_BOTH));
    let (arrive_x, arrive_y) = handle(b.arrive, b.arrive_weight, matches!(b.weight_mode, RCTWM_ARRIVE | RCTWM_BOTH));
    let (x1, x2) = (a.time + leave_x, b.time - arrive_x);
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..32 {
        let mid = (low + high) * 0.5;
        if bezier(a.time, x1, x2, b.time, mid) < time {
            low = mid;
        } else {
            high = mid;
        }
    }
    bezier(a.value, a.value + leave_y, b.value - arrive_y, b.value, (low + high) * 0.5)
}

fn read_compressed(pkg: &Package) -> Result<(AnimInfo, Vec<i32>, Compressed)> {
    let export = find_export(pkg, "AnimSequence")?;
    let mut r = Reader::new(pkg.export_data(export)?);
    let props = read_object_header(pkg, export, &mut r)?;
    let mut info = info_from_props(pkg, &export.object_name, &props);
    r.skip(16)?;
    r.skip(2)?;
    if !r.bool32()? {
        return Err("animation has no compressed data".into());
    }
    let start = r.pos;
    let newer = match read_layout_4_23_plus(pkg, &mut r) {
        Ok((track_map, compressed)) => {
            if info.num_frames == 0 {
                info.num_frames = compressed.num_keys;
            }
            return Ok((info, track_map, compressed));
        }
        Err(e) => e,
    };
    let mut older = Vec::new();
    for is_4_22 in [true, false] {
        r.pos = start;
        match read_layout_4_21(pkg, &mut r, info.num_frames, is_4_22) {
            Ok((track_map, compressed)) => return Ok((info, track_map, compressed)),
            Err(e) => older.push(e),
        }
    }
    if newer.contains("ACL") {
        Err(newer)
    } else {
        Err(format!("{} / {}", newer, older.join(" / ")))
    }
}

#[derive(Default)]
struct Channel<T> {
    keys: Vec<T>,
    times: Vec<f32>,
}

#[derive(Default)]
struct Track {
    pos: Channel<[f32; 3]>,
    rot: Channel<[f32; 4]>,
    scale: Channel<[f32; 3]>,
}

fn quat_from_xyz(x: f32, y: f32, z: f32) -> [f32; 4] {
    let w2 = 1.0 - x * x - y * y - z * z;
    [x, y, z, if w2 > 0.0 { w2.sqrt() } else { 0.0 }]
}

struct KeyReader<'a> {
    r: Reader<'a>,
    num_frames: u32,
}

impl KeyReader<'_> {
    fn align4(&mut self) {
        self.r.pos = self.r.pos.div_ceil(4) * 4;
    }

    fn check_keys(&self, num_keys: usize) -> Result<()> {
        if num_keys > self.r.data.len() {
            return Err(format!("implausible key count {}", num_keys));
        }
        Ok(())
    }

    fn read_times(&mut self, num_keys: usize) -> Result<Vec<f32>> {
        if num_keys <= 1 {
            return Ok(Vec::new());
        }
        let times = if self.num_frames < 256 {
            (0..num_keys).map(|_| self.r.u8().map(|v| v as f32)).collect::<Result<Vec<_>>>()?
        } else {
            (0..num_keys).map(|_| self.r.u16().map(|v| v as f32)).collect::<Result<Vec<_>>>()?
        };
        self.align4();
        Ok(times)
    }

    fn vec3(&mut self) -> Result<[f32; 3]> {
        Ok([self.r.f32()?, self.r.f32()?, self.r.f32()?])
    }

    fn fixed16(&mut self) -> Result<f32> {
        Ok((self.r.u16()? as i32 - 32767) as f32 / 32767.0)
    }

    fn quat_fixed48(&mut self, mask: u32) -> Result<[f32; 4]> {
        let mut c = [0f32; 3];
        for (i, v) in c.iter_mut().enumerate() {
            *v = if mask & (1 << i) != 0 { self.fixed16()? } else { 0.0 };
        }
        Ok(quat_from_xyz(c[0], c[1], c[2]))
    }

    fn packed_xyz_11_11_10(&mut self, mins: [f32; 3], ranges: Option<[f32; 3]>) -> Result<[f32; 3]> {
        let packed = self.r.u32()?;
        let x = ((packed >> 21) as i32 - 1023) as f32 / 1023.0;
        let y = (((packed & 0x001f_fc00) >> 10) as i32 - 1023) as f32 / 1023.0;
        let z = ((packed & 0x3ff) as i32 - 511) as f32 / 511.0;
        Ok(match ranges {
            Some(rg) => [x * rg[0] + mins[0], y * rg[1] + mins[1], z * rg[2] + mins[2]],
            None => [x, y, z],
        })
    }

    fn quat_float32(&mut self) -> Result<[f32; 4]> {
        let packed = self.r.u32()?;
        let ux = packed >> 21;
        let uy = (packed & 0x001f_fc00) >> 10;
        let uz = packed & 0x3ff;
        let x = f32::from_bits(((((ux >> 7) & 7) + 123) << 23) | ((ux & 0x7f | 32 * (ux & 0xffff_fc00)) << 16));
        let y = f32::from_bits(((((uy >> 7) & 7) + 123) << 23) | ((uy & 0x7f | 32 * (uy & 0xffff_fc00)) << 16));
        let z = f32::from_bits(((((uz >> 6) & 7) + 123) << 23) | ((uz & 0x3f | 32 * (uz & 0xffff_fe00)) << 17));
        Ok(quat_from_xyz(x, y, z))
    }

    fn vector_interval_fixed32(&mut self, mins: [f32; 3], ranges: [f32; 3]) -> Result<[f32; 3]> {
        let packed = self.r.u32()?;
        let x = ((packed & 0x3ff) as i32 - 511) as f32 / 511.0;
        let y = (((packed & 0x001f_fc00) >> 10) as i32 - 1023) as f32 / 1023.0;
        let z = ((packed >> 21) as i32 - 1023) as f32 / 1023.0;
        Ok([x * ranges[0] + mins[0], y * ranges[1] + mins[1], z * ranges[2] + mins[2]])
    }

    fn quat(&mut self, format: u8, mins: [f32; 3], ranges: [f32; 3], mask: u32) -> Result<[f32; 4]> {
        match format {
            ACF_FLOAT96_NO_W => {
                let v = self.vec3()?;
                Ok(quat_from_xyz(v[0], v[1], v[2]))
            }
            ACF_FIXED48_NO_W => self.quat_fixed48(mask),
            ACF_FIXED32_NO_W => {
                let v = self.packed_xyz_11_11_10([0.0; 3], None)?;
                Ok(quat_from_xyz(v[0], v[1], v[2]))
            }
            ACF_INTERVAL_FIXED32_NO_W => {
                let v = self.packed_xyz_11_11_10(mins, Some(ranges))?;
                Ok(quat_from_xyz(v[0], v[1], v[2]))
            }
            ACF_FLOAT32_NO_W => self.quat_float32(),
            ACF_IDENTITY => Ok([0.0, 0.0, 0.0, 1.0]),
            _ => Err(format!("unknown rotation format {}", format)),
        }
    }

    fn read_mins_ranges(&mut self, mask: u32) -> Result<([f32; 3], [f32; 3])> {
        let mut mins = [0f32; 3];
        let mut ranges = [0f32; 3];
        for i in 0..3 {
            if mask & (1 << i) != 0 {
                mins[i] = self.r.f32()?;
                ranges[i] = self.r.f32()?;
            }
        }
        Ok((mins, ranges))
    }

    fn per_track_header(&mut self) -> Result<(u8, u32, usize)> {
        let packed = self.r.u32()?;
        let num_keys = (packed & 0x00ff_ffff) as usize;
        self.check_keys(num_keys)?;
        Ok(((packed >> 28) as u8, (packed >> 24) & 0xf, num_keys))
    }

    fn per_track_quat(&mut self) -> Result<Channel<[f32; 4]>> {
        let (format, mask, num_keys) = self.per_track_header()?;
        let (mins, ranges) = if format == ACF_INTERVAL_FIXED32_NO_W { self.read_mins_ranges(mask)? } else { ([0.0; 3], [0.0; 3]) };
        let format = if format == ACF_NONE { ACF_FLOAT96_NO_W } else { format };
        let keys = (0..num_keys).map(|_| self.quat(format, mins, ranges, mask)).collect::<Result<Vec<_>>>()?;
        self.align4();
        let times = if mask & 8 != 0 { self.read_times(num_keys)? } else { Vec::new() };
        Ok(Channel { keys, times })
    }

    fn per_track_vector(&mut self, identity: [f32; 3]) -> Result<Channel<[f32; 3]>> {
        let (format, mask, num_keys) = self.per_track_header()?;
        let (mins, ranges) = if format == ACF_INTERVAL_FIXED32_NO_W { self.read_mins_ranges(mask)? } else { ([0.0; 3], [0.0; 3]) };
        let mut keys = Vec::with_capacity(num_keys);
        for _ in 0..num_keys {
            keys.push(match format {
                ACF_NONE | ACF_FLOAT96_NO_W if mask & 7 == 0 => self.vec3()?,
                ACF_NONE | ACF_FLOAT96_NO_W => {
                    let mut v = [0f32; 3];
                    for (i, c) in v.iter_mut().enumerate() {
                        if mask & (1 << i) != 0 {
                            *c = self.r.f32()?;
                        }
                    }
                    v
                }
                ACF_INTERVAL_FIXED32_NO_W => self.vector_interval_fixed32(mins, ranges)?,
                ACF_FIXED48_NO_W => {
                    let mut v = [0f32; 3];
                    for (i, c) in v.iter_mut().enumerate() {
                        if mask & (1 << i) != 0 {
                            *c = self.r.u16()? as f32 - 255.0;
                        }
                    }
                    v
                }
                ACF_IDENTITY => identity,
                _ => return Err(format!("unknown vector format {}", format)),
            });
        }
        self.align4();
        let times = if mask & 8 != 0 { self.read_times(num_keys)? } else { Vec::new() };
        Ok(Channel { keys, times })
    }

    fn key_lerp_vector(&mut self, offset: i32, num_keys: usize, format: u8, has_times: bool) -> Result<Channel<[f32; 3]>> {
        self.check_keys(num_keys)?;
        self.r.pos = offset as usize;
        let format = if num_keys == 1 { ACF_NONE } else { format };
        let (mins, ranges) = if format == ACF_INTERVAL_FIXED32_NO_W { (self.vec3()?, self.vec3()?) } else { ([0.0; 3], [0.0; 3]) };
        let mut keys = Vec::with_capacity(num_keys);
        for _ in 0..num_keys {
            keys.push(match format {
                ACF_NONE | ACF_FLOAT96_NO_W => self.vec3()?,
                ACF_INTERVAL_FIXED32_NO_W => self.vector_interval_fixed32(mins, ranges)?,
                ACF_FIXED48_NO_W => [self.fixed16()? * 128.0, self.fixed16()? * 128.0, self.fixed16()? * 128.0],
                ACF_IDENTITY => [0.0; 3],
                _ => return Err(format!("unknown vector format {}", format)),
            });
        }
        self.align4();
        let times = if has_times { self.read_times(num_keys)? } else { Vec::new() };
        Ok(Channel { keys, times })
    }

    fn key_lerp_quat(&mut self, offset: i32, num_keys: usize, format: u8, has_times: bool) -> Result<Channel<[f32; 4]>> {
        self.check_keys(num_keys)?;
        self.r.pos = offset as usize;
        let format = if num_keys == 1 { ACF_FLOAT96_NO_W } else { format };
        let (mins, ranges) = if format == ACF_INTERVAL_FIXED32_NO_W { (self.vec3()?, self.vec3()?) } else { ([0.0; 3], [0.0; 3]) };
        let mut keys = Vec::with_capacity(num_keys);
        for _ in 0..num_keys {
            keys.push(if format == ACF_NONE {
                [self.r.f32()?, self.r.f32()?, self.r.f32()?, self.r.f32()?]
            } else {
                self.quat(format, mins, ranges, 7)?
            });
        }
        self.align4();
        let times = if has_times { self.read_times(num_keys)? } else { Vec::new() };
        Ok(Channel { keys, times })
    }
}

fn decode_track(c: &Compressed, kr: &mut KeyReader, track: usize) -> Result<Track> {
    let mut out = Track::default();
    let offsets = &c.track_offsets;
    let scale_valid = c.scale_strip_size > 0 && !c.scale_offsets.is_empty();
    if c.key_format == AKF_PER_TRACK_COMPRESSION {
        let (trans, rot) = (offsets[track * 2], offsets[track * 2 + 1]);
        let scale = if scale_valid { c.scale_offsets[track] } else { -1 };
        if trans != -1 {
            kr.r.pos = trans as usize;
            out.pos = kr.per_track_vector([0.0; 3])?;
        }
        if rot != -1 {
            kr.r.pos = rot as usize;
            out.rot = kr.per_track_quat()?;
        }
        if scale != -1 {
            kr.r.pos = scale as usize;
            out.scale = kr.per_track_vector([1.0; 3])?;
        }
    } else {
        let has_times = c.key_format == AKF_VARIABLE_KEY_LERP;
        let o = &offsets[track * 4..track * 4 + 4];
        if o[1] > 0 {
            out.pos = kr.key_lerp_vector(o[0], o[1] as usize, c.trans_format, has_times)?;
        }
        if scale_valid {
            let (offset, keys) = (c.scale_offsets[track * 2], c.scale_offsets[track * 2 + 1]);
            if keys > 0 && offset > 0 {
                out.scale = kr.key_lerp_vector(offset, keys as usize, c.scale_format, has_times)?;
            }
        }
        if o[3] > 0 {
            out.rot = kr.key_lerp_quat(o[2], o[3] as usize, c.rot_format, has_times)?;
        }
    }
    Ok(out)
}

fn sample<const N: usize>(ch: &Channel<[f32; N]>, frame: f32, num_frames: u32, is_quat: bool) -> [f32; N] {
    let n = ch.keys.len();
    if n == 1 {
        return ch.keys[0];
    }
    let (i, t) = if ch.times.len() == n {
        match ch.times.iter().position(|&k| k > frame) {
            Some(0) => (0, 0.0),
            None => (n - 1, 0.0),
            Some(next) => {
                let (a, b) = (ch.times[next - 1], ch.times[next]);
                (next - 1, if b > a { (frame - a) / (b - a) } else { 0.0 })
            }
        }
    } else {
        let pos = if num_frames > 1 { frame * (n - 1) as f32 / (num_frames - 1) as f32 } else { 0.0 };
        let i = (pos.floor() as usize).min(n - 1);
        (i, pos - i as f32)
    };
    let a = ch.keys[i];
    if i + 1 >= n || t <= 0.0 {
        return a;
    }
    let mut b = ch.keys[i + 1];
    if is_quat {
        let dot: f32 = (0..N).map(|k| a[k] * b[k]).sum();
        if dot < 0.0 {
            b.iter_mut().for_each(|v| *v = -*v);
        }
    }
    let mut out = [0f32; N];
    for k in 0..N {
        out[k] = a[k] + (b[k] - a[k]) * t;
    }
    if is_quat {
        let len = out.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-8);
        out.iter_mut().for_each(|v| *v /= len);
    }
    out
}

struct RawTrack {
    bone_index: i32,
    channels: [bool; 3],
    frames: Vec<([f32; 3], [f32; 4], [f32; 3])>,
}

fn keyed_tracks(info: &AnimInfo, track_map: &[i32], compressed: &Compressed) -> Result<Vec<RawTrack>> {
    let per_track = if compressed.key_format == AKF_PER_TRACK_COMPRESSION { 2 } else { 4 };
    if compressed.track_offsets.len() < track_map.len() * per_track {
        return Err("track offsets do not match the track count".into());
    }
    let scale_strip = if compressed.key_format == AKF_PER_TRACK_COMPRESSION { 1 } else { 2 };
    if compressed.scale_strip_size > 0 && !compressed.scale_offsets.is_empty() && compressed.scale_offsets.len() < track_map.len() * scale_strip {
        return Err("scale offsets do not match the track count".into());
    }
    let mut kr = KeyReader { r: Reader::new(&compressed.stream), num_frames: info.num_frames };
    let mut tracks = Vec::with_capacity(track_map.len());
    for (track, &bone_index) in track_map.iter().enumerate() {
        let decoded = decode_track(compressed, &mut kr, track)?;
        let frames = (0..info.num_frames)
            .map(|f| {
                let frame = f as f32;
                let p = if decoded.pos.keys.is_empty() { [0.0; 3] } else { sample(&decoded.pos, frame, info.num_frames, false) };
                let q = if decoded.rot.keys.is_empty() { [0.0, 0.0, 0.0, 1.0] } else { sample(&decoded.rot, frame, info.num_frames, true) };
                let s = if decoded.scale.keys.is_empty() { [1.0; 3] } else { sample(&decoded.scale, frame, info.num_frames, false) };
                (p, q, s)
            })
            .collect();
        let channels = [!decoded.pos.keys.is_empty(), !decoded.rot.keys.is_empty(), !decoded.scale.keys.is_empty()];
        tracks.push(RawTrack { bone_index, channels, frames });
    }
    Ok(tracks)
}

fn acl_tracks(info: &mut AnimInfo, track_map: &[i32], compressed: &Compressed) -> Result<Vec<RawTrack>> {
    let clip = decode_transform_tracks(&compressed.stream)?;
    if clip.tracks.len() != track_map.len() {
        return Err(format!("ACL has {} tracks but the animation maps {}", clip.tracks.len(), track_map.len()));
    }
    info.num_frames = clip.num_frames;
    if info.length <= 0.0 && clip.sample_rate > 0.0 {
        info.length = clip.num_frames.saturating_sub(1) as f32 / clip.sample_rate;
    }
    let n = clip.num_frames as usize;
    Ok(clip
        .tracks
        .into_iter()
        .zip(track_map)
        .map(|(t, &bone_index)| RawTrack {
            bone_index,
            channels: [t.translation.is_some(), t.rotation.is_some(), t.scale.is_some()],
            frames: (0..n)
                .map(|f| {
                    (
                        t.translation.as_ref().map_or([0.0; 3], |v| v[f]),
                        t.rotation.as_ref().map_or([0.0, 0.0, 0.0, 1.0], |v| v[f]),
                        t.scale.as_ref().map_or([1.0; 3], |v| v[f]),
                    )
                })
                .collect(),
        })
        .collect())
}

pub fn read_animation(pkg: &Package, skeleton_bones: &[String]) -> Result<SampledAnimation> {
    let (mut info, track_map, compressed) = read_compressed(pkg)?;
    let tracks = if compressed.acl {
        acl_tracks(&mut info, &track_map, &compressed)?
    } else {
        keyed_tracks(&info, &track_map, &compressed)?
    };
    if info.num_frames == 0 {
        return Err("animation has no frames".into());
    }
    let frames = info.num_frames as usize;
    let mut bones = Vec::new();
    let mut channels = Vec::new();
    let mut samples = Vec::with_capacity(tracks.len() * frames * 10);
    for track in tracks {
        let Some(bone) = skeleton_bones.get(track.bone_index as usize) else { continue };
        bones.push(bone.clone());
        channels.push(track.channels);
        let mut previous: Option<[f32; 4]> = None;
        for (p, mut q, s) in track.frames {
            if let Some(prev) = previous {
                if prev.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>() < 0.0 {
                    q.iter_mut().for_each(|v| *v = -*v);
                }
            }
            previous = Some(q);
            let pv = convert_vector(p[0], p[1], p[2]);
            let qv = convert_vector(q[0], q[1], q[2]);
            let sv = convert_vector(s[0], s[1], s[2]);
            samples.extend_from_slice(&[
                pv[0] * UNIT_SCALE,
                pv[1] * UNIT_SCALE,
                pv[2] * UNIT_SCALE,
                qv[0],
                qv[1],
                qv[2],
                q[3],
                sv[0],
                sv[1],
                sv[2],
            ]);
        }
    }
    let fps = if info.length > 0.0 && frames > 1 { (frames - 1) as f32 / info.length } else { 30.0 };
    let curve_samples = sample_curves(&compressed.curves, frames, fps).unwrap_or_default();
    let curves = if curve_samples.is_empty() { Vec::new() } else { compressed.curves.names.clone() };
    Ok(SampledAnimation { info, bones, channels, samples, curves, curve_samples })
}

fn sample_curves(curves: &CompressedCurves, frames: usize, fps: f32) -> Result<Vec<f32>> {
    if curves.names.is_empty() {
        return Ok(Vec::new());
    }
    if curves.codec.contains("ACL") {
        let decoded = decode_scalar_tracks(&curves.data)?;
        if decoded.tracks.len() != curves.names.len() {
            return Err(format!("ACL has {} curves but the animation names {}", decoded.tracks.len(), curves.names.len()));
        }
        let mut out = Vec::with_capacity(curves.names.len() * frames);
        for values in &decoded.tracks {
            if values.is_empty() {
                out.resize(out.len() + frames, 0.0);
                continue;
            }
            out.extend((0..frames).map(|f| {
                let pos = (f as f32 / fps * decoded.sample_rate).clamp(0.0, (values.len() - 1) as f32);
                let i = (pos.floor() as usize).min(values.len() - 1);
                let next = values[(i + 1).min(values.len() - 1)];
                values[i] + (next - values[i]) * (pos - i as f32)
            }));
        }
        return Ok(out);
    }
    let decoded = decode_rich_curves(curves)?;
    Ok(decoded.iter().flat_map(|keys| (0..frames).map(move |f| eval_curve(keys, f as f32 / fps))).collect())
}
