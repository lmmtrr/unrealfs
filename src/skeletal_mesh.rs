use crate::package::{ObjectExport, Package};
use crate::properties::{prop_name, prop_object, prop_vector, read_object_header};
use crate::bulk::read_bulk_data;
use crate::reader::{Reader, Result};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    pub minor: u32,
    pub section_extra: usize,
    pub ue5: bool,
    pub material_extra: usize,
}

impl Profile {
    pub fn candidates(ue5: bool) -> Vec<Profile> {
        let mut out = Vec::new();
        if ue5 {
            for section_extra in [4, 0, 8] {
                for material_extra in [4, 0] {
                    out.push(Profile { minor: 27, section_extra, ue5, material_extra });
                }
            }
            return out;
        }
        for minor in [26, 25, 27, 24, 23, 22, 21, 20, 19] {
            for section_extra in [0, 4] {
                out.push(Profile { minor, section_extra, ue5, material_extra: 0 });
            }
        }
        out
    }

    pub fn label(&self) -> String {
        let base = if self.ue5 { "UE5".to_string() } else { format!("UE4.{}", self.minor) };
        let standard_extra = if self.ue5 { 4 } else { 0 };
        if self.section_extra != standard_extra {
            format!("{} (+{} section bytes)", base, self.section_extra)
        } else {
            base
        }
    }

    fn new_cooked_format(&self) -> bool {
        self.minor >= 24
    }

    fn new_weight_format(&self) -> bool {
        self.minor >= 25
    }
}

#[derive(Serialize, Clone)]
pub struct Bone {
    pub name: String,
    pub parent: i32,
    pub rotation: [f32; 4],
    pub translation: [f32; 3],
    pub scale: [f32; 3],
}

#[derive(Clone)]
pub struct MorphTarget {
    pub name: String,
    pub vertices: Vec<u32>,
    pub position_deltas: Vec<f32>,
    pub normal_deltas: Vec<f32>,
}

#[derive(Serialize, Clone)]
pub struct Socket {
    pub name: String,
    pub bone: String,
    pub rotation: [f32; 4],
    pub translation: [f32; 3],
    pub scale: [f32; 3],
}

#[derive(Serialize, Clone)]
pub struct MaterialSlot {
    pub slot_name: String,
    pub material: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Section {
    pub material_index: u32,
    pub first_index: u32,
    pub index_count: u32,
}

pub struct MeshData {
    pub name: String,
    pub version: String,
    pub profile: Option<Profile>,
    pub bones: Vec<Bone>,
    pub materials: Vec<MaterialSlot>,
    pub sections: Vec<Section>,
    pub skeleton: Option<String>,
    pub physics_asset: Option<String>,
    pub physics_bones: Vec<String>,
    pub sockets: Vec<Socket>,
    pub lod_index: usize,
    pub morphs: Vec<MorphTarget>,
    pub positions: Vec<f32>,
    pub normals: Vec<f32>,
    pub uvs: Vec<f32>,
    pub indices: Vec<u32>,
    pub skin_indices: Vec<u32>,
    pub skin_weights: Vec<f32>,
}

pub const UNIT_SCALE: f32 = 0.01;

pub fn convert_vector(x: f32, y: f32, z: f32) -> [f32; 3] {
    [y, z, x]
}

pub(crate) struct StripFlags {
    global: u8,
    class: u8,
}

impl StripFlags {
    pub(crate) fn read(r: &mut Reader) -> Result<Self> {
        Ok(StripFlags { global: r.u8()?, class: r.u8()? })
    }

    pub(crate) fn editor_stripped(&self) -> bool {
        self.global & 1 != 0
    }

    pub(crate) fn audio_visual_stripped(&self) -> bool {
        self.global & 2 != 0
    }

