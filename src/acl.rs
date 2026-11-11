use crate::reader::Result;

const TRACKS_TAG: u32 = 0xac11_ac11;
const TRACK_TYPE_QVVF: u8 = 12;
const VERSION_FIRST: u16 = 7;
const VERSION_LATEST: u16 = 10;
const VERSION_RAW_BITS_31: u16 = 9;
const ROT_FULL: u32 = 0;
const ROT_DROP_W_FULL: u32 = 2;
const ROT_DROP_W_VARIABLE: u32 = 3;
const SUB_TRACK_CONSTANT: u32 = 1;
const SUB_TRACK_ANIMATED: u32 = 2;
const TRANSFORM_HEADER: usize = 32;
const TRANSFORM_HEADER_SIZE: usize = 52;
const MAX_SAMPLES: usize = 1 << 24;
const MAX_TRACK_SAMPLES: usize = 1 << 28;

pub struct AclTrack {
    pub rotation: Option<Vec<[f32; 4]>>,
    pub translation: Option<Vec<[f32; 3]>>,
    pub scale: Option<Vec<[f32; 3]>>,
}

pub struct AclClip {
    pub num_frames: u32,
    pub sample_rate: f32,
    pub tracks: Vec<AclTrack>,
}

struct Data<'a>(&'a [u8]);

impl Data<'_> {
    fn u8(&self, at: usize) -> Result<u8> {
        self.0.get(at).copied().ok_or_else(|| format!("ACL data out of range at {}", at))
    }

    fn u16(&self, at: usize) -> Result<u16> {
        let b = self.0.get(at..at + 2).ok_or_else(|| format!("ACL data out of range at {}", at))?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&self, at: usize) -> Result<u32> {
        let b = self.0.get(at..at + 4).ok_or_else(|| format!("ACL data out of range at {}", at))?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f32(&self, at: usize) -> Result<f32> {
        self.u32(at).map(f32::from_bits)
    }

    fn vec3(&self, at: usize) -> Result<[f32; 3]> {
        Ok([self.f32(at)?, self.f32(at + 4)?, self.f32(at + 8)?])
    }

    fn bits(&self, bit_offset: usize, num_bits: u32) -> Result<u32> {
        if num_bits == 0 {
            return Ok(0);
        }
        if num_bits > 32 {
            return Err(format!("invalid ACL bit width {}", num_bits));
        }
        let start = bit_offset / 8;
        let end = (bit_offset + num_bits as usize).div_ceil(8);
        if end > self.0.len() {
            return Err(format!("ACL bit stream out of range at bit {}", bit_offset));
        }
        let mut raw = [0u8; 8];
        let n = (end - start).min(8);
        raw[..n].copy_from_slice(&self.0[start..start + n]);
        let value = u64::from_be_bytes(raw) << (bit_offset % 8);
        Ok((value >> (64 - num_bits)) as u32)
    }

    fn unorm(&self, bit_offset: usize, num_bits: u32) -> Result<f32> {
        Ok(self.bits(bit_offset, num_bits)? as f32 / ((1u64 << num_bits) - 1) as f32)
    }
}

fn check_counts(bytes: &[u8], num_tracks: usize, num_samples: usize) -> Result<()> {
    if num_tracks > bytes.len() || num_samples > MAX_SAMPLES || num_tracks.saturating_mul(num_samples) > MAX_TRACK_SAMPLES {
        return Err(format!("implausible ACL size ({} tracks, {} samples)", num_tracks, num_samples));
    }
    Ok(())
}

fn quat_from_positive_w(v: [f32; 3]) -> [f32; 4] {
    let w = (1.0 - v[0] * v[0] - v[1] * v[1] - v[2] * v[2]).abs().sqrt();
    [v[0], v[1], v[2], w]
}

fn normalize(q: [f32; 4]) -> [f32; 4] {
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    if len > 0.0 { q.map(|c| c / len) } else { [0.0, 0.0, 0.0, 1.0] }
}

fn mul_add(v: [f32; 3], extent: [f32; 3], min: [f32; 3]) -> [f32; 3] {
    [v[0] * extent[0] + min[0], v[1] * extent[1] + min[1], v[2] * extent[2] + min[2]]
}

struct Segment {
    start: u32,
    sample_indices: Option<u32>,
    pose_bits: usize,
    rotation_bits: usize,
    translation_bits: usize,
    format_data: usize,
    range_data: usize,
    animated_data: usize,
}

