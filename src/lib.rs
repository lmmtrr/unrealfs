#![allow(dead_code)]

pub mod acl;
pub mod animation;
pub mod bulk;
pub mod material;
pub mod package;
pub mod properties;
pub mod reader;
pub mod skeletal_mesh;
pub mod static_mesh;
pub mod texture;
pub mod unversioned;
pub mod usmap;

use animation::{read_anim_contents, read_anim_info, read_animation, read_skeleton_bones, AnimInfo, SampledAnimation};
use material::read_material;
use texture::{read_texture, DecodedTexture};
use package::Package;
use serde::Serialize;
use skeletal_mesh::{read_morph_targets, read_physics_bodies, read_skeletal_mesh, read_sockets, MeshData, Profile, Socket};
use static_mesh::read_static_mesh;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use usmap::Mappings;

#[derive(Default)]
struct PackageFiles {
    uasset: Option<Vec<u8>>,
    uexp: Option<Vec<u8>>,
    ubulk: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub struct MeshEntry {
    pub path: String,
    pub name: String,
    pub is_static: bool,
}

#[derive(Serialize)]
pub struct AnimEntry {
    pub path: String,
    pub has_bone_tracks: bool,
    pub curves: Vec<String>,
    #[serde(flatten)]
    pub info: AnimInfo,
}

fn split_package_path(path: &str) -> Option<(String, &'static str)> {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    for ext in ["uasset", "uexp", "ubulk"] {
        if lower.ends_with(&format!(".{}", ext)) {
            let stem = &normalized[..normalized.len() - ext.len() - 1];
            return Some((stem.to_string(), ext));
        }
    }
    None
}

fn find_package<'a>(packages: &'a BTreeMap<String, Package>, game_path: &str) -> Option<&'a Package> {
    let path = game_path.split('.').next().unwrap_or(game_path).to_ascii_lowercase();
    let wanted: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).rev().collect();
    let stem = *wanted.first()?;
    packages
        .iter()
        .filter_map(|(key, pkg)| {
            let key = key.to_ascii_lowercase();
            let have: Vec<&str> = key.split('/').rev().collect();
            if have.first() != Some(&stem) {
                return None;
            }
            let shared = wanted.iter().zip(&have).take_while(|(a, b)| a == b).count();
            Some((shared, pkg))
        })
        .max_by_key(|(shared, _)| *shared)
        .map(|(_, pkg)| pkg)
}

fn is_mappings_file(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".usmap")
}

pub fn is_unreal_file(path: &str) -> bool {
    split_package_path(path).is_some() || is_mappings_file(path)
}

#[derive(Default)]
pub struct UeEnvironment {
    pending: BTreeMap<String, PackageFiles>,
    packages: BTreeMap<String, Package>,
    errors: Vec<String>,
    meshes: HashMap<String, MeshData>,
    last_profile: Option<Profile>,
    mappings: Option<Arc<Mappings>>,
}

impl UeEnvironment {
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> bool {
        if is_mappings_file(path) {
            match Mappings::parse(&data) {
                Ok(mappings) => self.set_mappings(mappings),
                Err(e) => self.errors.push(format!("{}: {}", path, e)),
            }
            return true;
        }
        let Some((key, ext)) = split_package_path(path) else {
            return false;
        };
        let entry = self.pending.entry(key).or_default();
        match ext {
            "uasset" => entry.uasset = Some(data),
            "uexp" => entry.uexp = Some(data),
            _ => entry.ubulk = Some(data),
        }
        true
    }