    pub(crate) fn class_stripped(&self, flag: u8) -> bool {
        self.class & flag != 0
    }
}

struct RenderSection {
    material_index: i16,
    base_index: u32,
    num_triangles: u32,
    base_vertex_index: u32,
    num_vertices: u32,
    bone_map: Vec<u16>,
}

struct Geometry {
    indices: Vec<u32>,
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    influences: Vec<Vec<(u16, u16)>>,
}

fn read_index_buffer(r: &mut Reader) -> Result<Vec<u32>> {
    let data_size = r.u8()?;
    let (elem_size, count, data) = r.bulk_array()?;
    match (data_size, elem_size) {
        (2, 2) => Ok(data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect()),
        (4, 4) => Ok(data
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        _ if count == 0 => Ok(Vec::new()),
        _ => Err(format!("unexpected index size {} / {}", data_size, elem_size)),
    }
}

pub(crate) fn read_positions(r: &mut Reader) -> Result<Vec<[f32; 3]>> {
    let stride = r.i32()?;
    let num_vertices = r.i32()?;
    let (elem_size, count, data) = r.bulk_array()?;
    if stride != 12 || elem_size != 12 || count != num_vertices as usize {
        return Err(format!("unexpected position buffer (stride {}, {} vs {})", stride, count, num_vertices));
    }
    Ok(data
        .chunks_exact(12)
        .map(|c| {
            let f = |i: usize| f32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
            [f(0), f(4), f(8)]
        })
        .collect())
}

fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as f32;
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 => sign * f32::INFINITY,
        _ => sign * (1.0 + mant / 1024.0) * 2f32.powi(exp - 15),
    }
}

pub(crate) fn read_static_mesh_vertex_buffer(r: &mut Reader, profile: Profile) -> Result<(Vec<[f32; 3]>, Vec<[f32; 2]>)> {
    let strip = StripFlags::read(r)?;
    let num_tex_coords = r.i32()?;
    let num_vertices = r.i32()?;
    let full_precision_uvs = r.bool32()?;
    let high_precision_tangents = r.bool32()?;
    if num_tex_coords <= 0 || num_tex_coords > 8 || num_vertices < 0 {
        return Err(format!("unexpected vertex buffer ({} uv sets, {} vertices)", num_tex_coords, num_vertices));
    }
    if strip.audio_visual_stripped() {
        return Ok((Vec::new(), Vec::new()));
    }
    let num_vertices = num_vertices as usize;
    let num_tex_coords = num_tex_coords as usize;
    let (tangent_size, tangent_count, tangents) = r.bulk_array()?;
    let expected_tangent_size = if high_precision_tangents { 16 } else { 8 };
    if tangent_count != num_vertices || tangent_size != expected_tangent_size {
        return Err(format!("unexpected tangent buffer ({} x {})", tangent_count, tangent_size));
    }
    let signed = profile.minor >= 20;
    let normals = tangents
        .chunks_exact(tangent_size)
        .map(|c| {
            let n = &c[tangent_size / 2..];
            let v = if high_precision_tangents {
                let s = |i: usize| {
                    let raw = u16::from_le_bytes([n[i], n[i + 1]]);
                    if signed { raw as i16 as f32 / 32767.0 } else { (raw as f32 - 32767.5) / 32767.5 }
                };
                [s(0), s(2), s(4)]
            } else {
                let s = |i: usize| if signed { n[i] as i8 as f32 / 127.0 } else { n[i] as f32 / 127.5 - 1.0 };
                [s(0), s(1), s(2)]
            };
            v
        })
        .collect();
    let (uv_size, uv_count, uv_data) = r.bulk_array()?;
    let uv_elem = if full_precision_uvs { 8 } else { 4 };
    if uv_size != uv_elem || uv_count < num_vertices * num_tex_coords {
        return Err(format!("unexpected uv buffer ({} x {})", uv_count, uv_size));
    }
    let stride = uv_elem * num_tex_coords;
    let uvs = (0..num_vertices)
        .map(|i| {
            let c = &uv_data[i * stride..i * stride + uv_elem];
            if full_precision_uvs {
                [
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                ]
            } else {
                [
                    half_to_f32(u16::from_le_bytes([c[0], c[1]])),
                    half_to_f32(u16::from_le_bytes([c[2], c[3]])),
                ]
            }
        })
        .collect();
    Ok((normals, uvs))
}

