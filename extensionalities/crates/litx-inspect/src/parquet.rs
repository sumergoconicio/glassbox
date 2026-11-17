//! Bounded Parquet metadata with optional native row previews. External paths are never opened.
use crate::{InspectOptions, InspectReport, xml};
use parquet_format_safe::{FileMetaData, SchemaElement, thrift::protocol::TCompactInputProtocol};
use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct ParquetProfile {
    pub basis: &'static str,
    pub rows: u64,
    pub row_groups: usize,
    pub row_group_rows: Vec<u64>,
    pub leaf_columns: usize,
    pub columns: Vec<ParquetColumn>,
    pub created_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ParquetColumn {
    pub path: String,
    pub path_segments: Vec<String>,
    pub physical_type: &'static str,
    pub logical_type: Option<String>,
}

fn schema(
    nodes: &[SchemaElement],
    index: &mut usize,
    prefix: &[String],
    depth: usize,
    columns: &mut Vec<ParquetColumn>,
) -> xml::Result<()> {
    if depth > 64 {
        return Err("Parquet schema depth cap exceeded".into());
    }
    let node = nodes.get(*index).ok_or("truncated Parquet schema tree")?;
    *index += 1;
    let mut path = prefix.to_vec();
    if depth > 0 {
        path.push(node.name.clone());
    }
    match (node.type_, node.num_children) {
        (Some(physical), None | Some(0)) if depth > 0 => {
            let name = match physical.0 {
                0 => "BOOLEAN",
                1 => "INT32",
                2 => "INT64",
                3 => "INT96",
                4 => "FLOAT",
                5 => "DOUBLE",
                6 => "BYTE_ARRAY",
                7 => "FIXED_LEN_BYTE_ARRAY",
                _ => return Err("unknown Parquet physical type".into()),
            };
            columns.push(ParquetColumn {
                path: path.join("."),
                path_segments: path,
                physical_type: name,
                logical_type: node.logical_type.as_ref().map(|t| format!("{t:?}")),
            });
        }
        (None, Some(children)) if children >= 0 && children as usize <= nodes.len() => {
            for _ in 0..children {
                schema(nodes, index, &path, depth + 1, columns)?;
            }
        }
        _ => return Err("invalid Parquet schema node".into()),
    }
    Ok(())
}

pub(crate) fn inspect(path: &Path, report: &mut InspectReport, options: &InspectOptions) {
    let result = (|| -> xml::Result<()> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if length < 12 {
            return Err("truncated Parquet file".into());
        }
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        if &magic != b"PAR1" {
            return Err("Parquet header missing or encrypted".into());
        }
        file.seek(SeekFrom::End(-8))?;
        let mut trailer = [0; 8];
        file.read_exact(&mut trailer)?;
        if &trailer[4..] != b"PAR1" {
            return Err("Parquet footer missing or encrypted".into());
        }
        let size = u32::from_le_bytes(trailer[..4].try_into()?) as usize;
        if size == 0 || size > 1 << 20 || size as u64 > length - 12 {
            return Err("Parquet footer length invalid or exceeds 1 MiB cap".into());
        }
        file.seek(SeekFrom::Start(length - 8 - size as u64))?;
        let mut bytes = vec![0; size];
        file.read_exact(&mut bytes)?;
        // The safe protocol checks lengths before allocating strings/lists.
        let mut cursor = std::io::Cursor::new(bytes);
        let metadata = FileMetaData::read_from_in_protocol(&mut TCompactInputProtocol::new(
            &mut cursor,
            1 << 20,
        ))?;
        if cursor.position() != size as u64 {
            return Err("trailing Parquet footer metadata".into());
        }
        if metadata.schema.len() > 16_384
            || metadata.row_groups.len() > 16_384
            || metadata.num_rows < 0
        {
            return Err("Parquet schema/row-group cap exceeded or negative row count".into());
        }
        let mut columns = Vec::new();
        let mut index = 0;
        schema(&metadata.schema, &mut index, &[], 0, &mut columns)?;
        if index != metadata.schema.len() {
            return Err("unreachable Parquet schema nodes".into());
        }
        let mut total = 0u64;
        let mut row_group_rows = Vec::new();
        for group in &metadata.row_groups {
            if group.num_rows < 0 || group.columns.len() != columns.len() {
                return Err("invalid Parquet row group".into());
            }
            total = total
                .checked_add(group.num_rows as u64)
                .ok_or("Parquet row count overflow")?;
            row_group_rows.push(group.num_rows as u64);
        }
        if total != metadata.num_rows as u64 {
            return Err("Parquet row-group row counts disagree".into());
        }
        let profile = ParquetProfile {
            basis: "footer_metadata",
            rows: total,
            row_groups: metadata.row_groups.len(),
            row_group_rows,
            leaf_columns: columns.len(),
            columns,
            created_by: metadata.created_by.clone(),
        };
        let mut markdown = format!(
            "# Parquet schema\n\n- Rows: {}\n- Row groups: {}\n- Leaf columns: {}\n\n| Column | Physical type | Logical type |\n| --- | --- | --- |\n",
            profile.rows, profile.row_groups, profile.leaf_columns
        );
        for column in &profile.columns {
            markdown.push_str(&format!(
                "| {} | {} | {} |\n",
                crate::markdown::escape(&column.path).replace('|', "\\|"),
                column.physical_type,
                crate::markdown::escape(column.logical_type.as_deref().unwrap_or(""))
                    .replace('|', "\\|")
            ));
            if markdown.len() > options.max_chars.saturating_mul(4) {
                break;
            }
        }
        crate::markdown::append(report, &markdown, options, "native_structure");
        report.parquet = Some(profile);
        #[cfg(all(feature = "parquet-preview", not(target_arch = "wasm32")))]
        {
            let mut footer = cursor.into_inner();
            footer.extend_from_slice(&trailer);
            match crate::parquet_rows::preview(&mut file, length, footer, &metadata) {
                Ok(preview) => {
                    crate::tabular::render(&preview, report, options);
                    report.dataset = Some(preview);
                }
                Err(error) => report.error("parquet_preview", error),
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        report.error("parquet", error);
    }
}