    fn parse_pending(&mut self) {
        let keys: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, f)| f.uasset.is_some() && f.uexp.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let files = self.pending.remove(&key).unwrap();
            let uasset = files.uasset.unwrap();
            match Package::parse(&key, &uasset, files.uexp.unwrap(), files.ubulk) {
                Ok(mut pkg) => {
                    pkg.mappings = self.mappings.clone();
                    self.packages.insert(key, pkg);
                }
                Err(e) => self.errors.push(format!("{}: {}", key, e)),
            }
        }
    }

    pub fn set_mappings(&mut self, mappings: Mappings) {
        let mappings = Arc::new(mappings);
        for pkg in self.packages.values_mut() {
            pkg.mappings = Some(mappings.clone());
        }
        self.mappings = Some(mappings);
        self.meshes.clear();
    }

    pub fn has_mappings(&self) -> bool {
        self.mappings.is_some()
    }

    pub fn needs_mappings(&mut self) -> bool {
        self.parse_pending();
        self.mappings.is_none() && self.packages.values().any(|p| p.unversioned_properties)
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn mesh(&self, path: &str) -> Option<&MeshData> {
        self.meshes.get(path)
    }

    pub fn sockets(&mut self) -> Vec<Socket> {
        self.parse_pending();
        self.packages.values().flat_map(read_sockets).collect()
    }

    pub fn meshes(&mut self) -> Vec<MeshEntry> {
        self.parse_pending();
        let mut out = Vec::new();
        for (path, pkg) in &self.packages {
            for export in &pkg.exports {
                let class = pkg.export_class_name(export);
                if matches!(class, Some("SkeletalMesh" | "StaticMesh")) {
                    out.push(MeshEntry {
                        path: path.clone(),
                        name: export.object_name.clone(),
                        is_static: class == Some("StaticMesh"),
                    });
                }
            }
        }
        out
    }

    pub fn material_info(&mut self, game_path: &str) -> Option<material::MaterialInfo> {
        self.parse_pending();
        let packages = &self.packages;
        find_package(packages, game_path).and_then(|pkg| read_material(pkg, |p| find_package(packages, p)))
    }

    pub fn texture(&mut self, game_path: &str) -> Result<DecodedTexture, String> {
        self.parse_pending();
        let pkg = find_package(&self.packages, game_path).ok_or_else(|| format!("{} is not loaded", game_path))?;
        read_texture(pkg).map_err(|e| format!("{}: {}", game_path, e))
    }

    pub fn animation_list(&mut self) -> Vec<AnimEntry> {
        self.parse_pending();
        self.packages
            .iter()
            .filter_map(|(path, pkg)| {
                let info = read_anim_info(pkg).ok()?;
                let (has_bone_tracks, curves) = read_anim_contents(pkg).unwrap_or_default();
                Some(AnimEntry { path: path.clone(), has_bone_tracks, curves, info })
            })
            .collect()
    }

    fn skeleton_bone_names(&self, skeleton: &str) -> Result<Vec<String>, String> {
        if let Some(pkg) = find_package(&self.packages, skeleton) {
            return read_skeleton_bones(pkg);
        }
        let wanted = skeleton.rsplit('/').next().unwrap_or(skeleton);
        self.meshes
            .values()
            .find(|m| m.skeleton.as_deref().and_then(|s| s.rsplit(['/', '.']).next()) == Some(wanted))
            .map(|m| m.bones.iter().map(|b| b.name.clone()).collect())
            .ok_or_else(|| format!("skeleton {} is not loaded", wanted))
    }

    pub fn sample_animation(&mut self, path: &str) -> Result<SampledAnimation, String> {
        self.parse_pending();
        let pkg = self.packages.get(path).ok_or_else(|| format!("package {} not loaded", path))?;
        let info = read_anim_info(pkg)?;
        let skeleton = info.skeleton.as_deref().ok_or("animation has no skeleton")?;
        let bones = self.skeleton_bone_names(skeleton)?;
        let pkg = &self.packages[path];
        read_animation(pkg, &bones).map_err(|e| format!("{}: {}", path, e))
    }

    pub fn load_mesh(&mut self, path: &str) -> Result<&MeshData, String> {
        if !self.meshes.contains_key(path) {
            let pkg = self.packages.get(path).ok_or_else(|| format!("package {} not loaded", path))?;
            let export = pkg
                .exports
                .iter()
                .find(|e| matches!(pkg.export_class_name(e), Some("SkeletalMesh" | "StaticMesh")))
                .ok_or_else(|| format!("{} has no SkeletalMesh or StaticMesh export", path))?;
            if pkg.export_class_name(export) == Some("StaticMesh") {
                let mesh = read_static_mesh(pkg, export).map_err(|e| format!("{}: {}", path, e))?;
                self.meshes.insert(path.to_string(), mesh);
                return Ok(&self.meshes[path]);
            }
            let mut candidates = Profile::candidates(pkg.is_ue5);
            if let Some((4, minor, _)) = pkg.engine_version {
                candidates.sort_by_key(|p| p.minor != minor as u32);
            }
            if let Some(last) = self.last_profile {
                candidates.retain(|p| *p != last);
                candidates.insert(0, last);
            }
            let mut first_error = None;
            let mut result = None;
            for profile in candidates {
                match read_skeletal_mesh(pkg, export, profile) {
                    Ok(mesh) => {
                        result = Some(mesh);
                        break;
                    }
                    Err(e) => {
                        first_error.get_or_insert(format!("{}: {}", profile.label(), e));
                    }
                }
            }
            let mut mesh = result.ok_or_else(|| {
                format!("could not read {} with any supported engine version ({})", path, first_error.unwrap_or_default())
            })?;
            mesh.sockets = read_sockets(pkg);
            mesh.morphs = read_morph_targets(pkg, mesh.lod_index, mesh.positions.len() / 3);
            if let Some(physics_pkg) = mesh.physics_asset.as_deref().and_then(|p| find_package(&self.packages, p)) {
                mesh.physics_bones = read_physics_bodies(physics_pkg);
            }
            if let Some(skeleton_pkg) = mesh.skeleton.as_deref().and_then(|s| find_package(&self.packages, s)) {
                mesh.sockets.extend(read_sockets(skeleton_pkg));
            }
            self.last_profile = mesh.profile;
            self.meshes.insert(path.to_string(), mesh);
        }
        Ok(&self.meshes[path])
    }
}
