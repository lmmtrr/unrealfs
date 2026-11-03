# unrealfs

A pure Rust library for parsing and reading assets from Unreal Engine packages (`.uasset` / `.uexp` / `.ubulk`), with optional `.usmap` mappings for unversioned properties.

Supports static meshes, skeletal meshes (bones, sockets, morph targets, physics bodies), materials, textures, animations (including ACL-compressed tracks) and spine-ue4 assets (extracted back to `.atlas` / `.json`·`.skel` / `.png`).

## Usage

Add `unrealfs` to your `Cargo.toml`:

```toml
[dependencies]
unrealfs = "0.1"
```

Then feed package files into a `UeEnvironment` and read assets from it:

```rust
use unrealfs::{is_unreal_file, UeEnvironment};

let mut env = UeEnvironment::default();
for path in ["SK_Mannequin.uasset", "SK_Mannequin.uexp", "SK_Mannequin.ubulk"] {
    if is_unreal_file(path) {
        env.add_file(path, std::fs::read(path).unwrap());
    }
}

for entry in env.meshes() {
    let mesh = env.load_mesh(&entry.path).unwrap();
    println!("{}: {} bones", entry.name, mesh.bones.len());
}
```

## Acknowledgement

- [CUE4Parse](https://github.com/FabianFG/CUE4Parse)

## License

[MIT License](https://github.com/lmmtrr/unrealfs/blob/main/LICENSE)
