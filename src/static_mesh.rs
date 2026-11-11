use crate::bulk::read_bulk_data;
use crate::package::{ObjectExport, Package};
use crate::properties::{read_object_header, PropValue, Properties};
use crate::reader::{Reader, Result};
use crate::skeletal_mesh::{
    convert_vector, read_positions, read_static_mesh_vertex_buffer, MaterialSlot, MeshData, Profile, Section, StripFlags,
    UNIT_SCALE,
};

const MAX_LODS: usize = 8;
const CDSF_MIN_LOD_DATA: u8 = 2;
const LOD_RESYNC_WINDOW: usize = 1024;

#[derive(Clone, Copy)]
struct Layout {
    streamed_lods: bool,
    buffer_strip_flags: bool,
    section_extra: usize,
    lod_bounds: usize,
    buffer_gap: usize,
    minor: u32,
}

impl Layout {
    fn candidates(pkg: &Package) -> Vec<Layout> {
        if pkg.is_ue5 {
            let mut out = Vec::new();
            for lod_bounds in [0, 56] {
                for section_extra in [12, 16, 8] {
                    for buffer_gap in [4, 0] {
                        out.push(Layout {
                            streamed_lods: true,
                            buffer_strip_flags: true,
                            section_extra,
                            lod_bounds,
                            buffer_gap,
                            minor: 27,
                        });
                    }
                }
            }
            return out;
        }
        let engine_minor = pkg.engine_version.filter(|v| v.0 == 4).map(|v| v.1 as u32);
        let mut out = Vec::new();
        for (streamed_lods, buffer_strip_flags) in [(true, true), (true, false), (false, false)] {
            for section_extra in [0, 4, 8] {
                let minor = engine_minor.unwrap_or(if streamed_lods { 23 } else { 20 });
                out.push(Layout { streamed_lods, buffer_strip_flags, section_extra, lod_bounds: 0, buffer_gap: 0, minor });
            }
        }
        if let Some(minor) = engine_minor {
            out.sort_by_key(|l| l.streamed_lods != (minor >= 23));
        }
        out
    }

    fn label(&self, pkg: &Package) -> String {
        match pkg.engine_version {
            Some((major, minor, _)) => format!("UE{}.{}", major, minor),
            None if pkg.is_ue5 => "UE5".into(),
            None if self.streamed_lods => "UE4.23+".into(),
            None => "UE4.19-4.22".into(),
        }
    }
}

struct StaticSection {
    material_index: i32,
    first_index: u32,
    num_triangles: u32,
    min_vertex: u32,
    max_vertex: u32,
}

type RenderData = (Vec<StaticSection>, Geometry, usize);

struct Geometry {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

fn read_section(r: &mut Reader, layout: Layout) -> Result<StaticSection> {
    let section = StaticSection {
        material_index: r.i32()?,
        first_index: r.u32()?,
        num_triangles: r.u32()?,
        min_vertex: r.u32()?,
        max_vertex: r.u32()?,
    };
    r.bool32()?;
    r.bool32()?;
    for _ in 0..layout.section_extra / 4 {
        r.bool32()?;
    }
    if !(0..256).contains(&section.material_index) || section.min_vertex > section.max_vertex {
        return Err("implausible static mesh section".into());
    }
    Ok(section)
}

fn read_index_buffer(r: &mut Reader) -> Result<Vec<u32>> {
    let is_32bit = r.bool32()?;
    let (elem_size, _, data) = r.bulk_array()?;
    if elem_size != 1 {
        return Err(format!("unexpected index buffer element size {}", elem_size));
    }
    Ok(if is_32bit {
        data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    } else {
        data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect()
    })
}

fn read_buffers(r: &mut Reader, layout: Layout) -> Result<Geometry> {
    if layout.buffer_strip_flags {
        StripFlags::read(r)?;
    }
    let positions = read_positions(r)?;
    let profile = Profile { minor: layout.minor, section_extra: 0, ue5: false, material_extra: 0 };
    let (normals, uvs) = read_static_mesh_vertex_buffer(r, profile)?;
    let color_strip = StripFlags::read(r)?;
    r.i32()?;
    let num_colors = r.i32()?;
    if num_colors > 0 && !color_strip.audio_visual_stripped() {
        r.bulk_array()?;
    }
    let indices = read_index_buffer(r)?;
    Ok(Geometry { positions, normals, uvs, indices })
}

enum LodRead {
    Geometry(Vec<StaticSection>, Geometry),
    Empty,
    Unavailable(String),
}

fn read_lod<'a>(pkg: &'a Package, r: &mut Reader<'a>, layout: Layout) -> Result<LodRead> {
    let strip = StripFlags::read(r)?;
    let num_sections = r.count(28)?;
    if num_sections == 0 {
        return Err("LOD has no sections".into());
    }
    let sections = (0..num_sections).map(|_| read_section(r, layout)).collect::<Result<Vec<_>>>()?;
    r.skip(layout.lod_bounds)?;
    r.f32()?;
    if !layout.streamed_lods {
        if strip.audio_visual_stripped() || strip.class_stripped(CDSF_MIN_LOD_DATA) {
            return Err("LOD buffers are stripped".into());
        }
        return Ok(LodRead::Geometry(sections, read_buffers(r, layout)?));
    }
    let cooked_out = r.bool32()?;
    let inlined = r.bool32()?;
    r.skip(layout.buffer_gap)?;
    if strip.audio_visual_stripped() || cooked_out {
        return Ok(LodRead::Empty);
    }
    if inlined {
        return Ok(LodRead::Geometry(sections, read_buffers(r, layout)?));
    }
    match read_bulk_data(pkg, r) {
        Ok(Some(payload)) => Ok(LodRead::Geometry(sections, read_buffers(&mut Reader::new(payload), layout)?)),
        Ok(None) => Err("streamed LOD has no data".into()),
        Err(e) => Ok(LodRead::Unavailable(e)),
    }
}