struct Layout {
    rotation_format: u32,
    translation_variable: bool,
    scale_variable: bool,
    has_scale: bool,
    has_segments: bool,
    raw_bits: u32,
    num_rotations: usize,
    num_translations: usize,
    num_scales: usize,
    clip_range_data: usize,
}

struct Keyframe {
    rotations: Vec<[f32; 4]>,
    translations: Vec<[f32; 3]>,
    scales: Vec<[f32; 3]>,
}

fn decode_keyframe(d: &Data, l: &Layout, seg: &Segment, stored_index: usize) -> Result<Keyframe> {
    let mut bit = seg.animated_data * 8 + stored_index * seg.pose_bits;
    let mut format = seg.format_data;
    let mut seg_range = seg.range_data;
    let mut clip_range = l.clip_range_data;
    let mut rotations = Vec::with_capacity(l.num_rotations);
    let variable = l.rotation_format == ROT_DROP_W_VARIABLE;
    for group_start in (0..l.num_rotations).step_by(4) {
        let group = (l.num_rotations - group_start).min(4);
        for i in 0..group {
            let q = if variable {
                let num_bits = d.u8(format + i)? as u32;
                let (mut v, ignore_segment, ignore_clip) = if num_bits == 0 {
                    let byte = |k: usize| d.u8(seg_range + i + k * 4).map(|b| b as u32);
                    let v = [(byte(0)? << 8) | byte(1)?, (byte(2)? << 8) | byte(3)?, (byte(4)? << 8) | byte(5)?];
                    (v.map(|c| c as f32 / 65535.0), true, false)
                } else if num_bits == l.raw_bits {
                    let v = [d.bits(bit, 32)?, d.bits(bit + 32, 32)?, d.bits(bit + 64, 32)?];
                    bit += 96;
                    (v.map(f32::from_bits), true, true)
                } else {
                    let n = num_bits as usize;
                    let v = [d.unorm(bit, num_bits)?, d.unorm(bit + n, num_bits)?, d.unorm(bit + 2 * n, num_bits)?];
                    bit += 3 * n;
                    (v, false, false)
                };
                if l.has_segments && !ignore_segment {
                    let byte = |k: usize| d.u8(seg_range + i + k * 4).map(|b| b as f32 / 255.0);
                    v = mul_add(v, [byte(3)?, byte(4)?, byte(5)?], [byte(0)?, byte(1)?, byte(2)?]);
                }
                if !ignore_clip {
                    let at = |k: usize| d.f32(clip_range + (k * group + i) * 4);
                    v = mul_add(v, [at(3)?, at(4)?, at(5)?], [at(0)?, at(1)?, at(2)?]);
                }
                quat_from_positive_w(v)
            } else if l.rotation_format == ROT_FULL {
                let q = [d.bits(bit, 32)?, d.bits(bit + 32, 32)?, d.bits(bit + 64, 32)?, d.bits(bit + 96, 32)?];
                bit += 128;
                q.map(f32::from_bits)
            } else {
                let v = [d.bits(bit, 32)?, d.bits(bit + 32, 32)?, d.bits(bit + 64, 32)?];
                bit += 96;
                quat_from_positive_w(v.map(f32::from_bits))
            };
            rotations.push(normalize(q));
        }
        if variable {
            format += 4;
            seg_range += 24;
            clip_range += group * 24;
        }
    }
    let padded_rotations = if variable { l.num_rotations.div_ceil(4) * 4 } else { 0 };
    let translation_start = (
        seg.format_data + padded_rotations,
        seg.range_data + padded_rotations * 6,
        l.clip_range_data + if variable { l.num_rotations * 24 } else { 0 },
    );
    let translation_size = if l.translation_variable { l.num_translations } else { 0 };
    let scale_start = (
        translation_start.0 + translation_size,
        translation_start.1 + translation_size * 6,
        translation_start.2 + translation_size * 24,
    );
    let read_vectors = |count: usize, variable: bool, mut bit: usize, start: (usize, usize, usize)| -> Result<Vec<[f32; 3]>> {
        let (mut format, mut seg_range, mut clip_range) = start;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            if !variable {
                out.push(d.vec3_bits(bit)?);
                bit += 96;
                continue;
            }
            let num_bits = d.u8(format)? as u32;
            format += 1;
            let (mut v, ignore_segment, ignore_clip) = if num_bits == 0 {
                let v = [d.u16(seg_range)?, d.u16(seg_range + 2)?, d.u16(seg_range + 4)?];
                seg_range += 6;
                (v.map(|c| c as f32 / 65535.0), true, false)
            } else if num_bits == l.raw_bits {
                let v = d.vec3_bits(bit)?;
                bit += 96;
                seg_range += 6;
                (v, true, true)
            } else {
                let n = num_bits as usize;
                let v = [d.unorm(bit, num_bits)?, d.unorm(bit + n, num_bits)?, d.unorm(bit + 2 * n, num_bits)?];
                bit += 3 * n;
                (v, false, false)
            };
            if l.has_segments && !ignore_segment {
                let byte = |k: usize| d.u8(seg_range + k).map(|b| b as f32 / 255.0);
                v = mul_add(v, [byte(3)?, byte(4)?, byte(5)?], [byte(0)?, byte(1)?, byte(2)?]);
                seg_range += 6;
            }
            if !ignore_clip {
                v = mul_add(v, d.vec3(clip_range + 12)?, d.vec3(clip_range)?);
            }
            clip_range += 24;
            out.push(v);
        }
        Ok(out)
    };
    let frame_bit = seg.animated_data * 8 + stored_index * seg.pose_bits;
    let translations = read_vectors(l.num_translations, l.translation_variable, frame_bit + seg.rotation_bits, translation_start)?;
    let scales = if l.has_scale {
        read_vectors(l.num_scales, l.scale_variable, frame_bit + seg.rotation_bits + seg.translation_bits, scale_start)?
    } else {
        Vec::new()
    };
    Ok(Keyframe { rotations, translations, scales })
}