fn read_skin_weights(r: &mut Reader, profile: Profile) -> Result<Vec<Vec<(u16, u16)>>> {
    let strip = StripFlags::read(r)?;
    let num_vertices;
    let max_influences;
    let mut use_16bit_indices = false;
    let mut use_16bit_weights = false;
    let mut variable_influences = false;
    if !profile.new_cooked_format() {
        let extra = r.bool32()?;
        num_vertices = r.u32()? as usize;
        max_influences = if extra { 8 } else { 4 };
    } else if !profile.new_weight_format() {
        let extra = r.bool32()?;
        r.u32()?;
        num_vertices = r.u32()? as usize;
        max_influences = if extra { 8 } else { 4 };
    } else {
        variable_influences = r.bool32()?;
        max_influences = r.u32()? as usize;
        r.u32()?;
        num_vertices = r.u32()? as usize;
        use_16bit_indices = r.bool32()?;
        if profile.ue5 {
            use_16bit_weights = r.bool32()?;
        }
    }
    if max_influences == 0 || max_influences > 12 {
        return Err(format!("unexpected bone influence count {}", max_influences));
    }
    if strip.audio_visual_stripped() {
        return Ok(Vec::new());
    }
    let (_, _, data) = r.bulk_array()?;
    let mut lookup: Vec<u32> = Vec::new();
    if profile.new_weight_format() {
        let lookup_strip = StripFlags::read(r)?;
        r.i32()?;
        if !lookup_strip.audio_visual_stripped() {
            let (_, _, lookup_data) = r.bulk_array()?;
            lookup = lookup_data
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
        }
    }
    let index_size = if use_16bit_indices { 2 } else { 1 };
    let weight_size = if use_16bit_weights { 2 } else { 1 };
    let read_vertex = |offset: usize, n: usize| -> Result<Vec<(u16, u16)>> {
        let end = offset + n * (index_size + weight_size);
        if end > data.len() {
            return Err("skin weight data is truncated".into());
        }
        let mut out = Vec::with_capacity(n);
        for k in 0..n {
            let bone = if use_16bit_indices {
                u16::from_le_bytes([data[offset + k * 2], data[offset + k * 2 + 1]])
            } else {
                data[offset + k] as u16
            };
            let at = offset + n * index_size + k * weight_size;
            let weight = if use_16bit_weights { u16::from_le_bytes([data[at], data[at + 1]]) } else { data[at] as u16 };
            if weight > 0 {
                out.push((bone, weight));
            }
        }
        Ok(out)
    };
    let mut influences = Vec::with_capacity(num_vertices);
    if variable_influences {
        if lookup.len() != num_vertices {
            return Err("skin weight lookup does not match vertex count".into());
        }
        for entry in &lookup {
            influences.push(read_vertex((entry >> 8) as usize, (entry & 0xff) as usize)?);
        }
    } else {
        let stride = if num_vertices > 0 { data.len() / num_vertices } else { 0 };
        if stride != max_influences * (index_size + weight_size) {
            return Err(format!("unexpected skin weight stride {}", stride));
        }
        for i in 0..num_vertices {
            influences.push(read_vertex(i * stride, max_influences)?);
        }
    }
    Ok(influences)
}

fn read_render_section(r: &mut Reader, profile: Profile) -> Result<RenderSection> {
    let strip = StripFlags::read(r)?;
    let material_index = r.i16()?;
    let base_index = r.u32()?;
    let num_triangles = r.u32()?;
    r.bool32()?;
    if profile.minor >= 26 {
        r.u8()?;
    }
    r.bool32()?;
    r.skip(profile.section_extra)?;
    let base_vertex_index = r.u32()?;
    let cloth_mapping = r.count(64)?;
    r.skip(cloth_mapping * 64)?;
    let bone_map_len = r.count(2)?;
    let bone_map = (0..bone_map_len).map(|_| r.u16()).collect::<Result<Vec<_>>>()?;
    let num_vertices = r.i32()?;
    let max_bone_influences = r.i32()?;
    if num_vertices < 0 || !(0..=12).contains(&max_bone_influences) {
        return Err(format!("implausible section ({} vertices, {} influences)", num_vertices, max_bone_influences));
    }
    r.i16()?;
    r.skip(20)?;
    if profile.minor < 23 || !strip.class_stripped(1) {
        let dup_data = r.count(4)?;
        r.skip(dup_data * 4)?;
        let dup_index = r.count(8)?;
        r.skip(dup_index * 8)?;
    }
    if profile.minor >= 20 {
        r.bool32()?;
    }
    Ok(RenderSection {
        material_index,
        base_index,
        num_triangles,
        base_vertex_index,
        num_vertices: num_vertices as u32,
        bone_map,
    })
}

