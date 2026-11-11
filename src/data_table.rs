use crate::package::Package;
use crate::properties::{PropValue, Properties, read_object_header, read_tagged_properties};
use crate::reader::{Reader, Result};
use crate::unversioned::read_unversioned_properties;

pub fn read_data_table(pkg: &Package) -> Option<Result<Vec<(String, Properties)>>> {
    let export = pkg.exports.iter().find(|e| pkg.export_class_name(e) == Some("DataTable"))?;
    Some((|| {
        let data = pkg.export_data(export)?;
        let mut r = Reader::new(data);
        let props = read_object_header(pkg, export, &mut r)?;
        let row_struct = match props.get("RowStruct") {
            Some(PropValue::Object(i)) => pkg.object_name(*i),
            _ => None,
        };
        let count = r.count(8)?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let name = pkg.read_name(&mut r)?;
            let row = match pkg.mappings.as_deref().filter(|_| pkg.unversioned_properties) {
                Some(mappings) => {
                    let row_struct = row_struct.ok_or("the DataTable has no row struct")?;
                    read_unversioned_properties(pkg, mappings, &mut r, row_struct)?
                }
                None => read_tagged_properties(pkg, &mut r)?,
            };
            rows.push((name, row));
        }
        Ok(rows)
    })())
}
