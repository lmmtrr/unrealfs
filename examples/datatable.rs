use unrealfs::properties::PropValue;

fn show(v: &PropValue) -> String {
    match v {
        PropValue::Name(n) => n.clone(),
        PropValue::Array(a) => format!("[{}]", a.iter().map(show).collect::<Vec<_>>().join(", ")),
        PropValue::Struct(s) => {
            let mut k: Vec<_> = s.iter().collect();
            k.sort_by(|a, b| a.0.cmp(b.0));
            format!("{{{}}}", k.iter().map(|(k, v)| format!("{}: {}", k, show(v))).collect::<Vec<_>>().join(", "))
        }
        other => format!("{:?}", other),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::Path::new(&args[1]);
    let uasset = std::fs::read(path).unwrap();
    let uexp = std::fs::read(path.with_extension("uexp")).unwrap();
    let pkg = unrealfs::package::Package::parse(&path.to_string_lossy(), &uasset, uexp, None).unwrap();
    eprintln!("ue5={} ver={} engine={:?}", pkg.is_ue5, pkg.ue5_version, pkg.engine_version);
    let rows = unrealfs::data_table::read_data_table(&pkg).expect("no DataTable").unwrap();
    eprintln!("{} rows", rows.len());
    for (name, props) in rows.iter().filter(|(n, _)| args.get(2).is_none_or(|f| n.contains(f.as_str()))) {
        println!("== {}", name);
        let mut k: Vec<_> = props.iter().collect();
        k.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in k {
            println!("  {} = {}", k, show(v));
        }
    }
}