fn read_sections(r: &mut Reader, profile: Profile) -> Result<Vec<RenderSection>> {
    let n = r.count(30)?;
    (0..n).map(|_| read_render_section(r, profile)).collect()
}

type VertexStreams = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<Vec<(u16, u16)>>);

fn read_vertex_streams(r: &mut Reader, profile: Profile) -> Result<VertexStreams> {
    let positions = read_positions(r)?;
    let (normals, uvs) = read_static_mesh_vertex_buffer(r, profile)?;
    let influences = read_skin_weights(r, profile)?;
    Ok((positions, normals, uvs, influences))
}

fn read_streamed_data(r: &mut Reader, profile: Profile) -> Result<Geometry> {
    StripFlags::read(r)?;
    let indices = read_index_buffer(r)?;
    let (positions, normals, uvs, influences) = read_vertex_streams(r, profile)?;
    Ok(Geometry { indices, positions, normals, uvs, influences })
}

fn read_lod<'a>(pkg: &'a Package, r: &mut Reader<'a>, profile: Profile) -> Result<Option<(Vec<RenderSection>, Geometry)>> {
    let strip = StripFlags::read(r)?;
    let cooked_out = r.bool32()?;
    let inlined = r.bool32()?;
    let required_bones = r.count(2)?;
    r.skip(required_bones * 2)?;
    if strip.audio_visual_stripped() || cooked_out {
        return Ok(None);
    }
    let sections = read_sections(r, profile)?;
    let active_bones = r.count(2)?;
    r.skip(active_bones * 2)?;
    r.u32()?;
    let start = r.pos;
    let wrapped = if inlined {
        read_bulk_data(pkg, r).ok().flatten().and_then(|payload| read_streamed_data(&mut Reader::new(payload), profile).ok())
    } else {
        None
    };
    let geometry = if let Some(geometry) = wrapped {
        geometry
    } else if inlined {
        r.pos = start;
        read_streamed_data(r, profile)?
    } else {
        match read_bulk_data(pkg, r)? {
            Some(payload) => read_streamed_data(&mut Reader::new(payload), profile)?,
            None => return Ok(None),
        }
    };
    Ok(Some((sections, geometry)))
}

fn read_lod_legacy(r: &mut Reader, profile: Profile) -> Result<Option<(Vec<RenderSection>, Geometry)>> {
    let strip = StripFlags::read(r)?;
    let sections = read_sections(r, profile)?;
    let indices = read_index_buffer(r)?;
    let active_bones = r.count(2)?;
    r.skip(active_bones * 2)?;
    let required_bones = r.count(2)?;
    r.skip(required_bones * 2)?;
    if strip.audio_visual_stripped() || strip.class_stripped(2) {
        return Ok(None);
    }
    let (positions, normals, uvs, influences) = read_vertex_streams(r, profile)?;
    Ok(Some((sections, Geometry { indices, positions, normals, uvs, influences })))
}

fn validate(sections: &[RenderSection], geo: &Geometry, num_bones: usize, num_materials: usize) -> Result<()> {
    let n = geo.positions.len();
    if n == 0 || geo.indices.is_empty() {
        return Err("LOD has no geometry".into());
    }
    if geo.normals.len() != n || geo.uvs.len() != n || geo.influences.len() != n {
        return Err("vertex stream lengths do not match".into());
    }
    if geo.indices.iter().any(|&i| i as usize >= n) {
        return Err("index out of range".into());
    }
    for s in sections {
        if s.material_index < 0 || (num_materials > 0 && s.material_index as usize >= num_materials) {
            return Err(format!("section material index {} out of range", s.material_index));
        }
        if (s.base_index + s.num_triangles * 3) as usize > geo.indices.len()
            || (s.base_vertex_index + s.num_vertices) as usize > n
        {
            return Err("section range out of bounds".into());
        }
        if s.bone_map.iter().any(|&b| b as usize >= num_bones) {
            return Err("section bone map out of range".into());
        }
    }
    Ok(())
}

