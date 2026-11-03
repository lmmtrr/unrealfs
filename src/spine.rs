use std::path::{Path, PathBuf};
use crate::package::Package;
use crate::properties::{PropValue, Properties, read_object_header};
use crate::reader::{Reader, Result};

pub const ATLAS_CLASS: &str = "SpineAtlasAsset";
pub const SKELETON_CLASS: &str = "SpineSkeletonDataAsset";

pub struct SpineAtlas {
    pub name: String,
    pub file_name: Option<String>,
    pub text: String,
    pub pages: Vec<String>,
}

pub struct SpineSkeleton {
    pub name: String,
    pub file_name: Option<String>,
    pub data: Vec<u8>,
}

impl SpineSkeleton {
    pub fn is_binary(&self) -> bool {
        match self.file_name.as_deref().and_then(extension) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => false,
            Some(ext) if ext.eq_ignore_ascii_case("skel") => true,
            _ => {
                let body = self.data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&self.data);
                body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{')
            }
        }
    }
}

fn extension(path: &str) -> Option<&str> {
    let file = file_stem_and_ext(path);
    file.rfind('.').map(|i| &file[i + 1..])
}

fn file_stem_and_ext(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn file_stem(path: &str) -> &str {
    let file = file_stem_and_ext(path);
    file.rfind('.').map_or(file, |i| &file[..i])
}

fn export_props(pkg: &Package, class: &str) -> Vec<(String, Result<Properties>)> {
    pkg.exports
        .iter()
        .filter(|e| e.outer_index == 0 && pkg.export_class_name(e) == Some(class))
        .map(|e| {
            let props = pkg.export_data(e).and_then(|data| read_object_header(pkg, e, &mut Reader::new(data)));
            (e.object_name.clone(), props)
        })
        .collect()
}

fn prop_string(props: &Properties, name: &str) -> Option<String> {
    match props.get(name) {
        Some(PropValue::Name(s)) if !s.is_empty() && s != "None" => Some(s.clone()),
        _ => None,
    }
}

pub fn read_spine_atlases(pkg: &Package) -> Vec<Result<SpineAtlas>> {
    export_props(pkg, ATLAS_CLASS)
        .into_iter()
        .map(|(name, props)| {
            let props = props.map_err(|e| format!("{}: {}", name, e))?;
            let text = prop_string(&props, "rawData").ok_or_else(|| format!("{}: atlas has no rawData", name))?;
            let pages = match props.get("atlasPages") {
                Some(PropValue::Array(items)) => items
                    .iter()
                    .map(|v| match v {
                        PropValue::Object(i) => pkg.import_path(*i).or_else(|| pkg.object_name(*i).map(str::to_string)).unwrap_or_default(),
                        _ => String::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Ok(SpineAtlas { name, file_name: prop_string(&props, "atlasFileName"), text, pages })
        })
        .collect()
}

pub fn read_spine_skeletons(pkg: &Package) -> Vec<Result<SpineSkeleton>> {
    export_props(pkg, SKELETON_CLASS)
        .into_iter()
        .map(|(name, props)| {
            let props = props.map_err(|e| format!("{}: {}", name, e))?;
            let data: Vec<u8> = match props.get("rawData") {
                Some(PropValue::Array(items)) => items
                    .iter()
                    .map(|v| match v {
                        PropValue::Int(b) => Ok(*b as u8),
                        _ => Err(format!("{}: rawData is not a byte array", name)),
                    })
                    .collect::<Result<_>>()?,
                _ => return Err(format!("{}: skeleton has no rawData", name)),
            };
            Ok(SpineSkeleton { name, file_name: prop_string(&props, "skeletonDataFileName"), data })
        })
        .collect()
}

pub fn atlas_page_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut expect_page = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            expect_page = true;
        } else if expect_page {
            names.push(line.to_string());
            expect_page = false;
        }
    }
    names
}

fn base_key(name: &str) -> String {
    let mut s = file_stem(name).to_ascii_lowercase();
    for suffix in ["data", "skeleton", "skel", "atlas", "json"] {
        for sep in ['-', '_', '.'] {
            if let Some(rest) = s.strip_suffix(&format!("{}{}", sep, suffix)) {
                s = rest.to_string();
            }
        }
    }
    s
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

pub struct SpineOutput {
    pub package: String,
    pub file_name: String,
    pub data: Vec<u8>,
}

fn safe_file_name(name: &str) -> String {
    file_stem_and_ext(name)
        .chars()
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect()
}

impl crate::UeEnvironment {
    pub fn spine_outputs(&mut self) -> std::result::Result<Vec<SpineOutput>, String> {
        self.parse_pending();
        let mut atlases = Vec::new();
        let mut skeletons = Vec::new();
        let mut errors = Vec::new();
        for (key, pkg) in &self.packages {
            for a in read_spine_atlases(pkg) {
                match a {
                    Ok(a) => atlases.push((key.clone(), a)),
                    Err(e) => errors.push(format!("{}: {}", key, e)),
                }
            }
            for s in read_spine_skeletons(pkg) {
                match s {
                    Ok(s) => skeletons.push((key.clone(), s)),
                    Err(e) => errors.push(format!("{}: {}", key, e)),
                }
            }
        }
        if skeletons.is_empty() {
            self.errors.extend(errors);
            return Err("no Spine skeleton (SpineSkeletonDataAsset) found".to_string());
        }
        let mut used_names = std::collections::HashSet::new();
        let mut used_pages = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (key, skel) in &skeletons {
            let skel_key = base_key(skel.file_name.as_deref().unwrap_or(&skel.name));
            let atlas = atlases
                .iter()
                .find(|(k, _)| k == key)
                .or_else(|| if atlases.len() == 1 { atlases.first() } else { None })
                .or_else(|| {
                    atlases.iter().max_by_key(|(_, a)| {
                        let k = base_key(a.file_name.as_deref().unwrap_or(&a.name));
                        (k == skel_key, common_prefix(&k, &skel_key))
                    })
                });
            let mut base = safe_file_name(file_stem(skel.file_name.as_deref().unwrap_or(&skel.name)));
            if base.is_empty() {
                base = "skeleton".to_string();
            }
            let mut n = 2;
            let first = base.clone();
            while !used_names.insert(base.to_ascii_lowercase()) {
                base = format!("{}_{}", first, n);
                n += 1;
            }
            let ext = if skel.is_binary() { "skel" } else { "json" };
            out.push(SpineOutput { package: key.clone(), file_name: format!("{}.{}", base, ext), data: skel.data.clone() });
            let Some((atlas_key, atlas)) = atlas else {
                errors.push(format!("{}: no atlas (SpineAtlasAsset) found", skel.name));
                continue;
            };
            out.push(SpineOutput { package: atlas_key.clone(), file_name: format!("{}.atlas", base), data: atlas.text.as_bytes().to_vec() });
            for (page, texture) in atlas_page_names(&atlas.text).iter().zip(&atlas.pages) {
                let file_name = safe_file_name(page);
                let found = crate::find_package(&self.packages, texture)
                    .and_then(|pkg| self.packages.iter().find(|(_, p)| std::ptr::eq(*p, pkg)))
                    .ok_or_else(|| format!("{} is not loaded", texture));
                let result = found.and_then(|(texture_key, pkg)| {
                    if !used_pages.insert((texture_key.clone(), file_name.to_ascii_lowercase())) {
                        return Ok(None);
                    }
                    let t = crate::texture::read_texture(pkg)?;
                    Ok(Some((texture_key.clone(), encode_png(&t.rgba, t.width as u32, t.height as u32)?)))
                });
                match result {
                    Ok(Some((texture_key, data))) => out.push(SpineOutput { package: texture_key, file_name, data }),
                    Ok(None) => {}
                    Err(e) => errors.push(format!("{}: page {}: {}", atlas.name, page, e)),
                }
            }
        }
        self.errors.extend(errors);
        Ok(out)
    }

    pub fn extract_spine(&mut self, out_dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
        let outputs = self.spine_outputs()?;
        std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
        let mut written = Vec::new();
        for o in outputs {
            let path = out_dir.join(&o.file_name);
            if written.contains(&path) {
                continue;
            }
            std::fs::write(&path, &o.data).map_err(|e| e.to_string())?;
            written.push(path);
        }
        Ok(written)
    }
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> std::result::Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut encoder = png::Encoder::new(&mut data, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_names() {
        let text = "\nsp_boss003.png\nsize: 2048,2048\nformat: RGBA8888\nbianzi_L\n  rotate: true\n\nsp_boss003_2.png\nsize: 1024,1024\nhead\n  xy: 0, 0\n";
        assert_eq!(atlas_page_names(text), vec!["sp_boss003.png", "sp_boss003_2.png"]);
    }

    #[test]
    fn base_keys() {
        assert_eq!(base_key("sp_boss003-data"), "sp_boss003");
        assert_eq!(base_key("sp_boss003-atlas"), "sp_boss003");
        assert_eq!(base_key("F:/spine/boss003/sp_boss003.atlas"), "sp_boss003");
    }
}