fn validate(sections: &[StaticSection], geo: &Geometry) -> Result<()> {
    let n = geo.positions.len();
    if n == 0 || geo.indices.is_empty() {
        return Err("LOD has no geometry".into());
    }
    if geo.normals.len() != n || geo.uvs.len() != n {
        return Err("vertex stream lengths do not match".into());
    }
    if !geo.indices.len().is_multiple_of(3) || geo.indices.iter().any(|&i| i as usize >= n) {
        return Err("index buffer out of range".into());
    }
    for s in sections {
        if s.first_index as u64 + s.num_triangles as u64 * 3 > geo.indices.len() as u64 || s.max_vertex as usize >= n {
            return Err("section range out of bounds".into());
        }
    }
    Ok(())
}

fn read_render_data<'a>(
    pkg: &'a Package,
    r: &mut Reader<'a>,
    layout: Layout,
) -> Result<RenderData> {
    let num_lods = r.count(8)?;
    if num_lods == 0 || num_lods > MAX_LODS {
        return Err(format!("implausible LOD count {}", num_lods));
    }
    for index in 0..num_lods {
        match read_lod(pkg, r, layout)? {
            LodRead::Geometry(sections, geo) => {
                validate(&sections, &geo)?;
                return Ok((sections, geo, index));
            }
            LodRead::Empty => {}
            LodRead::Unavailable(e) => {
                let after = r.pos;
                let next = (0..LOD_RESYNC_WINDOW).filter(|_| index + 1 < num_lods).find_map(|skip| {
                    r.pos = after + skip;
                    match read_lod(pkg, r, layout) {
                        Ok(LodRead::Geometry(sections, geo)) if validate(&sections, &geo).is_ok() => Some((sections, geo, index + 1)),
                        _ => None,
                    }
                });
                return next.ok_or(e);
            }
        }
    }
    Err("no LOD with geometry found".into())
}

fn struct_object(props: &Properties, name: &str) -> i32 {
    match props.get(name) {
        Some(PropValue::Object(i)) => *i,
        _ => 0,
    }
}

fn material_path(pkg: &Package, index: i32) -> Option<String> {
    pkg.import_path(index).or_else(|| pkg.object_name(index).map(String::from))
}