pub fn read_reference_skeleton(pkg: &Package, r: &mut Reader) -> Result<Vec<Bone>> {
    let num_bones = r.count(12)?;
    let mut bone_info = Vec::with_capacity(num_bones);
    for _ in 0..num_bones {
        let name = pkg.read_name(r)?;
        let parent = r.i32()?;
        if parent < -1 || parent >= num_bones as i32 {
            return Err(format!("bone {} has invalid parent {}", name, parent));
        }
        bone_info.push((name, parent));
    }
    let doubles = pkg.large_world_coordinates();
    let num_poses = r.count(if doubles { 80 } else { 40 })?;
    if num_poses != num_bones {
        return Err("bone pose count does not match bone count".into());
    }
    let mut bones = Vec::with_capacity(num_bones);
    for (name, parent) in bone_info {
        let mut f = [0f32; 10];
        for v in f.iter_mut() {
            *v = if doubles { f64::from_le_bytes(r.bytes(8)?.try_into().unwrap()) as f32 } else { r.f32()? };
            if !v.is_finite() {
                return Err(format!("bone {} has a non-finite transform", name));
            }
        }
        let t = convert_vector(f[4], f[5], f[6]);
        let rot = convert_vector(f[0], f[1], f[2]);
        bones.push(Bone {
            name,
            parent,
            rotation: [rot[0], rot[1], rot[2], f[3]],
            translation: [t[0] * UNIT_SCALE, t[1] * UNIT_SCALE, t[2] * UNIT_SCALE],
            scale: convert_vector(f[7], f[8], f[9]),
        });
    }
    let name_map = r.count(12)?;
    r.skip(name_map * 12)?;
    Ok(bones)
}

fn read_header(pkg: &Package, r: &mut Reader, profile: Profile) -> Result<(Vec<MaterialSlot>, Vec<Bone>)> {
    let strip = StripFlags::read(r)?;
    if !strip.editor_stripped() {
        return Err("uncooked (editor) skeletal meshes are not supported".into());
    }
    let bounds: Vec<f32> = if pkg.large_world_coordinates() {
        (0..7).map(|_| Ok(f64::from_le_bytes(r.bytes(8)?.try_into().unwrap()) as f32)).collect::<Result<_>>()?
    } else {
        (0..7).map(|_| r.f32()).collect::<Result<_>>()?
    };
    if bounds.iter().any(|v| !v.is_finite()) || bounds[6] < 0.0 {
        return Err("implausible skeletal mesh bounds".into());
    }
    let num_materials = r.count(8)?;
    if num_materials > 256 {
        return Err(format!("implausible material count {}", num_materials));
    }
    let mut materials = Vec::with_capacity(num_materials);
    for _ in 0..num_materials {
        let material_index = r.i32()?;
        let slot_name = pkg.read_name(r)?;
        if profile.minor >= 25 && r.bool32()? {
            pkg.read_name(r)?;
        }
        r.skip(24 + profile.material_extra)?;
        let material = pkg.import_path(material_index).or_else(|| pkg.object_name(material_index).map(String::from));
        materials.push(MaterialSlot { slot_name, material });
    }
    let bones = read_reference_skeleton(pkg, r)?;
    if bones.first().is_none_or(|b| b.parent != -1) {
        return Err("skeletal mesh has no root bone".into());
    }
    Ok((materials, bones))
}

fn import_of_class(pkg: &Package, class: &str) -> Option<String> {
    let index = pkg.imports.iter().position(|i| i.class_name == class)?;
    pkg.import_path(-(index as i32) - 1)
}