impl Data<'_> {
    fn vec3_bits(&self, bit: usize) -> Result<[f32; 3]> {
        Ok([self.bits(bit, 32)?, self.bits(bit + 32, 32)?, self.bits(bit + 64, 32)?].map(f32::from_bits))
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn nlerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let sign = if a.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>() < 0.0 { -1.0 } else { 1.0 };
    normalize([0, 1, 2, 3].map(|k| a[k] + (b[k] * sign - a[k]) * t))
}

const TRACK_TYPE_FLOAT1F: u8 = 0;
const SCALAR_HEADER: usize = 32;
const BIT_RATE_NUM_BITS_V0: [u8; 19] = [0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 32];
const BIT_RATE_NUM_BITS: [u8; 25] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 32];

pub struct AclCurves {
    pub sample_rate: f32,
    pub tracks: Vec<Vec<f32>>,
}

pub fn decode_scalar_tracks(bytes: &[u8]) -> Result<AclCurves> {
    let d = Data(bytes);
    if d.u32(8)? != TRACKS_TAG {
        return Err("not an ACL compressed tracks buffer".into());
    }
    let version = d.u16(12)?;
    if !(VERSION_FIRST..=VERSION_LATEST).contains(&version) {
        return Err(format!("unsupported ACL version {}", version));
    }
    if d.u8(15)? != TRACK_TYPE_FLOAT1F {
        return Err("ACL curves are not float1f tracks".into());
    }
    let num_tracks = d.u32(16)? as usize;
    let num_samples = d.u32(20)? as usize;
    check_counts(bytes, num_tracks, num_samples)?;
    let sample_rate = d.f32(24)?;
    let wrap = version > VERSION_FIRST && d.u32(28)? & (1 << 30) != 0;
    let h = SCALAR_HEADER;
    let bits_per_frame = d.u32(h)? as usize;
    let metadata = h + d.u32(h + 4)? as usize;
    let mut constants = h + d.u32(h + 8)? as usize;
    let mut ranges = h + d.u32(h + 12)? as usize;
    let animated = (h + d.u32(h + 16)? as usize) * 8;
    let table: &[u8] = if version == VERSION_FIRST { &BIT_RATE_NUM_BITS_V0 } else { &BIT_RATE_NUM_BITS };
    let num_out = if wrap && num_samples > 0 { num_samples + 1 } else { num_samples };
    let mut track_bit = 0usize;
    let mut tracks = Vec::with_capacity(num_tracks);
    for track in 0..num_tracks {
        let bit_rate = d.u8(metadata + track)? as usize;
        let num_bits = *table.get(bit_rate).ok_or_else(|| format!("invalid ACL bit rate {}", bit_rate))? as u32;
        let values = if num_bits == 0 {
            let v = d.f32(constants)?;
            constants += 4;
            vec![v; num_out]
        } else {
            let range = if num_bits == 32 {
                None
            } else {
                let r = (d.f32(ranges)?, d.f32(ranges + 4)?);
                ranges += 8;
                Some(r)
            };
            (0..num_out)
                .map(|f| {
                    let bit = animated + (if f == num_samples { 0 } else { f }) * bits_per_frame + track_bit;
                    Ok(match range {
                        None => f32::from_bits(d.bits(bit, 32)?),
                        Some((min, extent)) => d.unorm(bit, num_bits)? * extent + min,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        if num_bits != 0 {
            track_bit += num_bits as usize;
        }
        tracks.push(values);
    }
    Ok(AclCurves { sample_rate, tracks })
}

pub fn num_frames(bytes: &[u8]) -> Option<u32> {
    let d = Data(bytes);
    if d.u32(8).ok()? != TRACKS_TAG {
        return None;
    }
    let num_samples = d.u32(20).ok()?;
    let wrap = d.u16(12).ok()? > VERSION_FIRST && d.u32(28).ok()? & (1 << 30) != 0;
    Some(if wrap && num_samples > 0 { num_samples + 1 } else { num_samples })
}

pub fn decode_transform_tracks(bytes: &[u8]) -> Result<AclClip> {
    let d = Data(bytes);
    if d.u32(8)? != TRACKS_TAG {
        return Err("not an ACL compressed tracks buffer".into());
    }
    let version = d.u16(12)?;
    if !(VERSION_FIRST..=VERSION_LATEST).contains(&version) {
        return Err(format!("unsupported ACL version {}", version));
    }
    if d.u8(15)? != TRACK_TYPE_QVVF {
        return Err("ACL buffer does not contain transform tracks".into());
    }
    let num_tracks = d.u32(16)? as usize;
    let num_samples = d.u32(20)? as usize;
    check_counts(bytes, num_tracks, num_samples)?;
    let sample_rate = d.f32(24)?;
    let misc = d.u32(28)?;
    if misc & (1 << 8) != 0 {
        return Err("ACL animations streamed from a database are not supported".into());
    }
    let has_stripped_keyframes = misc & (1 << 10) != 0;
    let wrap = version > VERSION_FIRST && misc & (1 << 30) != 0;
    let rotation_format = (misc >> 4) & 15;
    if ![ROT_FULL, ROT_DROP_W_FULL, ROT_DROP_W_VARIABLE].contains(&rotation_format) {
        return Err(format!("unsupported ACL rotation format {}", rotation_format));
    }
    let t = TRANSFORM_HEADER;
    let at = |field: usize| d.u32(t + field * 4).map(|v| v as usize);
    let num_segments = at(0)?;
    let layout = Layout {
        rotation_format,
        translation_variable: misc & (1 << 3) != 0,
        scale_variable: misc & (1 << 2) != 0,
        has_scale: misc & 1 != 0,
        has_segments: num_segments > 1,
        raw_bits: if version >= VERSION_RAW_BITS_31 { 31 } else { 32 },
        num_rotations: at(2)?,
        num_translations: at(3)?,
        num_scales: at(4)?,
        clip_range_data: t + at(12)?,
    };
    let num_constant = [at(5)?, at(6)?, at(7)?];
    let segment_headers = t + at(9)?;
    let sub_track_types = t + at(10)?;
    let constant_data = t + at(11)?;
    let num_variable = at(1)?;
    let mut segments = Vec::with_capacity(num_segments.min(bytes.len()));
    let header_size = if has_stripped_keyframes { 20 } else { 16 };
    for s in 0..num_segments {
        let h = segment_headers + s * header_size;
        let start = if num_segments > 1 { d.u32(t + TRANSFORM_HEADER_SIZE + s * 4)? } else { 0 };
        let format_data = t + d.u32(h + 12)? as usize;
        let range_data = (format_data + num_variable).div_ceil(2) * 2;
        let range_size = if num_segments > 1 { 6 * num_variable } else { 0 };
        segments.push(Segment {
            start,
            sample_indices: if has_stripped_keyframes { Some(d.u32(h + 16)?) } else { None },
            pose_bits: d.u32(h)? as usize,
            rotation_bits: d.u32(h + 4)? as usize,
            translation_bits: d.u32(h + 8)? as usize,
            format_data,
            range_data,
            animated_data: (range_data + range_size).div_ceil(4) * 4,
        });
    }
    let mut keyframes: Vec<Option<Keyframe>> = Vec::with_capacity(num_samples);
    for frame in 0..num_samples as u32 {
        let s = segments.iter().rposition(|s| s.start <= frame).ok_or("ACL segment lookup failed")?;
        let seg = &segments[s];
        let local = frame - seg.start;
        let stored = match seg.sample_indices {
            Some(indices) => {
                if local >= 32 || indices & (1u32 << (31 - local)) == 0 {
                    keyframes.push(None);
                    continue;
                }
                (indices & !(u32::MAX >> local)).count_ones() as usize
            }
            None => local as usize,
        };
        keyframes.push(Some(decode_keyframe(&d, &layout, seg, stored)?));
    }
    let known: Vec<usize> = (0..num_samples).filter(|&f| keyframes[f].is_some()).collect();
    if known.is_empty() && layout.num_rotations + layout.num_translations + layout.num_scales > 0 {
        return Err("ACL clip has no stored keyframes".into());
    }
    let keyframe_at = |frame: usize| -> (&Keyframe, &Keyframe, f32) {
        let next = known.partition_point(|&k| k <= frame);
        let a = known[next.saturating_sub(1)];
        let b = known.get(next).copied().unwrap_or(a);
        let t = if b > a && frame > a { (frame - a) as f32 / (b - a) as f32 } else { 0.0 };
        (keyframes[a].as_ref().unwrap(), keyframes[b].as_ref().unwrap(), t)
    };
    let num_out = if wrap && num_samples > 0 { num_samples + 1 } else { num_samples };
    let source_frame = |f: usize| if f == num_samples { 0 } else { f };
    let entries = num_tracks.div_ceil(16);
    let sub_type = |kind: usize, track: usize| -> Result<u32> {
        let packed = d.u32(sub_track_types + (kind * entries + track / 16) * 4)?;
        Ok((packed >> ((15 - track % 16) * 2)) & 3)
    };
    let mut constant_rotation = constant_data;
    let packed_rotation_size = if rotation_format == ROT_FULL { 16 } else { 12 };
    let mut constant_translation = constant_data + packed_rotation_size * num_constant[0];
    let mut constant_scale = constant_translation + 12 * num_constant[1];
    let (mut constant_rotation_index, mut animated) = (0usize, [0usize; 3]);
    let mut tracks = Vec::with_capacity(num_tracks);
    for track in 0..num_tracks {
        let rotation = match sub_type(0, track)? {
            SUB_TRACK_CONSTANT => {
                let q = if rotation_format == ROT_FULL {
                    let q = [d.f32(constant_rotation)?, d.f32(constant_rotation + 4)?, d.f32(constant_rotation + 8)?, d.f32(constant_rotation + 12)?];
                    constant_rotation += 16;
                    q
                } else {
                    let group_start = constant_rotation_index / 4 * 4;
                    let group = (num_constant[0] - group_start).min(4);
                    let base = constant_data + group_start * 12 + constant_rotation_index % 4 * 4;
                    quat_from_positive_w([d.f32(base)?, d.f32(base + group * 4)?, d.f32(base + group * 8)?])
                };
                constant_rotation_index += 1;
                Some(vec![normalize(q); num_out])
            }
            SUB_TRACK_ANIMATED => {
                let i = animated[0];
                if i >= layout.num_rotations {
                    return Err("ACL sub-track counts do not match the header".into());
                }
                animated[0] += 1;
                Some((0..num_out).map(|f| {
                    let (a, b, t) = keyframe_at(source_frame(f));
                    nlerp(a.rotations[i], b.rotations[i], t)
                }).collect())
            }
            _ => None,
        };
        let mut vector = |kind: usize, constant: &mut usize| -> Result<Option<Vec<[f32; 3]>>> {
            Ok(match sub_type(kind, track)? {
                SUB_TRACK_CONSTANT => {
                    let v = d.vec3(*constant)?;
                    *constant += 12;
                    Some(vec![v; num_out])
                }
                SUB_TRACK_ANIMATED => {
                    let i = animated[kind];
                    if i >= if kind == 1 { layout.num_translations } else { layout.num_scales } {
                        return Err("ACL sub-track counts do not match the header".into());
                    }
                    animated[kind] += 1;
                    Some((0..num_out).map(|f| {
                        let (a, b, t) = keyframe_at(source_frame(f));
                        let (va, vb) = if kind == 1 { (a.translations[i], b.translations[i]) } else { (a.scales[i], b.scales[i]) };
                        lerp3(va, vb, t)
                    }).collect())
                }
                _ => None,
            })
        };
        let translation = vector(1, &mut constant_translation)?;
        let scale = if layout.has_scale { vector(2, &mut constant_scale)? } else { None };
        tracks.push(AclTrack { rotation, translation, scale });
    }
    if animated != [layout.num_rotations, layout.num_translations, if layout.has_scale { layout.num_scales } else { 0 }] {
        return Err("ACL sub-track counts do not match the header".into());
    }
    Ok(AclClip { num_frames: num_out as u32, sample_rate, tracks })
}
