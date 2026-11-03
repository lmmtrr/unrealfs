use std::path::Path;

fn walk(dir: &Path, root: &Path, env: &mut unrealfs::UeEnvironment) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, root, env);
        } else if unrealfs::is_unreal_file(&p.to_string_lossy()) {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            env.add_file(&rel, std::fs::read(&p).unwrap());
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = Path::new(&args[1]);
    let root = Path::new(&args[2]);
    let t = std::time::Instant::now();
    let mut env = unrealfs::UeEnvironment::default();
    walk(root, root, &mut env);
    match env.extract_spine(out) {
        Ok(files) => {
            for f in &files {
                println!("{} atlas={:?} textures={}", f.skeleton.display(), f.atlas.as_ref().map(|p| p.display().to_string()), f.textures.len());
            }
        }
        Err(e) => println!("error: {}", e),
    }
    println!("errors: {:?}  ({} ms)", env.errors(), t.elapsed().as_millis());
}