pub fn read_skeletal_mesh(pkg: &Package, export: &ObjectExport, profile: Profile) -> Result<MeshData> {
    let data = pkg.export_data(export)?;
    let mut r = Reader::new(data);
    let (props, starts) = match read_object_header(pkg, export, &mut r) {
        Ok(props) => (Some(props), r.pos..r.pos + 1),
        Err(_) if pkg.unversioned_properties => (None, 0..data.len()),
        Err(e) => return Err(e),
    };
    let object = |name: &str, class: &str| match &props {
        Some(props) => pkg.import_path(prop_object(props, name)),
        None => import_of_class(pkg, class),
    };
    let skeleton = object("Skeleton", "Skeleton");
    let physics_asset = object("PhysicsAsset", "PhysicsAsset").map(|p| p.split('.').next().unwrap_or(&p).to_string());
    let scanning = props.is_none();
    let mut header = Err("could not locate the skeletal mesh data".to_string());
    for start in starts {
        r.pos = start;
        header = read_header(pkg, &mut r, profile);
        if header.is_ok() {
            break;
        }
    }
    let (materials, bones) = header.map_err(|e| if scanning { "could not locate the skeletal mesh data".to_string() } else { e })?;
    let _cooked = r.bool32()?;
    let num_lods = r.count(8)?;
    let mut found = None;
    let mut lod_index = 0;
    for index in 0..num_lods {
        let lod = if profile.new_cooked_format() {
            read_lod(pkg, &mut r, profile)?
        } else {
            read_lod_legacy(&mut r, profile)?
        };
        if let Some(lod) = lod {
            found = Some(lod);
            lod_index = index;
            break;
        }
    }
    let (sections, geo) = found.ok_or("no LOD with geometry found")?;
    validate(&sections, &geo, bones.len(), materials.len())?;
    let n = geo.positions.len();
    let mut positions = Vec::with_capacity(n * 3);
    for p in &geo.positions {
        let v = convert_vector(p[0], p[1], p[2]);
        positions.extend_from_slice(&[v[0] * UNIT_SCALE, v[1] * UNIT_SCALE, v[2] * UNIT_SCALE]);
    }
    let mut normals = Vec::with_capacity(n * 3);
    for p in &geo.normals {
        let v = convert_vector(p[0], p[1], p[2]);
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
        normals.extend_from_slice(&[v[0] / len, v[1] / len, v[2] / len]);
    }
    let uvs = geo.uvs.iter().flat_map(|uv| [uv[0], uv[1]]).collect();
    let mut skin_indices = vec![0u32; n * 4];
    let mut skin_weights = vec![0f32; n * 4];
    for s in &sections {
        let start = s.base_vertex_index as usize;
        for v in start..start + s.num_vertices as usize {
            let mut infl: Vec<(u16, u16)> = geo.influences[v].clone();
            infl.sort_by(|a, b| b.1.cmp(&a.1));
            infl.truncate(4);
            let total: f32 = infl.iter().map(|&(_, w)| w as f32).sum();
            for (k, &(local, w)) in infl.iter().enumerate() {
                let bone = s.bone_map.get(local as usize).copied().unwrap_or(0);
                skin_indices[v * 4 + k] = bone as u32;
                skin_weights[v * 4 + k] = if total > 0.0 { w as f32 / total } else { 0.0 };
            }
        }
    }
    let out_sections = sections
        .iter()
        .map(|s| Section {
            material_index: s.material_index as u32,
            first_index: s.base_index,
            index_count: s.num_triangles * 3,
        })
        .collect();
    Ok(MeshData {
        name: export.object_name.clone(),
        version: profile.label(),
        profile: Some(profile),
        bones,
        materials,
        sections: out_sections,
        skeleton,
        physics_asset,
        physics_bones: Vec::new(),
        sockets: Vec::new(),
        lod_index,
        morphs: Vec::new(),
        positions,
        normals,
        uvs,
        indices: geo.indices,
        skin_indices,
        skin_weights,
    })
}

fn rotator_to_quat(pitch: f32, yaw: f32, roll: f32) -> [f32; 4] {
    let half = std::f32::consts::PI / 360.0;
    let (sp, cp) = (pitch * half).sin_cos();
    let (sy, cy) = (yaw * half).sin_cos();
    let (sr, cr) = (roll * half).sin_cos();
    [
        cr * sp * sy - sr * cp * cy,
        -cr * sp * cy - sr * cp * sy,
        cr * cp * sy - sr * sp * cy,
        cr * cp * cy + sr * sp * sy,
    ]
}