fn materials_from_properties(pkg: &Package, props: &Properties) -> Vec<MaterialSlot> {
    match props.get("StaticMaterials") {
        Some(PropValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                PropValue::Struct(fields) => MaterialSlot {
                    slot_name: match fields.get("MaterialSlotName") {
                        Some(PropValue::Name(n)) => n.clone(),
                        _ => String::new(),
                    },
                    material: material_path(pkg, struct_object(fields, "MaterialInterface")),
                },
                _ => MaterialSlot { slot_name: String::new(), material: None },
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn read_native_materials(pkg: &Package, data: &[u8], slot_size: usize, trailer: usize) -> Option<Vec<MaterialSlot>> {
    (1..=64).find_map(|count| {
        let start = data.len().checked_sub(count * slot_size + 4 + trailer)?;
        let mut r = Reader::new(&data[start..]);
        if r.i32().ok()? != count as i32 {
            return None;
        }
        (0..count)
            .map(|_| {
                let index = r.i32().ok()?;
                let in_range = index_in_range(pkg, index);
                let slot_name = pkg.read_name(&mut r).ok()?;
                if slot_size == 44 {
                    pkg.read_name(&mut r).ok()?;
                }
                r.bool32().ok()?;
                r.bool32().ok()?;
                r.skip(16).ok()?;
                in_range.then(|| MaterialSlot { slot_name, material: material_path(pkg, index) })
            })
            .collect()
    })
}

fn index_in_range(pkg: &Package, index: i32) -> bool {
    if index < 0 {
        ((-index - 1) as usize) < pkg.imports.len()
    } else {
        index as usize <= pkg.exports.len()
    }
}

fn read_native_header(pkg: &Package, r: &mut Reader) -> Result<()> {
    let strip = StripFlags::read(r)?;
    if !strip.editor_stripped() {
        return Err("uncooked (editor) static meshes are not supported".into());
    }
    if !r.bool32()? {
        return Err("static mesh has no cooked render data".into());
    }
    let body_setup = r.i32()?;
    let nav_collision = r.i32()?;
    if !index_in_range(pkg, body_setup) || !index_in_range(pkg, nav_collision) {
        return Err("implausible static mesh header".into());
    }
    r.skip(16)?;
    let num_sockets = r.count(4)?;
    for _ in 0..num_sockets {
        if !index_in_range(pkg, r.i32()?) {
            return Err("implausible static mesh socket".into());
        }
    }
    Ok(())
}

fn find_render_data<'a>(
    pkg: &'a Package,
    r: &mut Reader<'a>,
    first_error: &mut Option<String>,
) -> Option<(Layout, RenderData)> {
    if let Err(e) = read_native_header(pkg, r) {
        first_error.get_or_insert(e);
        return None;
    }
    let start = r.pos;
    for offset in (0..=64).step_by(4) {
        for layout in Layout::candidates(pkg) {
            r.pos = start + offset;
            match read_render_data(pkg, r, layout) {
                Ok(result) => return Some((layout, result)),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
    }
    None
}

pub fn read_static_mesh(pkg: &Package, export: &ObjectExport) -> Result<MeshData> {
    let data = pkg.export_data(export)?;
    let mut r = Reader::new(data);
    let (props, starts) = match read_object_header(pkg, export, &mut r) {
        Ok(props) => (Some(props), r.pos..r.pos + 1),
        Err(_) if pkg.unversioned_properties => (None, 0..data.len()),
        Err(e) => return Err(e),
    };
    let scanning = props.is_none();
    let mut first_error = None;
    let found = starts.into_iter().find_map(|start| {
        r.pos = start;
        find_render_data(pkg, &mut r, &mut first_error)
    });
    let (layout, (sections, geo, lod_index)) = found.ok_or_else(|| {
        if scanning {
            "could not locate the static mesh render data".to_string()
        } else {
            format!("could not read the static mesh render data ({})", first_error.unwrap_or_default())
        }
    })?;
    let mut materials = props.as_ref().map(|p| materials_from_properties(pkg, p)).unwrap_or_default();
    if materials.is_empty() {
        materials = [(36, 0), (44, 0), (36, 4)]
            .into_iter()
            .find_map(|(slot_size, trailer)| read_native_materials(pkg, data, slot_size, trailer))
            .unwrap_or_default();
    }
    let needed = sections.iter().map(|s| s.material_index as usize + 1).max().unwrap_or(0);
    while materials.len() < needed {
        materials.push(MaterialSlot { slot_name: format!("Material {}", materials.len() + 1), material: None });
    }
    let mut positions = Vec::with_capacity(geo.positions.len() * 3);
    for p in &geo.positions {
        let v = convert_vector(p[0], p[1], p[2]);
        positions.extend_from_slice(&[v[0] * UNIT_SCALE, v[1] * UNIT_SCALE, v[2] * UNIT_SCALE]);
    }
    let mut normals = Vec::with_capacity(geo.normals.len() * 3);
    for p in &geo.normals {
        let v = convert_vector(p[0], p[1], p[2]);
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
        normals.extend_from_slice(&[v[0] / len, v[1] / len, v[2] / len]);
    }
    Ok(MeshData {
        name: export.object_name.clone(),
        version: layout.label(pkg),
        profile: None,
        bones: Vec::new(),
        materials,
        sections: sections
            .iter()
            .map(|s| Section {
                material_index: s.material_index as u32,
                first_index: s.first_index,
                index_count: s.num_triangles * 3,
            })
            .collect(),
        skeleton: None,
        physics_asset: None,
        physics_bones: Vec::new(),
        sockets: Vec::new(),
        lod_index,
        morphs: Vec::new(),
        positions,
        normals,
        uvs: geo.uvs.iter().flat_map(|uv| [uv[0], uv[1]]).collect(),
        indices: geo.indices,
        skin_indices: Vec::new(),
        skin_weights: Vec::new(),
    })
}
