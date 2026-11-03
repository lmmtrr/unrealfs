use crate::package::Package;
use crate::properties::{Properties, read_object_header, read_tagged_properties};
use crate::reader::{Reader, Result};

pub fn read_data_table(pkg: &Package) -> Option<Result<Vec<(String, Properties)>>> {
    let export = pkg.exports.iter().find(|e| pkg.export_class_name(e) == Some("DataTable"))?;
    Some((|| {
        if pkg.unversioned_properties {
            return Err("unversioned DataTable rows are not supported".to_string());
        }
        let data = pkg.export_data(export)?;
        let mut r = Reader::new(data);
        read_object_header(pkg, export, &mut r)?;
        let count = r.count(8)?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let name = pkg.read_name(&mut r)?;
            rows.push((name, read_tagged_properties(pkg, &mut r)?));
        }
        Ok(rows)
    })())
}