pub fn read_sockets(pkg: &Package) -> Vec<Socket> {
    let mut sockets = Vec::new();
    for export in &pkg.exports {
        if pkg.export_class_name(export) != Some("SkeletalMeshSocket") {
            continue;
        }
        let Ok(data) = pkg.export_data(export) else { continue };
        let Ok(props) = read_object_header(pkg, export, &mut Reader::new(data)) else { continue };
        let (Some(name), Some(bone)) = (prop_name(&props, "SocketName"), prop_name(&props, "BoneName")) else {
            continue;
        };
        let loc = prop_vector(&props, "RelativeLocation").unwrap_or([0.0; 3]);
        let rot = prop_vector(&props, "RelativeRotation").unwrap_or([0.0; 3]);
        let scale = prop_vector(&props, "RelativeScale").unwrap_or([1.0; 3]);
        let q = rotator_to_quat(rot[0], rot[1], rot[2]);
        let qv = convert_vector(q[0], q[1], q[2]);
        let t = convert_vector(loc[0], loc[1], loc[2]);
        sockets.push(Socket {
            name,
            bone,
            rotation: [qv[0], qv[1], qv[2], q[3]],
            translation: [t[0] * UNIT_SCALE, t[1] * UNIT_SCALE, t[2] * UNIT_SCALE],
            scale: convert_vector(scale[0], scale[1], scale[2]),
        });
    }
    sockets
}

fn read_morph_lods(r: &mut Reader, with_generated_flag: bool) -> Result<Vec<(Vec<u32>, Vec<f32>, Vec<f32>)>> {
    let num_lods = r.count(12)?;
    let mut lods = Vec::with_capacity(num_lods);
    for _ in 0..num_lods {
        let n = r.count(28)?;
        let mut vertices = Vec::with_capacity(n);
        let mut positions = Vec::with_capacity(n * 3);
        let mut normals = Vec::with_capacity(n * 3);
        for _ in 0..n {
            let p = [r.f32()?, r.f32()?, r.f32()?];
            let t = [r.f32()?, r.f32()?, r.f32()?];
            vertices.push(r.u32()?);
            let pv = convert_vector(p[0], p[1], p[2]);
            positions.extend_from_slice(&[pv[0] * UNIT_SCALE, pv[1] * UNIT_SCALE, pv[2] * UNIT_SCALE]);
            normals.extend_from_slice(&convert_vector(t[0], t[1], t[2]));
        }
        r.i32()?;
        let sections = r.count(4)?;
        r.skip(sections * 4)?;
        if with_generated_flag {
            r.bool32()?;
        }
        lods.push((vertices, positions, normals));
    }
    Ok(lods)
}

pub fn read_morph_targets(pkg: &Package, lod_index: usize, num_vertices: usize) -> Vec<MorphTarget> {
    let mut morphs = Vec::new();
    for export in &pkg.exports {
        if pkg.export_class_name(export) != Some("MorphTarget") {
            continue;
        }
        let Ok(data) = pkg.export_data(export) else { continue };
        let mut r = Reader::new(data);
        if read_object_header(pkg, export, &mut r).is_err() || r.skip(2).is_err() {
            continue;
        }
        let start = r.pos;
        let lods = [true, false].into_iter().find_map(|flag| {
            r.pos = start;
            read_morph_lods(&mut r, flag).ok().filter(|_| r.remaining() <= 4)
        });
        let Some(mut lods) = lods else { continue };
        if lod_index >= lods.len() {
            continue;
        }
        let (vertices, position_deltas, normal_deltas) = lods.swap_remove(lod_index);
        if vertices.iter().any(|&v| v as usize >= num_vertices) {
            continue;
        }
        morphs.push(MorphTarget { name: export.object_name.clone(), vertices, position_deltas, normal_deltas });
    }
    morphs
}

pub fn read_physics_bodies(pkg: &Package) -> Vec<String> {
    pkg.exports
        .iter()
        .filter(|e| pkg.export_class_name(e) == Some("SkeletalBodySetup"))
        .filter_map(|e| {
            let props = read_object_header(pkg, e, &mut Reader::new(pkg.export_data(e).ok()?)).ok()?;
            prop_name(&props, "BoneName")
        })
        .collect()
}
